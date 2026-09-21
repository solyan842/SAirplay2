use std::io;
use std::net::UdpSocket;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const RTX_RING_SLOTS: usize = 512;
const RTX_POLL: Duration = Duration::from_millis(200);

#[derive(Clone)]
pub struct RetransmitRing {
    slots: Arc<Mutex<Vec<Option<RtxSlot>>>>,
}

#[derive(Clone)]
struct RtxSlot {
    seq: u16,
    packet: Vec<u8>,
}

impl RetransmitRing {
    pub fn new() -> Self {
        Self {
            slots: Arc::new(Mutex::new(vec![None; RTX_RING_SLOTS])),
        }
    }

    pub fn store(&self, seq: u16, packet: &[u8]) {
        if let Ok(mut slots) = self.slots.lock() {
            slots[seq as usize % RTX_RING_SLOTS] = Some(RtxSlot {
                seq,
                packet: packet.to_vec(),
            });
        }
    }

    fn lookup(&self, seq: u16) -> Option<Vec<u8>> {
        self.slots
            .lock()
            .ok()
            .and_then(|slots| slots[seq as usize % RTX_RING_SLOTS].clone())
            .filter(|slot| slot.seq == seq)
            .map(|slot| slot.packet)
    }
}

impl Default for RetransmitRing {
    fn default() -> Self { Self::new() }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetransmitStats {
    pub requested: u64,
    pub answered: u64,
    pub expired: u64,
}

pub struct RetransmitWorker {
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    requested: Arc<AtomicU64>,
    answered: Arc<AtomicU64>,
    expired: Arc<AtomicU64>,
    worker: Option<JoinHandle<()>>,
}

impl RetransmitWorker {
    pub fn start(socket: UdpSocket, ring: RetransmitRing) -> io::Result<Self> {
        socket.set_nonblocking(true)?;

        let stop = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(true));
        let stop_thread = Arc::clone(&stop);
        let running_thread = Arc::clone(&running);
        let requested = Arc::new(AtomicU64::new(0));
        let answered = Arc::new(AtomicU64::new(0));
        let expired = Arc::new(AtomicU64::new(0));
        let requested_thread = Arc::clone(&requested);
        let answered_thread = Arc::clone(&answered);
        let expired_thread = Arc::clone(&expired);

        let worker = thread::Builder::new()
            .name("sairplay-rtx".into())
            .spawn(move || {
                let mut buf = [0u8; 512];
                while !stop_thread.load(Ordering::SeqCst) {
                    let mut received_any = false;
                    for _ in 0..256 {
                        let (n, from) = match socket.recv_from(&mut buf) {
                            Ok(v) => v,
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(_) => {
                                running_thread.store(false, Ordering::SeqCst);
                                return;
                            }
                        };
                        received_any = true;
                        if n < 8 || (buf[1] & 0x7f) != 0x55 {
                            continue;
                        }

                        let req_seq = u16::from_be_bytes([buf[2], buf[3]]);
                        let first = u16::from_be_bytes([buf[4], buf[5]]);
                        let requested = u16::from_be_bytes([buf[6], buf[7]]);
                        let count = if requested == 0 {
                            1usize
                        } else {
                            (requested as usize).min(RTX_RING_SLOTS)
                        };

                        requested_thread.fetch_add(count as u64, Ordering::SeqCst);
                        for k in 0..count {
                            let seq = first.wrapping_add(k as u16);
                            let Some(packet) = ring.lookup(seq) else {
                                expired_thread.fetch_add(1, Ordering::SeqCst);
                                continue;
                            };
                            let mut out = Vec::with_capacity(4 + packet.len());
                            out.extend_from_slice(&[
                                0x80,
                                0xD6,
                                (req_seq >> 8) as u8,
                                req_seq as u8,
                            ]);
                            out.extend_from_slice(&packet);
                            if socket.send_to(&out, from).is_ok() {
                                answered_thread.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    }

                    if !received_any {
                        thread::sleep(RTX_POLL);
                    }
                }
                running_thread.store(false, Ordering::SeqCst);
            })?;

        Ok(Self {
            stop,
            running,
            requested,
            answered,
            expired,
            worker: Some(worker),
        })
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn stats(&self) -> RetransmitStats {
        RetransmitStats {
            requested: self.requested.load(Ordering::SeqCst),
            answered: self.answered.load(Ordering::SeqCst),
            expired: self.expired.load(Ordering::SeqCst),
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.running.store(false, Ordering::SeqCst);
    }
}

impl Drop for RetransmitWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, UdpSocket};

    #[test]
    fn ring_is_exact_512_slot_sequence_lookup() {
        let ring = RetransmitRing::new();
        ring.store(7, b"seven");
        ring.store((7u16).wrapping_add(RTX_RING_SLOTS as u16), b"new");
        assert!(ring.lookup(7).is_none());
        assert_eq!(ring.lookup(519).as_deref(), Some(b"new".as_slice()));
    }

    #[test]
    fn responder_wraps_original_wire_packet_in_d6() {
        let control = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = control.local_addr().unwrap();
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        sender.set_read_timeout(Some(Duration::from_secs(1))).unwrap();

        let ring = RetransmitRing::new();
        ring.store(1000, b"wire-rtp");
        let mut worker = RetransmitWorker::start(control, ring).unwrap();

        let req = [0x80, 0xD5, 0x12, 0x34, 0x03, 0xE8, 0x00, 0x01];
        sender.send_to(&req, addr).unwrap();

        let mut out = [0u8; 64];
        let (n, _) = sender.recv_from(&mut out).unwrap();
        assert_eq!(&out[..4], &[0x80, 0xD6, 0x12, 0x34]);
        assert_eq!(&out[4..n], b"wire-rtp");

        worker.stop();
    }
}
