use crate::{
    EncryptedRtspChannel, EncryptedRtspError, NativeConnectError, NativeConnectFlow, RtspRequest,
};
use plist::{Dictionary, Value};
use std::io::Cursor;

#[derive(Debug, Clone)]
pub struct PtpSessionSetupConfig {
    pub cseq: u32,
    pub session_uri: String,
    pub session_uuid: String,
    pub group_uuid: String,
    pub peer_uuid: String,
    pub device_id: String,
    pub mac_address: String,
    pub name: String,
    pub local_address: String,
    pub clock_id: u64,
    pub dacp_id: String,
    pub active_remote: String,
}

#[derive(Debug)]
pub enum PtpSessionSetupError {
    Flow(NativeConnectError),
    Transport(EncryptedRtspError),
    Status(u16),
    Plist(plist::Error),
    InvalidRoot,
    MissingEventPort,
    InvalidEventPort,
}

impl From<NativeConnectError> for PtpSessionSetupError {
    fn from(value: NativeConnectError) -> Self { Self::Flow(value) }
}
impl From<EncryptedRtspError> for PtpSessionSetupError {
    fn from(value: EncryptedRtspError) -> Self { Self::Transport(value) }
}
impl From<plist::Error> for PtpSessionSetupError {
    fn from(value: plist::Error) -> Self { Self::Plist(value) }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtpSessionSetupResult {
    pub event_port: u16,
}

fn timing_peer(peer_uuid: &str, clock_id: u64, local_address: &str) -> Value {
    let mut peer = Dictionary::new();
    peer.insert("ID".into(), Value::String(peer_uuid.to_string()));
    peer.insert("DeviceType".into(), Value::Integer(0u64.into()));
    peer.insert("ClockID".into(), Value::Integer(clock_id.into()));
    peer.insert(
        "SupportsClockPortMatchingOverride".into(),
        Value::Boolean(false),
    );
    peer.insert(
        "Addresses".into(),
        Value::Array(vec![Value::String(local_address.to_string())]),
    );
    Value::Dictionary(peer)
}

pub fn build_ptp_session_plist(
    config: &PtpSessionSetupConfig,
) -> Result<Vec<u8>, PtpSessionSetupError> {
    let peer = timing_peer(&config.peer_uuid, config.clock_id, &config.local_address);

    let mut root = Dictionary::new();
    root.insert("timingProtocol".into(), Value::String("PTP".into()));
    root.insert("deviceID".into(), Value::String(config.device_id.clone()));
    root.insert("sessionUUID".into(), Value::String(config.session_uuid.clone()));
    root.insert("name".into(), Value::String(config.name.clone()));
    root.insert("macAddress".into(), Value::String(config.mac_address.clone()));
    root.insert("groupUUID".into(), Value::String(config.group_uuid.clone()));
    root.insert("groupContainsGroupLeader".into(), Value::Boolean(false));
    root.insert("timingPeerInfo".into(), peer.clone());
    root.insert("timingPeerList".into(), Value::Array(vec![peer]));

    let mut out = Vec::new();
    Value::Dictionary(root).to_writer_binary(&mut out)?;
    Ok(out)
}

fn parse_event_port(body: &[u8]) -> Result<u16, PtpSessionSetupError> {
    let value = Value::from_reader(Cursor::new(body))?;
    let root = value
        .as_dictionary()
        .ok_or(PtpSessionSetupError::InvalidRoot)?;
    let event = root
        .get("eventPort")
        .and_then(Value::as_unsigned_integer)
        .ok_or(PtpSessionSetupError::MissingEventPort)?;

    if !(1024..=65535).contains(&event) {
        return Err(PtpSessionSetupError::InvalidEventPort);
    }
    Ok(event as u16)
}

pub fn setup_ptp_session(
    flow: &mut NativeConnectFlow,
    channel: &mut EncryptedRtspChannel,
    config: &PtpSessionSetupConfig,
) -> Result<PtpSessionSetupResult, PtpSessionSetupError> {
    if flow.phase() != crate::NativePhase::TimingReady {
        flow.session_setup()?;
        unreachable!("session_setup succeeds only from TimingReady");
    }

    let request = RtspRequest {
        method: "SETUP".into(),
        uri: config.session_uri.clone(),
        cseq: config.cseq,
        user_agent: "AirPlay/670.6.2".into(),
        dacp_id: config.dacp_id.clone(),
        active_remote: config.active_remote.clone(),
        client_instance: None,
        content_type: Some("application/x-apple-binary-plist".into()),
        body: build_ptp_session_plist(config)?,
    };

    let response = channel.exchange(&request.encode(), config.cseq)?;
    if response.status != 200 {
        return Err(PtpSessionSetupError::Status(response.status));
    }

    let event_port = parse_event_port(&response.body)?;
    flow.session_setup()?;
    Ok(PtpSessionSetupResult { event_port })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> PtpSessionSetupConfig {
        PtpSessionSetupConfig {
            cseq: 1,
            session_uri: "rtsp://192.168.1.2/42".into(),
            session_uuid: "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE".into(),
            group_uuid: "11111111-2222-3333-4444-555555555555".into(),
            peer_uuid: "99999999-AAAA-BBBB-CCCC-DDDDDDDDDDDD".into(),
            device_id: "A1:B2:C3:D4:E5:F6:07:08".into(),
            mac_address: "A1:B2:C3:D4:E5:F6".into(),
            name: "HomePod".into(),
            local_address: "192.168.1.2".into(),
            clock_id: 0xA1B2C3D4E5F60708,
            dacp_id: "A1B2C3D4E5F60708".into(),
            active_remote: "123456789".into(),
        }
    }

    #[test]
    fn plist_matches_source_ptp_session_fields() {
        let body = build_ptp_session_plist(&config()).unwrap();
        let value = Value::from_reader(Cursor::new(body)).unwrap();
        let root = value.as_dictionary().unwrap();
        assert_eq!(root.len(), 9);
        assert_eq!(root.get("timingProtocol").and_then(Value::as_string), Some("PTP"));
        assert_eq!(
            root.get("groupContainsGroupLeader").and_then(Value::as_boolean),
            Some(false)
        );
        let peer = root.get("timingPeerInfo").unwrap().as_dictionary().unwrap();
        assert_eq!(
            peer.get("ClockID").and_then(Value::as_unsigned_integer),
            Some(0xA1B2C3D4E5F60708)
        );
        assert_eq!(
            peer.get("Addresses").unwrap().as_array().unwrap()[0].as_string(),
            Some("192.168.1.2")
        );
    }
}
