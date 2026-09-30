//! Concrete native AP2 SOLO control-path owner.
//! Pinned ordering: GET /info -> HAP on same TCP -> live timing -> session SETUP
//! -> best-effort event TCP -> media sockets -> RECORD -> stream SETUP -> SETPEERS(PTP).

use crate::{
    send_record, send_setpeers, setup_buffered_stream, setup_ntp_session,
    setup_ptp_session, setup_realtime_stream, Ap2AudioFormat, Ap2Info,
    Ap2PreflightClient, EncryptedRtspChannel, EventChannel, NativeConnectFlow,
    NativeHapPairingClient, NativeMediaOwner, NtpSessionSetupConfig,
    PtpSessionSetupConfig, RecordConfig, SetPeersConfig, StoredHapCredentials,
    TransientPairingClient, BufferedStreamSetupConfig, RealtimeStreamSetupConfig,
    NativeTimingOwner,
};
use crate::event_channel::open_event_channel_best_effort;
use crate::feedback::{SharedCseq, SharedRtspControl};
use rand::RngCore;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use std::sync::{Arc, Mutex, atomic::AtomicU32};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveTiming {
    Ntp { timing_port: u16 },
    Ptp { master_clock_id: u64 },
}

#[derive(Debug, Clone)]
pub struct NativeControlConfig {
    pub host: String,
    pub port: u16,
    pub password: Option<String>,
    pub auth_credentials: Option<String>,
    pub dacp_id: String,
    pub active_remote: String,
    pub receiver_name: String,
    pub audio_format: Ap2AudioFormat,
    pub buffered_requested: bool,
    pub prefer_ptp: bool,
    pub follow_receiver_clock: bool,
    pub bind_ip: Option<IpAddr>,
    pub publish_ip: Option<IpAddr>,
}

impl NativeControlConfig {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            password: None,
            auth_credentials: None,
            dacp_id: "A1B2C3D4E5F60708".into(),
            active_remote: "123456789".into(),
            receiver_name: "SAirplay2".into(),
            audio_format: Ap2AudioFormat::ALAC_44100_16_STEREO,
            buffered_requested: false,
            prefer_ptp: false,
            follow_receiver_clock: false,
            bind_ip: None,
            publish_ip: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeControlErrorClass {
    Generic,
    AuthRequired,
    AuthFailed,
}

#[derive(Debug)]
pub enum NativeControlError {
    Preflight(String),
    Pairing(String),
    AuthRequired { http_status: u16, detail: String },
    AuthFailed { http_status: u16, detail: String },
    Flow(String),
    Identity,
    SessionSetup(String),
    Event(String),
    Media(String),
    Record(String),
    SetPeers(String),
}

impl NativeControlError {
    pub fn class(&self) -> NativeControlErrorClass {
        match self {
            Self::AuthRequired { .. } => NativeControlErrorClass::AuthRequired,
            Self::AuthFailed { .. } => NativeControlErrorClass::AuthFailed,
            _ => NativeControlErrorClass::Generic,
        }
    }

    pub fn http_status(&self) -> u16 {
        match self {
            Self::AuthRequired { http_status, .. } | Self::AuthFailed { http_status, .. } => *http_status,
            _ => 0,
        }
    }

