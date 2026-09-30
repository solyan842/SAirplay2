use socket2::SockRef;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const EVENT_PORT: u16 = 319;
const GENERAL_PORT: u16 = 320;
const MCAST_ADDR: Ipv4Addr = Ipv4Addr::new(224, 0, 1, 129);
const HDR_LEN: usize = 34;

const MSG_SYNC: u8 = 0x0;
const MSG_DELAY_REQ: u8 = 0x1;
const MSG_PDELAY_REQ: u8 = 0x2;
const MSG_PDELAY_RESP: u8 = 0x3;
const MSG_FOLLOW_UP: u8 = 0x8;
const MSG_DELAY_RESP: u8 = 0x9;
const MSG_PDELAY_RESP_FUP: u8 = 0xA;
const MSG_ANNOUNCE: u8 = 0xB;
const MSG_SIGNALING: u8 = 0xC;

const FLAG_TWO_STEP: u16 = 0x0200;
const FLAG_UNICAST: u16 = 0x0400;
const FLAG_PTP_TIMESCALE: u16 = 0x0008;

const TLV_REQUEST_UNICAST: u16 = 0x0004;
const TLV_GRANT_UNICAST: u16 = 0x0005;
const OFFSET_SNAP_NS: i64 = 1_000_000;
const OFFSET_EMA_DIV: i64 = 8;
const FOLLOW_STALE_NS: u64 = 60_000_000_000;
const EXCHANGE_GAP_NS: u64 = 3_000_000_000;
const PTP_MAX_PEERS: usize = 8;

#[derive(Debug)]
pub enum PtpEngineError {
    Ipv4Required,
    Bind { port: u16, source: io::Error },
    Configure(io::Error),
    Spawn(io::Error),
    PeerLimit { max: usize },
}

impl fmt::Display for PtpEngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ipv4Required => write!(f, "PTP alpha engine currently requires IPv4"),
            Self::Bind { port, source } => write!(f, "PTP UDP {port} bind failed: {source}"),
            Self::Configure(e) => write!(f, "PTP socket configure failed: {e}"),
            Self::Spawn(e) => write!(f, "PTP worker start failed: {e}"),
            Self::PeerLimit { max } => write!(f, "PTP receiver table is full ({max} receivers)"),
        }
    }
}

impl std::error::Error for PtpEngineError {}

#[derive(Debug, Default, Clone, Copy)]
struct ExchangeState {
    first_ns: u64,
    third_ns: u64,
    last_ns: u64,
    count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtpExchange {
    pub count: u32,
    pub first_ms: u64,
    pub last_ms: u64,
    pub third_ms: u64,
}

#[derive(Debug)]
struct ReceiverClockState {
    receiver: Ipv4Addr,
    follow_enabled: bool,
    clock_id: Option<u64>,
    offset_ns: Option<i64>,
    last_ns: u64,
    pending_sync_seq: Option<u16>,
    pending_sync_rx_ns: u64,
    pending_sync_corr_ns: i64,
    exchange: ExchangeState,
}

impl ReceiverClockState {
    fn new(receiver: Ipv4Addr, follow_enabled: bool) -> Self {
        Self {
            receiver,
            follow_enabled,
            clock_id: None,
            offset_ns: None,
            last_ns: 0,
            pending_sync_seq: None,
            pending_sync_rx_ns: 0,
            pending_sync_corr_ns: 0,
            exchange: ExchangeState::default(),
        }
    }

    fn clear_follow_lock(&mut self) {
        self.clock_id = None;
        self.offset_ns = None;
        self.pending_sync_seq = None;
    }

    fn follow_locked(&mut self, now: u64) -> bool {
        if !self.follow_enabled {
            return false;
        }
        if self.last_ns != 0 && now.saturating_sub(self.last_ns) > FOLLOW_STALE_NS {
            self.clear_follow_lock();
            return false;
        }
        self.clock_id.is_some() && self.offset_ns.is_some()
    }
}

#[derive(Debug, Default)]
struct ReceiverClockTable {
    entries: Vec<ReceiverClockState>,
}

impl ReceiverClockTable {
    fn register(
        &mut self,
        receiver: Ipv4Addr,
        follow_enabled: bool,
    ) -> Result<(), PtpEngineError> {
        if let Some(state) = self.entries.iter_mut().find(|state| state.receiver == receiver) {
            if state.follow_enabled != follow_enabled {
                state.follow_enabled = follow_enabled;
                state.clear_follow_lock();
            }
            return Ok(());
        }
        if self.entries.len() >= PTP_MAX_PEERS {
            return Err(PtpEngineError::PeerLimit { max: PTP_MAX_PEERS });
        }
        self.entries
            .push(ReceiverClockState::new(receiver, follow_enabled));
        Ok(())
    }

