use crate::native_media::SendResult;
use crate::native_io::{send_datagram_deadline, UDP_SEND_TIMEOUT};
use crate::native_rtx::{build_response, parse_request, RtxCounters, RtxRing};
use std::io;
use std::net::UdpSocket;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub struct RtxWorker {
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl RtxWorker {
    pub fn start(
        socket: UdpSocket,
        ring: Arc<Mutex<RtxRing>>,
        counters: Arc<Mutex<RtxCounters>>,
    ) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(true));
        let stop_thread = Arc::clone(&stop);
        let running_thread = Arc::clone(&running);

        let worker = thread::Builder::new()
            .name("sairplay-msa-rtx".into())
            .spawn(move || {
                let mut buf = [0u8; 2048];
                while !stop_thread.load(Ordering::SeqCst) {
                    match socket.recv_from(&mut buf) {
                        Ok((n, peer)) => {
                            let Some(req) = parse_request(&buf[..n]) else { continue };
                            if let Ok(mut c) = counters.lock() {
                                c.requested = c.requested.saturating_add(req.count as u64);
                            }
                            for k in 0..req.count {
                                let seq = req.first_missing.wrapping_add(k);
                                let original = ring
                                    .lock()
                                    .ok()
                                    .and_then(|r| r.get(seq).map(|p| p.to_vec()));
                                let Some(original) = original else {
                                    if let Ok(mut c) = counters.lock() {
                                        c.expired = c.expired.saturating_add(1);
                                    }
                                    continue;
                                };
                                let Some(response) = build_response(req.request_seq, &original) else {
                                    if let Ok(mut c) = counters.lock() {
                                        c.expired = c.expired.saturating_add(1);
                                    }
                                    continue;
                                };
                                let result = send_datagram_deadline(
                                    &socket,
                                    &response,
                                    peer,
                                    UDP_SEND_TIMEOUT,
                                );
                                if result == SendResult::Sent {
                                    if let Ok(mut c) = counters.lock() {
                                        c.answered = c.answered.saturating_add(1);
                                    }
                                }
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
                running_thread.store(false, Ordering::SeqCst);
            })?;

        Ok(Self { stop, running, worker: Some(worker) })
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.running.store(false, Ordering::SeqCst);
    }
}

impl Drop for RtxWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr};

    #[test]
    fn worker_answers_exact_cached_packet_wrapper() {
        let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = rx.local_addr().unwrap();
        let worker_socket = rx.try_clone().unwrap();
        let ring = Arc::new(Mutex::new(RtxRing::default()));
        ring.lock().unwrap().store(0x20, &[1,2,3]);
        let counters = Arc::new(Mutex::new(RtxCounters::default()));
        let mut worker = RtxWorker::start(worker_socket, Arc::clone(&ring), Arc::clone(&counters)).unwrap();

        let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let req = [0x80,0xd5,0x12,0x34,0x00,0x20,0x00,0x01];
        client.send_to(&req, addr).unwrap();

        let mut out = [0u8;64];
        let (n, _) = client.recv_from(&mut out).unwrap();
        assert_eq!(&out[..n], &[0x80,0xd6,0x12,0x34,1,2,3]);
        worker.stop();
        let c = counters.lock().unwrap();
        assert_eq!((c.requested,c.answered,c.expired),(1,1,0));
    }
}