    pub fn detail(&self) -> String {
        match self {
            Self::Preflight(v) | Self::Pairing(v) | Self::Flow(v)
            | Self::SessionSetup(v) | Self::Event(v) | Self::Media(v)
            | Self::Record(v) | Self::SetPeers(v) => v.clone(),
            Self::AuthRequired { detail, .. } | Self::AuthFailed { detail, .. } => detail.clone(),
            Self::Identity => "invalid sender identity".into(),
        }
    }
}

pub struct NativeControlReady {
    pub control: SharedRtspControl,
    pub event: Option<EventChannel>,
    pub media: NativeMediaOwner,
    pub info: Ap2Info,
    pub receiver: SocketAddr,
    pub local_ip: IpAddr,
    pub publish_ip: IpAddr,
    pub session_uri: String,
    pub session_uuid: String,
    pub group_uuid: Option<String>,
    pub pair_verified: bool,
    pub hap_shared_secret: [u8; 32],
    pub session_id: u32,
    pub ssrc: u32,
    pub timing: LiveTiming,
    pub timing_owner: NativeTimingOwner,
    pub buffered: bool,
    pub latency_min: Option<u32>,
    pub latency_max: Option<u32>,
    pub arrival_to_render_latency_ms: Option<u32>,
    pub next_cseq: SharedCseq,
}

pub fn open_native_control(
    config: &NativeControlConfig,
) -> Result<NativeControlReady, NativeControlError> {
    let mut flow = NativeConnectFlow::default();

    let preflight_client = Ap2PreflightClient::new(
        config.dacp_id.clone(),
        config.active_remote.clone(),
    ).with_bind_ip(config.bind_ip);
    let (stream, mut preflight) = preflight_client
        .open_info_connection(&config.host, config.port)
        .map_err(|e| NativeControlError::Preflight(format!("{e:?}")))?;
    flow.tcp_connected()
        .map_err(|e| NativeControlError::Flow(format!("{e:?}")))?;
    flow.info_loaded()
        .map_err(|e| NativeControlError::Flow(format!("{e:?}")))?;

    let mut local_addr = stream.local_addr()
        .map_err(|e| NativeControlError::Preflight(format!("local addr: {e}")))?;
    let mut receiver = preflight.peer;
    let credentials = match config.auth_credentials.as_deref() {
        Some(raw) => Some(StoredHapCredentials::from_hex(raw)
            .map_err(|e| NativeControlError::Pairing(format!("{e:?}")))?),
        None => None,
    };

    // Pinned MSA password ladder:
    //  1) transient pair-setup using the device password,
    //  2) if the receiver rejects that leg and stored credentials exist,
    //     reopen TCP, repeat GET /info, then pair-verify.
    // A transport death during leg 1 is terminal; nothing was rejected.
    let (paired, pair_verified) = if let Some(password) = config.password.as_deref().filter(|v| !v.is_empty()) {
        match TransientPairingClient::default()
            .pair_channel_on_stream(stream, receiver, Some(password))
        {
            Ok(session) => (session, false),
            Err(error) if !pairing_error_is_transport(&error) && credentials.is_some() => {
                let (retry_stream, retry_preflight) = preflight_client
                    .open_info_connection(&config.host, config.port)
                    .map_err(|e| NativeControlError::Preflight(format!("pair-verify retry /info: {e:?}")))?;
                local_addr = retry_stream.local_addr()
                    .map_err(|e| NativeControlError::Preflight(format!("retry local addr: {e}")))?;
                receiver = retry_preflight.peer;
                preflight = retry_preflight;
                (
                    NativeHapPairingClient::default()
                        .pair_verify_on_stream(
                            retry_stream,
                            receiver,
                            &config.dacp_id,
                            credentials.as_ref().expect("checked"),
                        )
                        .map_err(|e| classify_pairing_error(e, true))?,
                    true,
                )
            }
            Err(error) => return Err(classify_pairing_error(error, true)),
        }
    } else if let Some(credentials) = credentials.as_ref() {
        (
            NativeHapPairingClient::default()
                .pair_verify_on_stream(stream, receiver, &config.dacp_id, credentials)
                .map_err(|e| classify_pairing_error(e, true))?,
            true,
        )
    } else {
        (
            TransientPairingClient::default()
                .pair_channel_on_stream(stream, receiver, None)
                .map_err(|e| classify_pairing_error(e, false))?,
            false,
        )
    };
    flow.paired()
        .map_err(|e| NativeControlError::Flow(format!("{e:?}")))?;

    // MSA requires timing to be live before encrypted session SETUP. PTP is
    // attempted first when requested and falls back to NTP on startup failure.
    let bind_ip = config.bind_ip.unwrap_or(local_addr.ip());
    let publish_ip = config.publish_ip.or(config.bind_ip).unwrap_or(local_addr.ip());

    let (timing_owner, timing) = NativeTimingOwner::start(
        receiver.ip(),
        bind_ip,
        &config.dacp_id,
        config.prefer_ptp,
        config.follow_receiver_clock,
    ).map_err(NativeControlError::SessionSetup)?;
    flow.timing_ready()
        .map_err(|e| NativeControlError::Flow(format!("{e:?}")))?;

    let pairing = paired.pairing;
    let mut control = paired.channel;
    let mut rng = rand::thread_rng();
    let session_id = rng.next_u32();
    let session_uuid = random_uuid_upper(&mut rng);
    let mut group_uuid_for_mrp: Option<String> = None;
    let session_uri = format_session_uri(local_addr.ip(), session_id);
    let device_id = dacp_device_id(&config.dacp_id).ok_or(NativeControlError::Identity)?;
    let buffered = config.buffered_requested && matches!(timing, LiveTiming::Ptp { .. });

    let event_port = match timing {
        LiveTiming::Ntp { timing_port } => {
            setup_ntp_session(
                &mut flow,
                &mut control,
                &NtpSessionSetupConfig {
                    cseq: 1,
                    session_uri: session_uri.clone(),
                    session_uuid: session_uuid.clone(),
                    device_id: Some(device_id.clone()),
                    timing_port,
                    dacp_id: config.dacp_id.clone(),
                    active_remote: config.active_remote.clone(),
                },
            )
            .map_err(|e| NativeControlError::SessionSetup(format!("{e:?}")))?
            .event_port
        }
        LiveTiming::Ptp { master_clock_id } => {
            let mac_address = dacp_mac_address(&config.dacp_id)
                .ok_or(NativeControlError::Identity)?;
            let group_uuid = random_uuid_upper(&mut rng);
            group_uuid_for_mrp = Some(group_uuid.clone());
            setup_ptp_session(
                &mut flow,
                &mut control,
                &PtpSessionSetupConfig {
                    cseq: 1,
                    session_uri: session_uri.clone(),
                    session_uuid: session_uuid.clone(),
                    group_uuid,
                    peer_uuid: random_uuid_upper(&mut rng),
                    device_id: device_id.clone(),
                    mac_address,
                    name: config.receiver_name.clone(),
                    local_address: publish_ip.to_string(),
                    clock_id: master_clock_id,
                    dacp_id: config.dacp_id.clone(),
                    active_remote: config.active_remote.clone(),
                },
            )
            .map_err(|e| NativeControlError::SessionSetup(format!("{e:?}")))?
            .event_port
        }
    };

    // MSA: eventPort and reverse event connection are best-effort.
    let event = open_event_channel_best_effort(
        &mut flow,
        receiver.ip(),
        event_port,
        &pairing.audio_secret,
        Duration::from_secs(3),
    ).map_err(|e| NativeControlError::Event(format!("{e:?}")))?;

    // MSA opens/binds RTP data+control sockets after session SETUP/event and
    // before RECORD/stream SETUP.
    let mut media = NativeMediaOwner::prepare(
        bind_ip,
        config.audio_format.sample_rate,
        config.audio_format.bit_depth,
        config.audio_format.channels,
        &pairing,
    ).map_err(|e| NativeControlError::Media(format!("{e:?}")))?;
    let local_ports = media.local_ports()
        .map_err(|e| NativeControlError::Media(format!("{e:?}")))?;

    send_record(
        &mut flow,
        &mut control,
        &RecordConfig {
            cseq: 2,
            session_uri: session_uri.clone(),
            dacp_id: config.dacp_id.clone(),
            active_remote: config.active_remote.clone(),
        },
    ).map_err(|e| NativeControlError::Record(format!("{e:?}")))?;

    let mut latency_min = None;
    let mut latency_max = None;
    let mut arrival_to_render_latency_ms = None;
    if buffered {
        let result = setup_buffered_stream(
            &mut flow,
            &mut control,
            &BufferedStreamSetupConfig {
                cseq: 3,
                session_uri: session_uri.clone(),
                dacp_id: config.dacp_id.clone(),
                active_remote: config.active_remote.clone(),
                local_control_port: local_ports.control,
                audio_secret: pairing.audio_secret,
                stream_connection_id: session_id,
                audio_format: config.audio_format,
            },
        ).map_err(|e| NativeControlError::Media(format!("{e:?}")))?;
        media.attach_buffered(receiver.ip(), result.data_port, result.control_port)
            .map_err(|e| NativeControlError::Media(format!("{e:?}")))?;
    } else {
        let result = setup_realtime_stream(
            &mut flow,
            &mut control,
            &RealtimeStreamSetupConfig {
                cseq: 3,
                session_uri: session_uri.clone(),
                dacp_id: config.dacp_id.clone(),
                active_remote: config.active_remote.clone(),
                local_data_port: local_ports.data,
                local_control_port: local_ports.control,
                audio_secret: pairing.audio_secret,
                stream_connection_id: session_id,
                audio_format: config.audio_format,
            },
        ).map_err(|e| NativeControlError::Media(format!("{e:?}")))?;
        media.attach_realtime(
            receiver.ip(),
            result.ports.data_port,
            result.ports.control_port,
        );
        latency_min = result.latency_min;
        latency_max = result.latency_max;
        arrival_to_render_latency_ms = result.arrival_to_render_latency_ms;
    }

    let next_cseq = if matches!(timing, LiveTiming::Ptp { .. }) {
        send_setpeers(
            &mut control,
            &SetPeersConfig {
                cseq: 4,
                session_uri: session_uri.clone(),
                receiver_address: receiver.ip().to_string(),
                local_address: publish_ip.to_string(),
                dacp_id: config.dacp_id.clone(),
                active_remote: config.active_remote.clone(),
            },
        ).map_err(|e| NativeControlError::SetPeers(format!("{e:?}")))?;
        timing_owner.set_session_peers(receiver.ip(), publish_ip);
        5
    } else {
        4
    };

    flow.ready()
        .map_err(|e| NativeControlError::Flow(format!("{e:?}")))?;

    // Pinned MSA: NTP SSRC == streamConnectionID; PTP SSRC == 0.
    let ssrc = if matches!(timing, LiveTiming::Ptp { .. }) { 0 } else { session_id };

    let control = Arc::new(Mutex::new(control));
    let next_cseq = Arc::new(AtomicU32::new(next_cseq));

    Ok(NativeControlReady {
        control,
        event,
        media,
        info: preflight.info,
        receiver,
        local_ip: local_addr.ip(),
        publish_ip,
        session_uri,
        session_uuid,
        group_uuid: group_uuid_for_mrp,
        // Pinned ap2_mrp_ready/ap2cl_mrp_channel_status gate on stored
        // auth_credentials being configured, not on which password ladder
        // leg happened to win this socket.
        pair_verified: config.auth_credentials.is_some(),
        hap_shared_secret: pairing.audio_secret,
        session_id,
        ssrc,
        timing,
        timing_owner,
        buffered,
        latency_min,
        latency_max,
        arrival_to_render_latency_ms,
        next_cseq,
    })
}

fn pairing_error_is_transport(error: &crate::PairingError) -> bool {
    matches!(
        error,
        crate::PairingError::Resolve
            | crate::PairingError::Connect(_)
            | crate::PairingError::Configure(_)
            | crate::PairingError::Write(_)
            | crate::PairingError::Read(_)
            | crate::PairingError::Timeout
            | crate::PairingError::Closed
    )
}

fn pairing_error_is_auth(error: &crate::PairingError) -> bool {
    matches!(
        error,
        crate::PairingError::Status(401 | 403)
            | crate::PairingError::TlvError(_)
            | crate::PairingError::InvalidServerProof
            | crate::PairingError::InvalidCredentials
            | crate::PairingError::InvalidSignature
    )
}

fn classify_pairing_error(error: crate::PairingError, presented_secret: bool) -> NativeControlError {
    let http_status = match error {
        crate::PairingError::Status(status) => status,
        _ => 0,
    };
    let detail = format!("{error:?}");
    if pairing_error_is_auth(&error) {
        if presented_secret {
            NativeControlError::AuthFailed { http_status, detail }
        } else {
            NativeControlError::AuthRequired { http_status, detail }
        }
    } else {
        NativeControlError::Pairing(detail)
    }
}

fn format_session_uri(local_ip: IpAddr, session_id: u32) -> String {
    match local_ip {
        IpAddr::V4(ip) => format!("rtsp://{ip}/{session_id}"),
        IpAddr::V6(ip) => format!("rtsp://[{ip}]/{session_id}"),
    }
}

fn compact_dacp(dacp_id: &str) -> Option<String> {
    let compact: String = dacp_id.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    (compact.len() == 16).then_some(compact)
}

fn dacp_device_id(dacp_id: &str) -> Option<String> {
    let compact = compact_dacp(dacp_id)?;
    Some((0..8)
        .map(|i| &compact[i * 2..i * 2 + 2])
        .collect::<Vec<_>>()
        .join(":")
        .to_ascii_uppercase())
}

fn dacp_mac_address(dacp_id: &str) -> Option<String> {
    let compact = compact_dacp(dacp_id)?;
    Some((0..6)
        .map(|i| &compact[i * 2..i * 2 + 2])
        .collect::<Vec<_>>()
        .join(":")
        .to_ascii_uppercase())
}

fn random_uuid_upper(rng: &mut impl RngCore) -> String {
    let mut b = [0u8; 16];
    rng.fill_bytes(&mut b);
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, SeedableRng};
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn session_uri_and_identity_match_source_shapes() {
        assert_eq!(
            format_session_uri(IpAddr::V4(Ipv4Addr::new(192,168,1,20)), 42),
            "rtsp://192.168.1.20/42"
        );
        assert_eq!(
            format_session_uri(IpAddr::V6(Ipv6Addr::LOCALHOST), 42),
            "rtsp://[::1]/42"
        );
        assert_eq!(
            dacp_device_id("A1B2C3D4E5F60708").as_deref(),
            Some("A1:B2:C3:D4:E5:F6:07:08")
        );
        assert_eq!(
            dacp_mac_address("A1B2C3D4E5F60708").as_deref(),
            Some("A1:B2:C3:D4:E5:F6")
        );
    }

    #[test]
    fn uuid_shape_matches_native_source() {
        let mut rng = StdRng::seed_from_u64(7);
        let uuid = random_uuid_upper(&mut rng);
        assert_eq!(uuid.len(), 36);
        for i in [8,13,18,23] { assert_eq!(uuid.as_bytes()[i], b'-'); }
    }

    #[test]
    fn buffered_is_only_legal_on_ptp() {
        assert!(!matches!(LiveTiming::Ntp { timing_port: 1 }, LiveTiming::Ptp { .. }));
        assert!(matches!(LiveTiming::Ptp { master_clock_id: 1 }, LiveTiming::Ptp { .. }));
    }
}
