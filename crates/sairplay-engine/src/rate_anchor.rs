use crate::{EncryptedRtspChannel, EncryptedRtspError, PtpClock, RtspRequest};
use plist::{Dictionary, Value};

#[derive(Debug, Clone)]
pub struct RateAnchorConfig {
    pub cseq: u32,
    pub session_uri: String,
    pub dacp_id: String,
    pub active_remote: String,
    pub rtp_time: u32,
    pub anchor_ns: u64,
    pub rate: u64,
}

#[derive(Debug)]
pub enum RateAnchorError {
    Transport(EncryptedRtspError),
    Plist(plist::Error),
    Status(u16),
}

impl From<EncryptedRtspError> for RateAnchorError {
    fn from(value: EncryptedRtspError) -> Self { Self::Transport(value) }
}
impl From<plist::Error> for RateAnchorError {
    fn from(value: plist::Error) -> Self { Self::Plist(value) }
}

pub fn build_setrateanchortime_plist(
    clock_id: u64,
    rtp_time: u32,
    anchor_ns: u64,
    rate: u64,
) -> Result<Vec<u8>, RateAnchorError> {
    let secs = anchor_ns / 1_000_000_000;
    let rem_ns = anchor_ns % 1_000_000_000;

    // Exact pinned-MSA conversion:
    // frac32 = rem_ns * 2^32 / 1e9, then place that value in the high
    // 32 bits of Apple's 64-bit networkTimeFrac field.
    let frac32 = ((rem_ns as u128) << 32) / 1_000_000_000u128;
    let network_time_frac = (frac32 as u64) << 32;

    let mut root = Dictionary::new();
    root.insert(
        "networkTimeTimelineID".into(),
        Value::Integer(clock_id.into()),
    );
    root.insert("networkTimeSecs".into(), Value::Integer(secs.into()));
    root.insert(
        "networkTimeFrac".into(),
        Value::Integer(network_time_frac.into()),
    );
    root.insert("rtpTime".into(), Value::Integer((rtp_time as u64).into()));
    root.insert("rate".into(), Value::Integer(rate.into()));

    let mut out = Vec::new();
    Value::Dictionary(root).to_writer_binary(&mut out)?;
    Ok(out)
}

