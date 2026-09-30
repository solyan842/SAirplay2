use crate::{EventChannel, EventChannelError};
use plist::Value;
use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MrpRemoteCommand {
    Play,
    Pause,
    PlayPause,
    Next,
    Previous,
}

pub type MrpRemoteCommandCallback = Arc<dyn Fn(MrpRemoteCommand) + Send + Sync + 'static>;

pub struct MrpEventWorker {
    stop: Arc<AtomicBool>,
    healthy: Arc<AtomicBool>,
    commands: Arc<Mutex<VecDeque<MrpRemoteCommand>>>,
    callback: Arc<Mutex<Option<MrpRemoteCommandCallback>>>,
    worker: Option<JoinHandle<()>>,
}

impl MrpEventWorker {
    pub fn start(mut channel: EventChannel) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let healthy = Arc::new(AtomicBool::new(true));
        let commands = Arc::new(Mutex::new(VecDeque::new()));
        let callback = Arc::new(Mutex::new(None));

        let stop_thread = Arc::clone(&stop);
        let healthy_thread = Arc::clone(&healthy);
        let commands_thread = Arc::clone(&commands);
        let callback_thread = Arc::clone(&callback);

        let worker = thread::spawn(move || {
            let mut plain = Vec::<u8>::with_capacity(16 * 1024);
            while !stop_thread.load(Ordering::SeqCst) {
                match channel.read_plaintext() {
                    Ok(Some(frame)) => {
                        plain.extend_from_slice(&frame);
                        if !process_requests(
                            &mut channel,
                            &mut plain,
                            &commands_thread,
                            &callback_thread,
                        ) {
                            healthy_thread.store(false, Ordering::SeqCst);
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(_) => {
                        healthy_thread.store(false, Ordering::SeqCst);
                        break;
                    }
                }
            }
        });

        Self {
            stop,
            healthy,
            commands,
            callback,
            worker: Some(worker),
        }
    }

    pub fn healthy(&self) -> bool {
        self.healthy.load(Ordering::SeqCst)
    }

    pub fn pop_command(&self) -> Option<MrpRemoteCommand> {
        self.commands.lock().ok()?.pop_front()
    }

    pub fn set_callback(&self, callback: Option<MrpRemoteCommandCallback>) {
        if let Ok(mut slot) = self.callback.lock() {
            *slot = callback;
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for MrpEventWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn process_requests(
    channel: &mut EventChannel,
    plain: &mut Vec<u8>,
    commands: &Arc<Mutex<VecDeque<MrpRemoteCommand>>>,
    callback: &Arc<Mutex<Option<MrpRemoteCommandCallback>>>,
) -> bool {
    let mut off = 0usize;
    while off < plain.len() {
        let available = &plain[off..];
        let Some(header_end0) = available.windows(4).position(|w| w == b"\r\n\r\n") else {
            break;
        };
        let header_len = header_end0 + 4;
        if header_len >= 8192 {
            return false;
        }
        let Ok(header) = std::str::from_utf8(&available[..header_len]) else {
            return false;
        };

        let content_len = match header_value(header, "Content-Length") {
            Some(v) => match v.parse::<usize>() {
                Ok(n) if n <= 16 * 1024 => n,
                _ => return false,
            },
            None => 0,
        };
        if header_len + content_len > available.len() {
            break;
        }

        let first = header.split("\r\n").next().unwrap_or("");
        let mut parts = first.split_whitespace();
        let method = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("");
        let version = parts.next().unwrap_or("RTSP/1.0");
        if method.is_empty() || path.is_empty() {
            return false;
        }

        if method.eq_ignore_ascii_case("POST") && path == "/command" && content_len > 0 {
            let body = &available[header_len..header_len + content_len];
            if let Some(command) = parse_remote_command(body) {
                if let Ok(mut queue) = commands.lock() {
                    queue.push_back(command);
                }
                let cb = callback.lock().ok().and_then(|slot| slot.clone());
                if let Some(cb) = cb {
                    cb(command);
                }
            }
        }

        let cseq = header_value(header, "CSeq");
        let server = header_value(header, "Server");
        let mut response = format!(
            "{version} 200 OK\r\nContent-Length: 0\r\nAudio-Latency: 0\r\n"
        );
        if let Some(server) = server {
            response.push_str("Server: ");
            response.push_str(server);
            response.push_str("\r\n");
        }
        if let Some(cseq) = cseq {
            response.push_str("CSeq: ");
            response.push_str(cseq);
            response.push_str("\r\n");
        }
        response.push_str("\r\n");
        if channel.write_plaintext(response.as_bytes()).is_err() {
            return false;
        }

        off += header_len + content_len;
    }

    if off > 0 {
        plain.drain(..off);
    }
    true
}

fn header_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header
        .split("\r\n")
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .find_map(|(k, v)| k.eq_ignore_ascii_case(name).then_some(v.trim()))
}

fn parse_remote_command(body: &[u8]) -> Option<MrpRemoteCommand> {
    if body.len() < 8 || !body.starts_with(b"bplist00") {
        return None;
    }
    let root = Value::from_reader(Cursor::new(body)).ok()?;
    let dict = root.as_dictionary()?;
    if dict.get("type")?.as_string()? != "sendMediaRemoteCommand" {
        return None;
    }
    match dict.get("value")?.as_string()? {
        "play" => Some(MrpRemoteCommand::Play),
        "paus" => Some(MrpRemoteCommand::Pause),
        "plps" => Some(MrpRemoteCommand::PlayPause),
        "nitm" => Some(MrpRemoteCommand::Next),
        "pitm" => Some(MrpRemoteCommand::Previous),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_command_tokens_map_exactly() {
        for (token, expected) in [
            ("play", MrpRemoteCommand::Play),
            ("paus", MrpRemoteCommand::Pause),
            ("plps", MrpRemoteCommand::PlayPause),
            ("nitm", MrpRemoteCommand::Next),
            ("pitm", MrpRemoteCommand::Previous),
        ] {
            let mut d = plist::Dictionary::new();
            d.insert("type".into(), Value::String("sendMediaRemoteCommand".into()));
            d.insert("value".into(), Value::String(token.into()));
            let mut body = Vec::new();
            Value::Dictionary(d).to_writer_binary(&mut body).unwrap();
            assert_eq!(parse_remote_command(&body), Some(expected));
        }
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let h = "POST /command RTSP/1.0\r\ncSeQ: 9\r\nServer: AirTunes\r\n\r\n";
        assert_eq!(header_value(h, "CSeq"), Some("9"));
        assert_eq!(header_value(h, "server"), Some("AirTunes"));
    }
}
