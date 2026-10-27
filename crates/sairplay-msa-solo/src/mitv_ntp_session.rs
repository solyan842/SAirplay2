//! Explicit MiTV test adapter; no change to native session defaults.
use crate::ntp_session_setup::{
    build_ntp_session_plist, setup_ntp_session_with_body,
    NtpSessionSetupConfig, NtpSessionSetupError, NtpSessionSetupResult,
};
use crate::{EncryptedRtspChannel, NativeConnectFlow};
use plist::Value;
use std::io::Cursor;

pub(super) fn enabled(endpoint: Option<&str>, host: &str, port: u16) -> bool {
    port == 52266 && endpoint.is_some_and(|v| v == format!("{host}:{port}"))
}

fn session_identity(body: &[u8]) -> Result<Vec<u8>, NtpSessionSetupError> {
    let mut value = Value::from_reader(Cursor::new(body))?;
    let root = value.as_dictionary_mut().ok_or(NtpSessionSetupError::InvalidRoot)?;
    // Hardware iPhone SETUP reports et=32. The installed HappyCast parser uses
    // nonzero et plus sourceVersion to classify the session-only first SETUP.
    // Match our existing AirPlay/670.6.2 agent; do not borrow iPhone identifiers.
    root.insert("et".into(), Value::Integer(32u64.into()));
    root.insert("sourceVersion".into(), Value::String("670.6.2".into()));
    // Receiver 0x202d4c looks up this key. With no streams and a modern source
    // version, its presence selects the session-only response path rather than
    // the stream type check. false describes this audio-only session.
    root.insert("isScreenMirroringSession".into(), Value::Boolean(false));
    let mut out = Vec::new();
    value.to_writer_binary(&mut out)?;
    Ok(out)
}

pub(super) fn setup(
    flow: &mut NativeConnectFlow,
    channel: &mut EncryptedRtspChannel,
    config: &NtpSessionSetupConfig,
) -> Result<NtpSessionSetupResult, NtpSessionSetupError> {
    let body = build_ntp_session_plist(
        &config.session_uuid, config.device_id.as_deref(), config.timing_port,
    )?;
    setup_ntp_session_with_body(flow, channel, config, session_identity(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mitv_identity_changes_only_three_session_keys() {
        let body = build_ntp_session_plist("SESSION", Some("OUR-DEVICE-ID"), 54321).unwrap();
        let base = Value::from_reader(Cursor::new(&body)).unwrap();
        let decorated = session_identity(&body).unwrap();
        let value = Value::from_reader(Cursor::new(&decorated)).unwrap();
        let root = value.as_dictionary().unwrap();
        assert_eq!(root.len(), base.as_dictionary().unwrap().len() + 3);
        for (key, value) in base.as_dictionary().unwrap() {
            assert_eq!(root.get(key), Some(value));
        }
        assert_eq!(root.get("et").and_then(Value::as_unsigned_integer), Some(32));
        assert_eq!(root.get("sourceVersion").and_then(Value::as_string), Some("670.6.2"));
        assert_eq!(root.get("isScreenMirroringSession").and_then(Value::as_boolean), Some(false));
        assert!(!root.contains_key("streams"));
        assert!(!root.contains_key("ekey"));
    }

    #[test]
    fn mitv_identity_adapter_requires_exact_explicit_endpoint() {
        assert!(enabled(Some("192.168.88.15:52266"), "192.168.88.15", 52266));
        assert!(!enabled(None, "192.168.88.15", 52266));
        assert!(!enabled(Some("192.168.88.15:52266"), "192.168.88.16", 52266));
        assert!(!enabled(Some("192.168.88.15:7000"), "192.168.88.15", 7000));
    }
}