pub fn send_setrateanchortime(
    channel: &mut EncryptedRtspChannel,
    clock: &PtpClock,
    config: &RateAnchorConfig,
) -> Result<(), RateAnchorError> {
    let body = build_setrateanchortime_plist(
        clock.master_clock_id(),
        config.rtp_time,
        config.anchor_ns,
        config.rate,
    )?;

    let request = RtspRequest {
        method: "SETRATEANCHORTIME".into(),
        uri: config.session_uri.clone(),
        cseq: config.cseq,
        user_agent: "AirPlay/670.6.2".into(),
        dacp_id: config.dacp_id.clone(),
        active_remote: config.active_remote.clone(),
        client_instance: None,
        content_type: Some("application/x-apple-binary-plist".into()),
        body,
    };

    let response = channel.exchange(&request.encode(), config.cseq)?;
    if response.status != 200 {
        return Err(RateAnchorError::Status(response.status));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HapControlCipher;
    use std::io::{Cursor, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::Duration;

    fn read_one_frame(socket: &mut TcpStream, cipher: &mut HapControlCipher) -> Vec<u8> {
        let mut carry = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = socket.read(&mut buf).unwrap();
            carry.extend_from_slice(&buf[..n]);
            if carry.len() < 2 { continue; }
            let plen = u16::from_le_bytes([carry[0], carry[1]]) as usize;
            let frame_len = 2 + plen + 16;
            if carry.len() >= frame_len {
                return cipher.decrypt(&carry[..frame_len]).unwrap();
            }
        }
    }

    #[test]
    fn plist_matches_pinned_msa_anchor_fields_and_fraction() {
        let clock_id = 0x1122334455667788;
        let anchor_ns = 12_345_678_901u64;
        let body = build_setrateanchortime_plist(
            clock_id,
            0xAABBCCDD,
            anchor_ns,
            1,
        ).unwrap();

        let value = Value::from_reader(Cursor::new(&body)).unwrap();
        let root = value.as_dictionary().unwrap();

        assert_eq!(
            root.get("networkTimeTimelineID").and_then(Value::as_unsigned_integer),
            Some(clock_id)
        );
        assert_eq!(
            root.get("networkTimeSecs").and_then(Value::as_unsigned_integer),
            Some(12)
        );

        let rem_ns = 345_678_901u64;
        let frac32 = (((rem_ns as u128) << 32) / 1_000_000_000u128) as u64;
        assert_eq!(
            root.get("networkTimeFrac").and_then(Value::as_unsigned_integer),
            Some(frac32 << 32)
        );
        assert_eq!(
            root.get("rtpTime").and_then(Value::as_unsigned_integer),
            Some(0xAABBCCDD)
        );
        assert_eq!(
            root.get("rate").and_then(Value::as_unsigned_integer),
            Some(1)
        );
    }

    #[test]
    fn encrypted_request_uses_setrateanchortime_and_session_uri() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let key = [0x91u8; 32];

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut cipher = HapControlCipher::new(key, key);
            let plain = read_one_frame(&mut socket, &mut cipher);

            let header_end = plain.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
            let header = std::str::from_utf8(&plain[..header_end]).unwrap();
            assert!(header.starts_with(
                "SETRATEANCHORTIME rtsp://127.0.0.1/session RTSP/1.0\r\n"
            ));
            assert!(header.contains("CSeq: 9\r\n"));
            assert!(header.contains(
                "Content-Type: application/x-apple-binary-plist\r\n"
            ));

            let body = Value::from_reader(Cursor::new(&plain[header_end..])).unwrap();
            let root = body.as_dictionary().unwrap();
            assert_eq!(
                root.get("networkTimeTimelineID").and_then(Value::as_unsigned_integer),
                Some(0x1020304050607080)
            );
            assert_eq!(
                root.get("rtpTime").and_then(Value::as_unsigned_integer),
                Some(55_000)
            );
            assert_eq!(
                root.get("rate").and_then(Value::as_unsigned_integer),
                Some(1)
            );

            let reply = b"RTSP/1.0 200 OK\r\nCSeq: 9\r\nContent-Length: 0\r\n\r\n";
            let wire = cipher.encrypt(reply).unwrap();
            socket.write_all(&wire).unwrap();
        });

        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let mut channel = EncryptedRtspChannel::new(
            stream,
            key,
            key,
            Duration::from_secs(2),
        );

        let clock = PtpClock::fixed(0x1020304050607080);
        let config = RateAnchorConfig {
            cseq: 9,
            session_uri: "rtsp://127.0.0.1/session".into(),
            dacp_id: "AABBCCDDEEFF0011".into(),
            active_remote: "123456789".into(),
            rtp_time: 55_000,
            anchor_ns: 1_234_567_890,
            rate: 1,
        };

        send_setrateanchortime(&mut channel, &clock, &config).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn non_200_anchor_is_rejected() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let key = [0x92u8; 32];

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut cipher = HapControlCipher::new(key, key);
            let _ = read_one_frame(&mut socket, &mut cipher);
            let reply = b"RTSP/1.0 400 Error\r\nCSeq: 10\r\nContent-Length: 0\r\n\r\n";
            let wire = cipher.encrypt(reply).unwrap();
            socket.write_all(&wire).unwrap();
        });

        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let mut channel = EncryptedRtspChannel::new(
            stream,
            key,
            key,
            Duration::from_secs(2),
        );
        let clock = PtpClock::fixed(1);
        let config = RateAnchorConfig {
            cseq: 10,
            session_uri: "rtsp://127.0.0.1/session".into(),
            dacp_id: "AABBCCDDEEFF0011".into(),
            active_remote: "123456789".into(),
            rtp_time: 0,
            anchor_ns: 0,
            rate: 1,
        };

        assert!(matches!(
            send_setrateanchortime(&mut channel, &clock, &config),
            Err(RateAnchorError::Status(400))
        ));
        server.join().unwrap();
    }
}
