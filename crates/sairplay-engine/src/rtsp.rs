use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspRequest {
    pub method: String,
    pub uri: String,
    pub cseq: u32,
    pub user_agent: String,
    pub dacp_id: String,
    pub active_remote: String,
    pub client_instance: Option<String>,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

impl RtspRequest {
    pub fn get_info(cseq: u32, dacp_id: impl Into<String>, active_remote: impl Into<String>) -> Self {
        let dacp_id = dacp_id.into();
        Self {
            method: "GET".into(),
            uri: "/info".into(),
            cseq,
            user_agent: "AirPlay/670.6.2".into(),
            client_instance: None,
            dacp_id,
            active_remote: active_remote.into(),
            content_type: None,
            body: Vec::new(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut head = format!(
            "{} {} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: {}\r\nDACP-ID: {}\r\nActive-Remote: {}\r\n",
            self.method,
            self.uri,
            self.cseq,
            self.user_agent,
            self.dacp_id,
            self.active_remote
        );

        if let Some(client_instance) = &self.client_instance {
            head.push_str(&format!("Client-Instance: {client_instance}\r\n"));
        }
        if let Some(content_type) = &self.content_type {
            head.push_str(&format!("Content-Type: {content_type}\r\n"));
        }

        head.push_str(&format!("Content-Length: {}\r\n\r\n", self.body.len()));

        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspResponse {
    pub version: String,
    pub status: u16,
    pub reason: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl RtspResponse {
    pub fn cseq(&self) -> Option<u32> {
        self.header("cseq")?.parse().ok()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtspError {
    HeaderTooLarge,
    InvalidStatusLine,
    InvalidStatusCode,
    InvalidHeader,
    InvalidContentLength,
    MissingCseq,
}

pub struct RtspCodec {
    carry: Vec<u8>,
    max_header_bytes: usize,
    max_body_bytes: usize,
}

impl Default for RtspCodec {
    fn default() -> Self {
        Self {
            carry: Vec::new(),
            max_header_bytes: 16 * 1024,
            max_body_bytes: 4 * 1024 * 1024,
        }
    }
}

impl RtspCodec {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<RtspResponse>, RtspError> {
        self.carry.extend_from_slice(bytes);
        let mut out = Vec::new();

        loop {
            let Some(header_end) = find_double_crlf(&self.carry) else {
                if self.carry.len() > self.max_header_bytes {
                    return Err(RtspError::HeaderTooLarge);
                }
                break;
            };

            let header_len = header_end + 4;
            if header_len > self.max_header_bytes {
                return Err(RtspError::HeaderTooLarge);
            }

            let header_bytes = &self.carry[..header_end];
            let header_text = std::str::from_utf8(header_bytes)
                .map_err(|_| RtspError::InvalidHeader)?;

            let mut lines = header_text.split("\r\n");
            let status_line = lines.next().ok_or(RtspError::InvalidStatusLine)?;
            let mut parts = status_line.splitn(3, ' ');
            let version = parts
                .next()
                .ok_or(RtspError::InvalidStatusLine)?
                .to_string();
            let status = parts
                .next()
                .ok_or(RtspError::InvalidStatusLine)?
                .parse::<u16>()
                .map_err(|_| RtspError::InvalidStatusCode)?;
            let reason = parts.next().unwrap_or("").to_string();

            if !version.starts_with("RTSP/") {
                return Err(RtspError::InvalidStatusLine);
            }

            let mut headers = BTreeMap::new();
            for line in lines {
                let (name, value) = line
                    .split_once(':')
                    .ok_or(RtspError::InvalidHeader)?;
                let key = name.trim().to_ascii_lowercase();
                if key.is_empty() {
                    return Err(RtspError::InvalidHeader);
                }
                headers.insert(key, value.trim().to_string());
            }

            let content_len = match headers.get("content-length") {
                Some(value) => value
                    .parse::<usize>()
                    .map_err(|_| RtspError::InvalidContentLength)?,
                None => 0,
            };

            if content_len > self.max_body_bytes {
                return Err(RtspError::InvalidContentLength);
            }

            let message_len = header_len + content_len;
            if self.carry.len() < message_len {
                break;
            }

            let body = self.carry[header_len..message_len].to_vec();
            self.carry.drain(..message_len);

            out.push(RtspResponse {
                version,
                status,
                reason,
                headers,
                body,
            });
        }

        Ok(out)
    }

    pub fn take_matching(
        responses: &mut Vec<RtspResponse>,
        expected_cseq: u32,
    ) -> Result<Option<RtspResponse>, RtspError> {
        let mut match_index = None;

        for (index, response) in responses.iter().enumerate() {
            let cseq = response.cseq().ok_or(RtspError::MissingCseq)?;
            if cseq == expected_cseq {
                match_index = Some(index);
                break;
            }
        }

        Ok(match_index.map(|index| responses.remove(index)))
    }

    pub fn carry_len(&self) -> usize {
        self.carry.len()
    }
}

fn find_double_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(cseq: u32, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn info_request_has_reference_headers_and_empty_body() {
        let req = RtspRequest::get_info(1, "AABBCCDDEEFF0011", "123456789");
        let encoded = String::from_utf8(req.encode()).unwrap();

        assert!(encoded.starts_with("GET /info RTSP/1.0\r\n"));
        assert!(encoded.contains("CSeq: 1\r\n"));
        assert!(encoded.contains("User-Agent: AirPlay/670.6.2\r\n"));
        assert!(encoded.contains("DACP-ID: AABBCCDDEEFF0011\r\n"));
        assert!(encoded.contains("Active-Remote: 123456789\r\n"));
        assert!(!encoded.contains("Client-Instance:"));
        assert!(encoded.ends_with("Content-Length: 0\r\n\r\n"));
    }

    #[test]
    fn fragmented_response_is_reassembled_without_losing_carry() {
        let msg = response(7, b"bplist00");
        let mut codec = RtspCodec::default();

        let first = codec.push(&msg[..13]).unwrap();
        assert!(first.is_empty());
        assert_eq!(codec.carry_len(), 13);

        let second = codec.push(&msg[13..]).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].cseq(), Some(7));
        assert_eq!(second[0].body, b"bplist00");
        assert_eq!(codec.carry_len(), 0);
    }

    #[test]
    fn multiple_responses_in_one_read_are_split() {
        let mut wire = response(10, b"old");
        wire.extend_from_slice(&response(11, b"new"));

        let mut codec = RtspCodec::default();
        let parsed = codec.push(&wire).unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].cseq(), Some(10));
        assert_eq!(parsed[1].cseq(), Some(11));
    }

    #[test]
    fn delayed_old_response_does_not_steal_current_exchange() {
        let mut pending = Vec::new();
        let mut codec = RtspCodec::default();

        // A late CSeq 20 arrives before the response to current CSeq 21.
        pending.extend(codec.push(&response(20, b"late-feedback")).unwrap());
        assert!(RtspCodec::take_matching(&mut pending, 21).unwrap().is_none());
        assert_eq!(pending.len(), 1);

        pending.extend(codec.push(&response(21, b"current")).unwrap());
        let current = RtspCodec::take_matching(&mut pending, 21)
            .unwrap()
            .expect("current response");

        assert_eq!(current.body, b"current");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].cseq(), Some(20));
    }

    #[test]
    fn response_body_may_contain_header_like_bytes() {
        let body = b"abc\r\n\r\ndef";
        let mut codec = RtspCodec::default();
        let parsed = codec.push(&response(3, body)).unwrap();
        assert_eq!(parsed[0].body, body);
    }
}