    fn remove(&mut self, receiver: Ipv4Addr) {
        if let Some(index) = self.entries.iter().position(|state| state.receiver == receiver) {
            self.entries.remove(index);
        }
    }

    fn state(&self, receiver: Ipv4Addr) -> Option<&ReceiverClockState> {
        self.entries.iter().find(|state| state.receiver == receiver)
    }

    fn state_mut(&mut self, receiver: Ipv4Addr) -> Option<&mut ReceiverClockState> {
        self.entries
            .iter_mut()
            .find(|state| state.receiver == receiver)
    }

    fn is_followed(&self, receiver: Ipv4Addr) -> bool {
        self.state(receiver)
            .is_some_and(|state| state.follow_enabled)
    }
}

#[derive(Clone, Debug)]
pub struct PtpClock {
    local_clock_id: u64,
    receiver: Option<Ipv4Addr>,
    clocks: Arc<Mutex<ReceiverClockTable>>,
}

impl PtpClock {
    pub fn fixed(clock_id: u64) -> Self {
        Self {
            local_clock_id: clock_id,
            receiver: None,
            clocks: Arc::new(Mutex::new(ReceiverClockTable::default())),
        }
    }

    pub fn master_clock_id(&self) -> u64 {
        let Some(receiver) = self.receiver else {
            return self.local_clock_id;
        };
        let now = now_unix_ns();
        if let Ok(mut clocks) = self.clocks.lock() {
            if let Some(state) = clocks.state_mut(receiver) {
                if state.follow_locked(now) {
                    return state.clock_id.unwrap_or(self.local_clock_id);
                }
            }
        }
        self.local_clock_id
    }

    pub fn master_now_ns(&self) -> u64 {
        let local = now_unix_ns();
        let Some(receiver) = self.receiver else {
            return local;
        };
        if let Ok(mut clocks) = self.clocks.lock() {
            if let Some(state) = clocks.state_mut(receiver) {
                if state.follow_locked(local) {
                    let offset = state.offset_ns.unwrap_or(0);
                    return if offset >= 0 {
                        local.saturating_add(offset as u64)
                    } else {
                        local.saturating_sub(offset.unsigned_abs())
                    };
                }
            }
        }
        local
    }

    pub fn is_follow_locked(&self) -> bool {
        let Some(receiver) = self.receiver else {
            return false;
        };
        let now = now_unix_ns();
        self.clocks
            .lock()
            .ok()
            .and_then(|mut clocks| {
                clocks
                    .state_mut(receiver)
                    .map(|state| state.follow_locked(now))
            })
            .unwrap_or(false)
    }

