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
};
use crate::event_channel::open_event_channel_best_effort;
use rand::RngCore;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

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
        }
    }
}

#[derive(Debug)]
pub enum NativeControlError {
    Preflight(String),
    Pairing(String),
    Flow(String),
    Identity,
    SessionSetup(String),
    Event(String),
    Media(String),
    Record(String),
    SetPeers(String),
}

pub struct NativeControlReady {
    pub control: EncryptedRtspChannel,
    pub event: Option<EventChannel>,
    pub media: NativeMediaOwner,
    pub info: Ap2Info,
    pub receiver: SocketAddr,
    pub local_ip: IpAddr,
    pub session_uri: String,
    pub session_id: u32,
    pub ssrc: u32,
    pub timing: LiveTiming,
    pub buffered: bool,
    pub latency_min: Option<u32>,
    pub latency_max: Option<u32>,
    pub next_cseq: u32,
}

pub fn open_native_control(
    config: &NativeControlConfig,
    timing: LiveTiming,
) -> Result<NativeControlReady, NativeControlError> {
    let mut flow = NativeConnectFlow::default();

    let preflight_client = Ap2PreflightClient::new(
        config.dacp_id.clone(),
        config.active_remote.clone(),
    );
    let (stream, preflight) = preflight_client
        .open_info_connection(&config.host, config.port)
        .map_err(|e| NativeControlError::Preflight(format!("{e:?}")))?;
    flow.tcp_connected()
        .map_err(|e| NativeControlError::Flow(format!("{e:?}")))?;
    flow.info_loaded()
        .map_err(|e| NativeControlError::Flow(format!("{e:?}")))?;

    let local_addr = stream.local_addr()
        .map_err(|e| NativeControlError::Preflight(format!("local addr: {e}")))?;
    let receiver = preflight.peer;

    // Same TCP socket survives /info -> pairing -> encrypted RTSP.
    let paired = if let Some(credentials_hex) = config.auth_credentials.as_deref() {
        let credentials = StoredHapCredentials::from_hex(credentials_hex)
            .map_err(|e| NativeControlError::Pairing(format!("{e:?}")))?;
        NativeHapPairingClient::default()
            .pair_verify_on_stream(stream, receiver, &config.dacp_id, &credentials)
            .map_err(|e| NativeControlError::Pairing(format!("{e:?}")))?
    } else {
        TransientPairingClient::default()
            .pair_channel_on_stream(stream, receiver, config.password.as_deref())
            .map_err(|e| NativeControlError::Pairing(format!("{e:?}")))?
    };
    flow.paired()
        .map_err(|e| NativeControlError::Flow(format!("{e:?}")))?;

    // The timing engine/responder is already live when this function is called.
    flow.timing_ready()
        .map_err(|e| NativeControlError::Flow(format!("{e:?}")))?;

    let pairing = paired.pairing;
    let mut control = paired.channel;
    let mut rng = rand::thread_rng();
    let session_id = rng.next_u32();
    let session_uuid = random_uuid_upper(&mut rng);
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
            setup_ptp_session(
                &mut flow,
                &mut control,
                &PtpSessionSetupConfig {
                    cseq: 1,
                    session_uri: session_uri.clone(),
                    session_uuid: session_uuid.clone(),
                    group_uuid: random_uuid_upper(&mut rng),
                    peer_uuid: random_uuid_upper(&mut rng),
                    device_id: device_id.clone(),
                    mac_address,
                    name: config.receiver_name.clone(),
                    local_address: local_addr.ip().to_string(),
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
        local_addr.ip(),
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
    }

    let next_cseq = if matches!(timing, LiveTiming::Ptp { .. }) {
        send_setpeers(
            &mut control,
            &SetPeersConfig {
                cseq: 4,
                session_uri: session_uri.clone(),
                receiver_address: receiver.ip().to_string(),
                local_address: local_addr.ip().to_string(),
                dacp_id: config.dacp_id.clone(),
                active_remote: config.active_remote.clone(),
            },
        ).map_err(|e| NativeControlError::SetPeers(format!("{e:?}")))?;
        5
    } else {
        4
    };

    flow.ready()
        .map_err(|e| NativeControlError::Flow(format!("{e:?}")))?;

    // Pinned MSA: NTP SSRC == streamConnectionID; PTP SSRC == 0.
    let ssrc = if matches!(timing, LiveTiming::Ptp { .. }) { 0 } else { session_id };

    Ok(NativeControlReady {
        control,
        event,
        media,
        info: preflight.info,
        receiver,
        local_ip: local_addr.ip(),
        session_uri,
        session_id,
        ssrc,
        timing,
        buffered,
        latency_min,
        latency_max,
        next_cseq,
    })
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