    pub fn exchange(&self) -> Option<PtpExchange> {
        let receiver = self.receiver?;
        let now = now_unix_ns();
        let clocks = self.clocks.lock().ok()?;
        let ex = clocks.state(receiver)?.exchange;
        if ex.count == 0 || now.saturating_sub(ex.last_ns) > EXCHANGE_GAP_NS {
            return None;
        }
        Some(PtpExchange {
            count: ex.count,
            first_ms: now.saturating_sub(ex.first_ns) / 1_000_000,
            last_ms: now.saturating_sub(ex.last_ns) / 1_000_000,
            third_ms: if ex.count >= 3 {
                now.saturating_sub(ex.third_ns) / 1_000_000
            } else {
                0
            },
        })
    }
}

pub struct PtpEngine {
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    clock_id: u64,
    primary_receiver: Ipv4Addr,
    peers: Arc<Mutex<Vec<Ipv4Addr>>>,
    peer_kick: Arc<AtomicBool>,
    clocks: Arc<Mutex<ReceiverClockTable>>,
    clock: PtpClock,
}

impl PtpEngine {
    pub fn start(
        receiver_ip: IpAddr,
        bind_ip: IpAddr,
        clock_id: u64,
        follow_receiver_clock: bool,
    ) -> Result<Self, PtpEngineError> {
        let receiver = match receiver_ip {
            IpAddr::V4(ip) => ip,
            IpAddr::V6(_) => return Err(PtpEngineError::Ipv4Required),
        };
        let bind_interface = match bind_ip {
            IpAddr::V4(ip) => ip,
            IpAddr::V6(_) => return Err(PtpEngineError::Ipv4Required),
        };

        let event = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, EVENT_PORT))
            .map_err(|source| PtpEngineError::Bind { port: EVENT_PORT, source })?;
        let general = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, GENERAL_PORT)) {
            Ok(socket) => socket,
            Err(source) => {
                drop(event);
                return Err(PtpEngineError::Bind { port: GENERAL_PORT, source });
            }
        };

        event.set_nonblocking(true).map_err(PtpEngineError::Configure)?;
        general.set_nonblocking(true).map_err(PtpEngineError::Configure)?;

        // Match upstream ptp_open_socket(): multicast membership is best-effort,
        // while bind failure itself is what triggers NTP fallback.
        let _ = event.join_multicast_v4(&MCAST_ADDR, &bind_interface);
        let _ = general.join_multicast_v4(&MCAST_ADDR, &bind_interface);
        // Upstream sets both membership interface and multicast egress
        // interface. std::net exposes only the former, so use the socket API
        // for the exact IP_MULTICAST_IF behavior.
        let _ = SockRef::from(&event).set_multicast_if_v4(&bind_interface);
        let _ = SockRef::from(&general).set_multicast_if_v4(&bind_interface);
        let _ = event.set_multicast_ttl_v4(1);
        let _ = general.set_multicast_ttl_v4(1);
        let _ = event.set_multicast_loop_v4(false);
        let _ = general.set_multicast_loop_v4(false);

        let running = Arc::new(AtomicBool::new(true));
        let running_thread = Arc::clone(&running);
        let peers = Arc::new(Mutex::new(Vec::<Ipv4Addr>::new()));
        let peers_thread = Arc::clone(&peers);
        let peer_kick = Arc::new(AtomicBool::new(false));
        let kick_thread = Arc::clone(&peer_kick);
        let mut initial_clocks = ReceiverClockTable::default();
        initial_clocks.register(receiver, follow_receiver_clock)?;
        let clocks = Arc::new(Mutex::new(initial_clocks));
        let clocks_thread = Arc::clone(&clocks);
        let clock = PtpClock {
            local_clock_id: clock_id,
            receiver: Some(receiver),
            clocks: Arc::clone(&clocks),
        };

        let worker = thread::Builder::new()
            .name("sairplay-ptp".into())
            .spawn(move || {
                run_ptp_loop(
                    event,
                    general,
                    clock_id,
                    running_thread,
                    peers_thread,
                    kick_thread,
                    clocks_thread,
                );
            })
            .map_err(PtpEngineError::Spawn)?;

        Ok(Self {
            running,
            worker: Some(worker),
            clock_id,
            primary_receiver: receiver,
            peers,
            peer_kick,
            clocks,
            clock,
        })
    }

    pub fn clock_id(&self) -> u64 {
        self.clock_id
    }

    pub fn master_clock_id(&self) -> u64 {
        self.clock.master_clock_id()
    }

    pub fn master_now_ns(&self) -> u64 {
        self.clock.master_now_ns()
    }

    pub fn clock_handle(&self) -> PtpClock {
        self.clock.clone()
    }

    pub fn clock_handle_for(&self, receiver_ip: IpAddr) -> Result<PtpClock, PtpEngineError> {
        let receiver = match receiver_ip {
            IpAddr::V4(ip) => ip,
            IpAddr::V6(_) => return Err(PtpEngineError::Ipv4Required),
        };
        Ok(PtpClock {
            local_clock_id: self.clock_id,
            receiver: Some(receiver),
            clocks: Arc::clone(&self.clocks),
        })
    }

    pub fn register_receiver(
        &self,
        receiver_ip: IpAddr,
        follow_receiver_clock: bool,
    ) -> Result<PtpClock, PtpEngineError> {
        let receiver = match receiver_ip {
            IpAddr::V4(ip) => ip,
            IpAddr::V6(_) => return Err(PtpEngineError::Ipv4Required),
        };
        self.clocks
            .lock()
            .map_err(|_| PtpEngineError::PeerLimit { max: PTP_MAX_PEERS })?
            .register(receiver, follow_receiver_clock)?;

        // Shared-daemon source parity: registration also places the receiver
        // into the active timing peer set before Session SETUP.
        if let Ok(mut peers) = self.peers.lock() {
            if !peers.contains(&receiver) {
                peers.push(receiver);
            }
            self.peer_kick.store(true, Ordering::SeqCst);
        }
        self.clock_handle_for(IpAddr::V4(receiver))
    }

    pub fn remove_receiver(&self, receiver_ip: IpAddr) {
        let IpAddr::V4(receiver) = receiver_ip else {
            return;
        };
        if let Ok(mut clocks) = self.clocks.lock() {
            clocks.remove(receiver);
        }
        if let Ok(mut peers) = self.peers.lock() {
            peers.retain(|peer| *peer != receiver);
        }
    }

    pub fn follow_locked(&self) -> bool {
        self.clock.is_follow_locked()
    }

    pub fn peer_exchange(&self) -> Option<PtpExchange> {
        self.clock.exchange()
    }

    pub fn peer_exchange_for(&self, receiver_ip: IpAddr) -> Option<PtpExchange> {
        self.clock_handle_for(receiver_ip).ok()?.exchange()
    }

    pub fn settle(&self, timeout: Duration) {
        self.settle_receiver(IpAddr::V4(self.primary_receiver), timeout);
    }

    pub fn settle_receiver(&self, receiver_ip: IpAddr, timeout: Duration) {
        let Ok(clock) = self.clock_handle_for(receiver_ip) else {
            return;
        };
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let decided = match clock.receiver {
                Some(receiver) => clock
                    .clocks
                    .lock()
                    .ok()
                    .and_then(|mut clocks| {
                        clocks.state_mut(receiver).map(|state| {
                            !state.follow_enabled || state.follow_locked(now_unix_ns())
                        })
                    })
                    .unwrap_or(true),
                None => true,
            };
            if decided {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn set_peers(&self, peers: &[IpAddr]) {
        if let Ok(mut slot) = self.peers.lock() {
            slot.clear();
            for peer in peers {
                if let IpAddr::V4(ip) = peer {
                    if !slot.contains(ip) {
                        slot.push(*ip);
                    }
                }
            }
            self.peer_kick.store(!slot.is_empty(), Ordering::SeqCst);
        }
    }

    /// Register additional receivers on an already-running PTP engine.
    /// The primary source uses one host-wide shared PTP daemon for multi-room;
    /// adding a member must not evict peers that are already synchronized.
    pub fn add_peers(&self, peers: &[IpAddr]) {
        if let Ok(mut slot) = self.peers.lock() {
            for peer in peers {
                if let IpAddr::V4(ip) = peer {
                    if !slot.contains(ip) {
                        slot.push(*ip);
                    }
                }
            }
            self.peer_kick.store(!slot.is_empty(), Ordering::SeqCst);
        }
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for PtpEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_ptp_loop(
    event: UdpSocket,
    general: UdpSocket,
    clock_id: u64,
    running: Arc<AtomicBool>,
    peers: Arc<Mutex<Vec<Ipv4Addr>>>,
    peer_kick: Arc<AtomicBool>,
    clocks: Arc<Mutex<ReceiverClockTable>>,
) {
    let mut sync_seq = 0u16;
    let mut announce_seq = 0u16;
    let mut signaling_seq = 0u16;
    let mut next_sync = Instant::now();
    let mut next_announce = Instant::now();
    let mut next_signaling = Instant::now();

    while running.load(Ordering::SeqCst) {
        let now = Instant::now();
        let kick = peer_kick.swap(false, Ordering::SeqCst);

        if kick || now >= next_sync {
            send_sync_pair(&event, &general, &peers, &clocks, clock_id, sync_seq);
            sync_seq = sync_seq.wrapping_add(1);
            next_sync = now + Duration::from_millis(125);
        }

        if kick || now >= next_announce {
            let packet = build_announce(clock_id, announce_seq);
            send_ptp(&general, GENERAL_PORT, &packet, &peers, &clocks);
            announce_seq = announce_seq.wrapping_add(1);
            next_announce = now + Duration::from_secs(1);
        }

        if kick || now >= next_signaling {
            let packet = build_sender_signaling(clock_id, signaling_seq);
            send_ptp(&general, GENERAL_PORT, &packet, &peers, &clocks);
            signaling_seq = signaling_seq.wrapping_add(1);
            next_signaling = now + Duration::from_secs(1);
        }

        drain_socket(&event, &event, &general, clock_id, &clocks);
        drain_socket(&general, &event, &general, clock_id, &clocks);
        thread::sleep(Duration::from_millis(2));
    }
}

fn send_ptp(
    socket: &UdpSocket,
    port: u16,
    packet: &[u8],
    peers: &Arc<Mutex<Vec<Ipv4Addr>>>,
    clocks: &Arc<Mutex<ReceiverClockTable>>,
) {
    // v0.5.4 source behavior: followed receivers are excluded from the
    // unicast peer snapshot. When no unicast destination remains, the sender
    // uses its normal multicast fallback.
    let snapshot: Vec<Ipv4Addr> = peers
        .lock()
        .map(|peers| {
            let clocks = clocks.lock().ok();
            peers
                .iter()
                .copied()
                .filter(|peer| {
                    !clocks
                        .as_ref()
                        .is_some_and(|table| table.is_followed(*peer))
                })
                .collect()
        })
        .unwrap_or_default();

    if snapshot.is_empty() {
        let _ = socket.send_to(packet, (MCAST_ADDR, port));
    } else {
        for peer in snapshot {
            let _ = socket.send_to(packet, (peer, port));
        }
    }
}

fn drain_socket(
    socket: &UdpSocket,
    event: &UdpSocket,
    general: &UdpSocket,
    clock_id: u64,
    clocks: &Arc<Mutex<ReceiverClockTable>>,
) {
    let mut buf = [0u8; 1536];
    loop {
        let (n, src) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        };
        if n < HDR_LEN {
            continue;
        }

        let msg_type = buf[0] & 0x0F;
        let rx_ns = now_unix_ns();

        if n >= 28 {
            let src_clock = u64::from_be_bytes(buf[20..28].try_into().unwrap());
            if src_clock == clock_id {
                continue;
            }
        }

        let src_v4 = match src.ip() {
            IpAddr::V4(ip) => Some(ip),
            IpAddr::V6(_) => None,
        };
        let followed = src_v4.is_some_and(|receiver| {
            clocks
                .lock()
                .ok()
                .is_some_and(|table| table.is_followed(receiver))
        });

        if followed {
            let receiver = src_v4.expect("followed receiver is IPv4");
            match msg_type {
                MSG_ANNOUNCE => {
                    follow_announce(&buf[..n], rx_ns, receiver, clocks);
                    continue;
                }
                MSG_SYNC => {
                    follow_sync(&buf[..n], rx_ns, false, receiver, clocks);
                    track_exchange(src_v4, rx_ns, clocks);
                    continue;
                }
                MSG_FOLLOW_UP => {
                    follow_sync(&buf[..n], rx_ns, true, receiver, clocks);
                    continue;
                }
                _ => {}
            }
        }

        match msg_type {
            MSG_DELAY_REQ => {
                track_exchange(src_v4, rx_ns, clocks);
                let packet = build_delay_resp(&buf[..n], clock_id, rx_ns);
                let _ = general.send_to(&packet, SocketAddr::new(src.ip(), GENERAL_PORT));
            }
            MSG_PDELAY_REQ => {
                track_exchange(src_v4, rx_ns, clocks);
                let (resp, follow) = build_pdelay_resp_pair(&buf[..n], clock_id, rx_ns);
                let _ = event.send_to(&resp, SocketAddr::new(src.ip(), EVENT_PORT));
                let _ = general.send_to(&follow, SocketAddr::new(src.ip(), GENERAL_PORT));
            }
            MSG_SIGNALING => {
                if let Some(packet) = build_signaling_grant(&buf[..n], clock_id) {
                    let _ = general.send_to(&packet, SocketAddr::new(src.ip(), GENERAL_PORT));
                }
            }
            _ => {}
        }
    }
}

fn track_exchange(
    src: Option<Ipv4Addr>,
    rx_ns: u64,
    clocks: &Arc<Mutex<ReceiverClockTable>>,
) {
    let Some(src) = src else { return; };
    if let Ok(mut table) = clocks.lock() {
        let Some(state) = table.state_mut(src) else {
            return;
        };
        let ex = &mut state.exchange;
        if ex.count == 0 || rx_ns.saturating_sub(ex.last_ns) > EXCHANGE_GAP_NS {
            ex.first_ns = rx_ns;
            ex.third_ns = 0;
            ex.last_ns = rx_ns;
            ex.count = 1;
        } else {
            ex.last_ns = rx_ns;
            ex.count = ex.count.saturating_add(1);
            if ex.count == 3 {
                ex.third_ns = rx_ns;
            }
        }
    }
}

fn read_ptp_timestamp(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < 10 { return None; }
    let sec = ((bytes[0] as u64) << 40)
        | ((bytes[1] as u64) << 32)
        | ((bytes[2] as u64) << 24)
        | ((bytes[3] as u64) << 16)
        | ((bytes[4] as u64) << 8)
        | bytes[5] as u64;
    let ns = u32::from_be_bytes(bytes[6..10].try_into().ok()?) as u64;
    Some(sec.saturating_mul(1_000_000_000).saturating_add(ns))
}

fn correction_ns(packet: &[u8]) -> i64 {
    if packet.len() < 16 { return 0; }
    let raw = u64::from_be_bytes(packet[8..16].try_into().unwrap()) as i64;
    raw / 65_536
}

fn fold_follow_offset(state: &mut ReceiverClockState, raw: i64) {
    match state.offset_ns {
        None => state.offset_ns = Some(raw),
        Some(old) => {
            let delta = raw - old;
            state.offset_ns = Some(if delta.abs() > OFFSET_SNAP_NS {
                raw
            } else {
                old + delta / OFFSET_EMA_DIV
            });
        }
    }
}

fn follow_announce(
    packet: &[u8],
    rx_ns: u64,
    receiver: Ipv4Addr,
    clocks: &Arc<Mutex<ReceiverClockTable>>,
) {
    if packet.len() < HDR_LEN + 30 {
        return;
    }
    let body = &packet[HDR_LEN..];
    let clock_id = u64::from_be_bytes(body[19..27].try_into().unwrap());
    if let Ok(mut table) = clocks.lock() {
        let Some(state) = table.state_mut(receiver) else {
            return;
        };
        if !state.follow_enabled {
            return;
        }
        if state.clock_id.is_some() && state.clock_id != Some(clock_id) {
            state.offset_ns = None;
            state.pending_sync_seq = None;
        }
        state.clock_id = Some(clock_id);
        state.last_ns = rx_ns;
    }
}

fn follow_sync(
    packet: &[u8],
    rx_ns: u64,
    follow_up: bool,
    receiver: Ipv4Addr,
    clocks: &Arc<Mutex<ReceiverClockTable>>,
) {
    if packet.len() < HDR_LEN + 10 {
        return;
    }
    let flags = u16::from_be_bytes([packet[6], packet[7]]);
    let seq = u16::from_be_bytes([packet[30], packet[31]]);
    let corr = correction_ns(packet);
    let Some(t1) = read_ptp_timestamp(&packet[HDR_LEN..HDR_LEN + 10]) else {
        return;
    };

    if let Ok(mut table) = clocks.lock() {
        let Some(state) = table.state_mut(receiver) else {
            return;
        };
        if !state.follow_enabled {
            return;
        }
        state.last_ns = rx_ns;
        if !follow_up {
            if flags & FLAG_TWO_STEP != 0 {
                state.pending_sync_seq = Some(seq);
                state.pending_sync_rx_ns = rx_ns;
                state.pending_sync_corr_ns = corr;
            } else {
                fold_follow_offset(state, t1 as i64 + corr - rx_ns as i64);
            }
        } else if state.pending_sync_seq == Some(seq) {
            let sample = t1 as i64
                + state.pending_sync_corr_ns
                + corr
                - state.pending_sync_rx_ns as i64;
            state.pending_sync_seq = None;
            fold_follow_offset(state, sample);
        }
    }
}

fn now_unix_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn write_header(
    out: &mut [u8],
    msg_type: u8,
    message_len: u16,
    flags: u16,
    clock_id: u64,
    sequence: u16,
    control: u8,
    log_interval: i8,
) {
    out[..HDR_LEN].fill(0);
    out[0] = 0x10 | (msg_type & 0x0F); // gPTP majorSdoId=1
    out[1] = 0x02; // PTP v2
    out[2..4].copy_from_slice(&message_len.to_be_bytes());
    out[4] = 0; // domain
    out[6..8].copy_from_slice(&flags.to_be_bytes());
    out[20..28].copy_from_slice(&clock_id.to_be_bytes());
    out[28..30].copy_from_slice(&0x8005u16.to_be_bytes());
    out[30..32].copy_from_slice(&sequence.to_be_bytes());
    out[32] = control;
    out[33] = log_interval as u8;
}

fn write_timestamp(out: &mut [u8], ns: u64) {
    let seconds = ns / 1_000_000_000;
    let nanos = (ns % 1_000_000_000) as u32;
    out[0] = (seconds >> 40) as u8;
    out[1] = (seconds >> 32) as u8;
    out[2] = (seconds >> 24) as u8;
    out[3] = (seconds >> 16) as u8;
    out[4] = (seconds >> 8) as u8;
    out[5] = seconds as u8;
    out[6..10].copy_from_slice(&nanos.to_be_bytes());
}

fn build_announce(clock_id: u64, sequence: u16) -> Vec<u8> {
    let len = HDR_LEN + 30 + 12;
    let mut out = vec![0u8; len];
    write_header(
        &mut out,
        MSG_ANNOUNCE,
        len as u16,
        FLAG_UNICAST | FLAG_PTP_TIMESCALE,
        clock_id,
        sequence,
        0,
        0,
    );
    let b = &mut out[HDR_LEN..];
    b[13] = 128;
    b[14] = 6;
    b[15] = 0x21;
    b[16..18].copy_from_slice(&0x436Au16.to_be_bytes());
    b[18] = 128;
    b[19..27].copy_from_slice(&clock_id.to_be_bytes());
    b[29] = 0x20;
    b[30..32].copy_from_slice(&0x0008u16.to_be_bytes());
    b[32..34].copy_from_slice(&8u16.to_be_bytes());
    b[34..42].copy_from_slice(&clock_id.to_be_bytes());
    out
}

fn send_sync_pair(
    event: &UdpSocket,
    general: &UdpSocket,
    peers: &Arc<Mutex<Vec<Ipv4Addr>>>,
    clocks: &Arc<Mutex<ReceiverClockTable>>,
    clock_id: u64,
    sequence: u16,
) {
    let slen = HDR_LEN + 10;
    let mut sync = vec![0u8; slen];
    write_header(
        &mut sync,
        MSG_SYNC,
        slen as u16,
        FLAG_UNICAST | FLAG_PTP_TIMESCALE | FLAG_TWO_STEP,
        clock_id,
        sequence,
        0,
        -3,
    );
    send_ptp(event, EVENT_PORT, &sync, peers, clocks);

    let egress = now_unix_ns();
    let flen = HDR_LEN + 10 + 32 + 20;
    let mut follow_packet = vec![0u8; flen];
    write_header(
        &mut follow_packet,
        MSG_FOLLOW_UP,
        flen as u16,
        FLAG_UNICAST | FLAG_PTP_TIMESCALE,
        clock_id,
        sequence,
        0,
        -3,
    );
    write_timestamp(&mut follow_packet[HDR_LEN..HDR_LEN + 10], egress);

    let mut o = HDR_LEN + 10;
    follow_packet[o..o + 4].copy_from_slice(&[0x00, 0x03, 0x00, 0x1C]);
    follow_packet[o + 4..o + 10].copy_from_slice(&[0x00, 0x80, 0xC2, 0x00, 0x00, 0x01]);
    o += 32;
    follow_packet[o..o + 4].copy_from_slice(&[0x00, 0x03, 0x00, 0x10]);
    follow_packet[o + 4..o + 10].copy_from_slice(&[0x00, 0x0D, 0x93, 0x00, 0x00, 0x04]);
    follow_packet[o + 10..o + 18].copy_from_slice(&clock_id.to_be_bytes());

    send_ptp(general, GENERAL_PORT, &follow_packet, peers, clocks);
}

fn build_delay_resp(req: &[u8], clock_id: u64, rx_ns: u64) -> Vec<u8> {
    let len = HDR_LEN + 20;
    let mut out = vec![0u8; len];
    let sequence = u16::from_be_bytes([req[30], req[31]]);
    write_header(
        &mut out,
        MSG_DELAY_RESP,
        len as u16,
        FLAG_UNICAST | FLAG_PTP_TIMESCALE | FLAG_TWO_STEP,
        clock_id,
        sequence,
        0,
        -3,
    );
    write_timestamp(&mut out[HDR_LEN..HDR_LEN + 10], rx_ns);
    if req.len() >= 30 {
        out[HDR_LEN + 10..HDR_LEN + 20].copy_from_slice(&req[20..30]);
    }
    out
}

fn build_pdelay_resp_pair(req: &[u8], clock_id: u64, rx_ns: u64) -> (Vec<u8>, Vec<u8>) {
    let len = HDR_LEN + 20;
    let sequence = u16::from_be_bytes([req[30], req[31]]);

    let mut resp = vec![0u8; len];
    write_header(
        &mut resp,
        MSG_PDELAY_RESP,
        len as u16,
        FLAG_UNICAST | FLAG_PTP_TIMESCALE | FLAG_TWO_STEP,
        clock_id,
        sequence,
        0,
        -3,
    );
    write_timestamp(&mut resp[HDR_LEN..HDR_LEN + 10], rx_ns);
    if req.len() >= 30 {
        resp[HDR_LEN + 10..HDR_LEN + 20].copy_from_slice(&req[20..30]);
    }

    let mut follow = vec![0u8; len];
    write_header(
        &mut follow,
        MSG_PDELAY_RESP_FUP,
        len as u16,
        FLAG_UNICAST | FLAG_PTP_TIMESCALE,
        clock_id,
        sequence,
        0,
        -3,
    );
    write_timestamp(&mut follow[HDR_LEN..HDR_LEN + 10], now_unix_ns());
    if req.len() >= 30 {
        follow[HDR_LEN + 10..HDR_LEN + 20].copy_from_slice(&req[20..30]);
    }

    (resp, follow)
}

fn build_signaling_grant(req: &[u8], clock_id: u64) -> Option<Vec<u8>> {
    if req.len() < HDR_LEN + 14 {
        return None;
    }
    let mut offset = HDR_LEN + 10;
    let mut grants = Vec::<[u8; 12]>::new();

    while offset + 4 <= req.len() && grants.len() < 8 {
        let tlv_type = u16::from_be_bytes([req[offset], req[offset + 1]]);
        let tlv_len = u16::from_be_bytes([req[offset + 2], req[offset + 3]]) as usize;
        if offset + 4 + tlv_len > req.len() {
            break;
        }

        if tlv_type == TLV_REQUEST_UNICAST && tlv_len >= 6 {
            let value = &req[offset + 4..offset + 4 + tlv_len];
            let mut grant = [0u8; 12];
            grant[0..2].copy_from_slice(&TLV_GRANT_UNICAST.to_be_bytes());
            grant[2..4].copy_from_slice(&8u16.to_be_bytes());
            grant[4] = value[0];
            grant[5] = value[1];
            let mut duration = u32::from_be_bytes([value[2], value[3], value[4], value[5]]);
            if duration == 0 {
                duration = 300;
            }
            grant[6..10].copy_from_slice(&duration.to_be_bytes());
            grant[10] = 0;
            grant[11] = 1;
            grants.push(grant);
        }

        offset += 4 + tlv_len;
    }

    if grants.is_empty() {
        return None;
    }

    let len = HDR_LEN + 10 + grants.len() * 12;
    let mut out = vec![0u8; len];
    let sequence = u16::from_be_bytes([req[30], req[31]]);
    write_header(
        &mut out,
        MSG_SIGNALING,
        len as u16,
        FLAG_UNICAST,
        clock_id,
        sequence,
        0x05,
        0x7F,
    );
    out[HDR_LEN..HDR_LEN + 10].copy_from_slice(&req[20..30]);
    let mut pos = HDR_LEN + 10;
    for grant in grants {
        out[pos..pos + 12].copy_from_slice(&grant);
        pos += 12;
    }
    Some(out)
}

fn build_sender_signaling(clock_id: u64, sequence: u16) -> Vec<u8> {
    let len = HDR_LEN + 10 + 26 + 36;
    let mut out = vec![0u8; len];
    write_header(
        &mut out,
        MSG_SIGNALING,
        len as u16,
        FLAG_UNICAST | FLAG_PTP_TIMESCALE,
        clock_id,
        sequence,
        0x05,
        -128,
    );
    let mut o = HDR_LEN + 10;
    out[o..o + 4].copy_from_slice(&[0x00, 0x03, 0x00, 0x16]);
    out[o + 4..o + 10].copy_from_slice(&[0x00, 0x0D, 0x93, 0x00, 0x00, 0x01]);
    out[o + 10..o + 14].copy_from_slice(&[0x00, 0x00, 0x03, 0x01]);
    o += 26;
    out[o..o + 4].copy_from_slice(&[0x00, 0x03, 0x00, 0x20]);
    out[o + 4..o + 10].copy_from_slice(&[0x00, 0x0D, 0x93, 0x00, 0x00, 0x05]);
    out[o + 10..o + 14].copy_from_slice(&[0x00, 0x00, 0x03, 0x01]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announce_matches_source_shape() {
        let p = build_announce(0x1122334455667788, 7);
        assert_eq!(p.len(), 76);
        assert_eq!(p[0], 0x1B);
        assert_eq!(p[1], 0x02);
        assert_eq!(u16::from_be_bytes([p[2], p[3]]), 76);
        assert_eq!(&p[20..28], &0x1122334455667788u64.to_be_bytes());
        assert_eq!(u16::from_be_bytes([p[30], p[31]]), 7);
        assert_eq!(&p[68..76], &0x1122334455667788u64.to_be_bytes());
    }

    #[test]
    fn receiver_follow_clocks_are_isolated_and_capped_at_eight() {
        let mut table = ReceiverClockTable::default();
        for n in 1..=PTP_MAX_PEERS as u8 {
            table
                .register(Ipv4Addr::new(192, 168, 50, n), true)
                .unwrap();
        }
        assert!(matches!(
            table.register(Ipv4Addr::new(192, 168, 50, 99), true),
            Err(PtpEngineError::PeerLimit { max: PTP_MAX_PEERS })
        ));

        let now = now_unix_ns();
        let first = Ipv4Addr::new(192, 168, 50, 1);
        let second = Ipv4Addr::new(192, 168, 50, 2);
        {
            let state = table.state_mut(first).unwrap();
            state.clock_id = Some(0x1111);
            state.offset_ns = Some(1_000);
            state.last_ns = now;
        }
        {
            let state = table.state_mut(second).unwrap();
            state.clock_id = Some(0x2222);
            state.offset_ns = Some(2_000);
            state.last_ns = now;
        }

        let clocks = Arc::new(Mutex::new(table));
        let first_clock = PtpClock {
            local_clock_id: 0xAAAA,
            receiver: Some(first),
            clocks: Arc::clone(&clocks),
        };
        let second_clock = PtpClock {
            local_clock_id: 0xAAAA,
            receiver: Some(second),
            clocks,
        };
        assert_eq!(first_clock.master_clock_id(), 0x1111);
        assert_eq!(second_clock.master_clock_id(), 0x2222);
    }

    #[test]
    fn delay_response_echoes_request_identity_and_sequence() {
        let mut req = vec![0u8; 54];
        req[20..30].copy_from_slice(&[1,2,3,4,5,6,7,8,0x80,0x05]);
        req[30..32].copy_from_slice(&0x1234u16.to_be_bytes());
        let p = build_delay_resp(&req, 9, 1_500_000_000);
        assert_eq!(p[0], 0x19);
        assert_eq!(u16::from_be_bytes([p[30], p[31]]), 0x1234);
        assert_eq!(&p[HDR_LEN + 10..HDR_LEN + 20], &req[20..30]);
    }
}
