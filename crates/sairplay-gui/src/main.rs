#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use eframe::egui;
use sairplay_engine::{
    Ap2PreflightClient, DeviceCatalog, DeviceRecord, DiscoveredService, DiscoveryEvent,
    LegacyGroupSession, LegacyMemberConfig, MdnsBrowser, NativeGroupMemberConfig, NativeGroupSession,
    NativeSession, NativeSessionConfig, RetransmitStats, Route, ServiceKind, VolumeSetResult,
    WasapiLoopbackCapture, ALAC_44100_16_2, ALAC_44100_24_2, ALAC_48000_16_2,
    ALAC_48000_24_2,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::IpAddr;
use std::path::PathBuf;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlaybackUiState {
    Idle,
    Connecting(String),
    Playing(String),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaybackMode {
    Single,
    MultiRoom,
    StereoPair,
}

enum ActiveSession {
    Single(NativeSession),
    Group(NativeGroupSession),
    Legacy(LegacyGroupSession),
}

impl ActiveSession {
    fn audio_running(&self) -> bool {
        match self {
            Self::Single(session) => session.audio_running(),
            Self::Group(session) => session.audio_running(),
            Self::Legacy(session) => session.is_running(),
        }
    }

    fn audio_error(&self) -> Option<String> {
        match self {
            Self::Single(session) => session.audio_error(),
            Self::Group(session) => session.audio_error(),
            Self::Legacy(session) => session.last_error(),
        }
    }

    fn audio_format(&self) -> Option<sairplay_engine::Ap2AudioFormat> {
        match self {
            Self::Single(session) => Some(session.audio_format()),
            Self::Group(session) => session.audio_format(),
            Self::Legacy(_) => None,
        }
    }

    fn drain_startup_events(&self) -> Vec<String> {
        match self {
            Self::Single(session) => session.drain_startup_events(),
            Self::Group(session) => session.drain_startup_events(),
            Self::Legacy(session) => session.drain_startup_events(),
        }
    }

    fn retransmit_stats(&self) -> RetransmitStats {
        match self {
            Self::Single(session) => session.retransmit_stats(),
            Self::Group(session) => session.retransmit_stats(),
            Self::Legacy(_) => RetransmitStats::default(),
        }
    }

    fn feedback_running(&self) -> bool {
        match self {
            Self::Single(session) => session.feedback_running(),
            Self::Group(session) => session.feedback_running(),
            Self::Legacy(session) => session.is_running(),
        }
    }

    fn feedback_error(&self) -> Option<String> {
        match self {
            Self::Single(session) => session.feedback_error(),
            Self::Group(session) => session.feedback_error(),
            Self::Legacy(_) => None,
        }
    }

    fn volume_controls(&self) -> Vec<sairplay_engine::NativeVolumeControl> {
        match self {
            Self::Single(session) => vec![session.volume_control()],
            Self::Group(session) => session.volume_controls(),
            Self::Legacy(_) => Vec::new(),
        }
    }

    fn initial_volume_results(&self) -> Vec<(String, VolumeSetResult)> {
        match self {
            Self::Single(session) => session
                .initial_volume_result()
                .map(|result| vec![("receiver".to_owned(), result)])
                .unwrap_or_default(),
            Self::Group(session) => session.initial_volume_results(),
            Self::Legacy(_) => Vec::new(),
        }
    }
}

struct ConnectSuccess {
    session: ActiveSession,
    active_fullnames: BTreeSet<String>,
    label: String,
    mode: PlaybackMode,
}

struct MembershipAdded {
    members: Vec<(String, NativeSession)>,
}

struct HiresProbeResult {
    fullname: String,
    advertised: Option<bool>,
    realtime_mask: Option<u64>,
    buffered_mask: Option<u64>,
}

enum LegacyPairingResult {
    Success {
        device_key: String,
        device_name: String,
        secret: String,
    },
    Failed {
        device_name: String,
        error: String,
    },
}


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiLanguage {
    Vi,
    En,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum DeviceArtwork {
    HomePodMiniWhite,
    HomePodMiniBlack,
    HomePodWhite,
    HomePodBlack,
    HomePodMiniPairWhite,
    HomePodMiniPairBlack,
    HomePodMiniPairMixed,
    HomePodPairWhite,
    HomePodPairBlack,
    HomePodPairMixed,
    MacBook,
    MacMini,
    MusicServer,
    AirportExpress,
    Tv,
    AppleTv,
    AirplaySpeakers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusTone {
    Green,
    Blue,
    Orange,
    Red,
    Gray,
}

impl StatusTone {
    fn colors(self) -> (egui::Color32, egui::Color32, egui::Color32) {
        match self {
            Self::Green => (
                egui::Color32::from_rgb(223, 246, 232),
                egui::Color32::from_rgb(34, 197, 94),
                egui::Color32::from_rgb(22, 128, 60),
            ),
            Self::Blue => (
                egui::Color32::from_rgb(224, 239, 255),
                egui::Color32::from_rgb(22, 119, 255),
                egui::Color32::from_rgb(15, 96, 210),
            ),
            Self::Orange => (
                egui::Color32::from_rgb(252, 239, 216),
                egui::Color32::from_rgb(245, 128, 32),
                egui::Color32::from_rgb(217, 104, 6),
            ),
            Self::Red => (
                egui::Color32::from_rgb(255, 226, 231),
                egui::Color32::from_rgb(255, 55, 95),
                egui::Color32::from_rgb(225, 29, 72),
            ),
            Self::Gray => (
                egui::Color32::from_rgb(233, 239, 247),
                egui::Color32::from_rgb(128, 148, 182),
                egui::Color32::from_rgb(96, 119, 155),
            ),
        }
    }
}

struct UiTheme;

impl UiTheme {
    fn bg() -> egui::Color32 { egui::Color32::from_rgb(244, 248, 255) }
    fn surface() -> egui::Color32 { egui::Color32::from_rgb(251, 253, 255) }
    fn surface_soft() -> egui::Color32 { egui::Color32::from_rgb(247, 250, 255) }
    fn border() -> egui::Color32 { egui::Color32::from_rgb(216, 229, 244) }
    fn guide() -> egui::Color32 { egui::Color32::from_rgb(226, 234, 244) }
    fn border_hover() -> egui::Color32 { egui::Color32::from_rgb(159, 204, 255) }
    fn border_active() -> egui::Color32 { egui::Color32::from_rgb(22, 119, 255) }
    fn text() -> egui::Color32 { egui::Color32::from_rgb(15, 30, 58) }
    fn text_soft() -> egui::Color32 { egui::Color32::from_rgb(107, 129, 166) }
    fn blue() -> egui::Color32 { egui::Color32::from_rgb(22, 119, 255) }
    fn blue_hover() -> egui::Color32 { egui::Color32::from_rgb(42, 134, 255) }
    fn blue_pressed() -> egui::Color32 { egui::Color32::from_rgb(15, 106, 232) }
    fn green() -> egui::Color32 { egui::Color32::from_rgb(34, 197, 94) }
    fn amber() -> egui::Color32 { egui::Color32::from_rgb(245, 128, 32) }
    fn red() -> egui::Color32 { egui::Color32::from_rgb(255, 55, 95) }
}

struct SairplayApp {
    log: Vec<String>,
    catalog: DeviceCatalog,
    discovery: Option<MdnsBrowser>,
    discovery_rx: Option<Receiver<DiscoveryEvent>>,
    selected_fullnames: BTreeSet<String>,
    selected_stereo_pair: Option<BTreeSet<String>>,
    active_fullnames: BTreeSet<String>,
    active_mode: Option<PlaybackMode>,
    playback: PlaybackUiState,
    connect_rx: Option<Receiver<Result<ConnectSuccess, String>>>,
    membership_rx: Option<Receiver<Result<MembershipAdded, String>>>,
    membership_pending: BTreeSet<String>,
    session: Option<ActiveSession>,
    initial_volume_text: String,
    volume_rx: Option<Receiver<Result<Vec<VolumeSetResult>, String>>>,
    pending_volume: Option<u8>,
    legacy_secrets: BTreeMap<String, String>,
    hires_overrides: BTreeMap<String, bool>,
    hires_capabilities: BTreeMap<String, bool>,
    hires_probe_pending: BTreeSet<String>,
    hires_probe_tx: mpsc::Sender<HiresProbeResult>,
    hires_probe_rx: Receiver<HiresProbeResult>,
    pairing_rx: Option<Receiver<LegacyPairingResult>>,
    pairing_pin_tx: Option<SyncSender<String>>,
    pairing_open: bool,
    pairing_pin: String,
    pairing_name: String,
    pairing_pin_sent: bool,
    pairing_retry_start: bool,
    native_retry_available: bool,
    language: UiLanguage,
    multiroom_enabled: bool,
    activation_open: bool,
    activation_key: String,
    last_feedback_error: Option<String>,
    last_retransmit_stats: RetransmitStats,
    hires_quality_warning_open: bool,
    hires_quality_warning_shown: bool,
    hires_quality_warning_text: String,
    show_multiroom_info: bool,
    show_pair_info: bool,
    device_textures: HashMap<DeviceArtwork, egui::TextureHandle>,
    device_textures_initialized: bool,
}

impl Default for SairplayApp {
    fn default() -> Self {
        let (discovery, discovery_rx, mut log) = match MdnsBrowser::start() {
            Ok((browser, rx)) => (
                Some(browser),
                Some(rx),
                vec![
                    "SAirplay2 initialized.".into(),
                    "Scanning AirPlay receivers...".into(),
                ],
            ),
            Err(err) => (
                None,
                None,
                vec![
                    "SAirplay2 initialized.".into(),
                    format!("mDNS discovery unavailable: {err}"),
                ],
            ),
        };

        let (hires_probe_tx, hires_probe_rx) = mpsc::channel();
        let initial_volume_text = load_saved_volume()
            .map(|volume| volume.to_string())
            .unwrap_or_else(|| "50".to_owned());

        Self {
            log: {
                log.shrink_to_fit();
                log
            },
            catalog: DeviceCatalog::default(),
            discovery,
            discovery_rx,
            selected_fullnames: BTreeSet::new(),
            selected_stereo_pair: None,
            active_fullnames: BTreeSet::new(),
            active_mode: None,
            playback: PlaybackUiState::Idle,
            connect_rx: None,
            membership_rx: None,
            membership_pending: BTreeSet::new(),
            session: None,
            initial_volume_text,
            volume_rx: None,
            pending_volume: None,
            legacy_secrets: BTreeMap::new(),
            hires_overrides: BTreeMap::new(),
            hires_capabilities: BTreeMap::new(),
            hires_probe_pending: BTreeSet::new(),
            hires_probe_tx,
            hires_probe_rx,
            pairing_rx: None,
            pairing_pin_tx: None,
            pairing_open: false,
            pairing_pin: String::new(),
            pairing_name: String::new(),
            pairing_pin_sent: false,
            pairing_retry_start: false,
            native_retry_available: true,
            language: UiLanguage::Vi,
            multiroom_enabled: false,
            activation_open: false,
            activation_key: String::new(),
            last_feedback_error: None,
            last_retransmit_stats: RetransmitStats::default(),
            hires_quality_warning_open: false,
            hires_quality_warning_shown: false,
            hires_quality_warning_text: String::new(),
            show_multiroom_info: false,
            show_pair_info: false,
            device_textures: HashMap::new(),
            device_textures_initialized: false,
        }
    }
}

impl SairplayApp {
    fn start_hires_probe(&mut self, service: &DiscoveredService) {
        if service.kind != ServiceKind::AirPlay
            || !service.txt.supports_airplay2()
            || self.hires_capabilities.contains_key(&service.fullname)
            || !self.hires_probe_pending.insert(service.fullname.clone())
        {
            return;
        }

        let fullname = service.fullname.clone();
        let host = preferred_service_address(service);
        let port = service.port;
        let tx = self.hires_probe_tx.clone();

        thread::Builder::new()
            .name("sairplay-hires-probe".into())
            .spawn(move || {
                let result = Ap2PreflightClient::new(
                    "A1B2C3D4E5F60708",
                    "123456789",
                )
                .get_info(&host, port)
                .ok();

                let advertised = result
                    .as_ref()
                    .map(|result| result.info.advertises_hires());
                let realtime_mask = result
                    .as_ref()
                    .and_then(|result| result.info.realtime.known.then_some(result.info.realtime.mask));
                let buffered_mask = result
                    .as_ref()
                    .and_then(|result| result.info.buffered.known.then_some(result.info.buffered.mask));

                let _ = tx.send(HiresProbeResult {
                    fullname,
                    advertised,
                    realtime_mask,
                    buffered_mask,
                });
            })
            .expect("failed to spawn hi-res capability probe");
    }

    fn pump_hires_probes(&mut self) {
        while let Ok(result) = self.hires_probe_rx.try_recv() {
            self.hires_probe_pending.remove(&result.fullname);
            match result.advertised {
                Some(advertised) => {
                    self.hires_capabilities
                        .insert(result.fullname.clone(), advertised);

                    let union_mask =
                        result.realtime_mask.unwrap_or(0) | result.buffered_mask.unwrap_or(0);
                    let formats = [
                        ("44.1/16", ALAC_44100_16_2),
                        ("44.1/24", ALAC_44100_24_2),
                        ("48/16", ALAC_48000_16_2),
                        ("48/24", ALAC_48000_24_2),
                    ]
                    .into_iter()
                    .filter_map(|(name, bit)| ((union_mask & bit) != 0).then_some(name))
                    .collect::<Vec<_>>();

                    self.log.push(format!(
                        "{}: /info formats = {} · realtime_mask={} · buffered_mask={}.",
                        result.fullname,
                        if formats.is_empty() {
                            "none of 44.1/16, 44.1/24, 48/16, 48/24".to_owned()
                        } else {
                            formats.join(", ")
                        },
                        result
                            .realtime_mask
                            .map(|mask| format!("0x{mask:016X}"))
                            .unwrap_or_else(|| "unknown".into()),
                        result
                            .buffered_mask
                            .map(|mask| format!("0x{mask:016X}"))
                            .unwrap_or_else(|| "unknown".into()),
                    ));
                    self.log.push(format!(
                        "{}: /info 24-bit capability = {}.",
                        result.fullname,
                        if advertised { "advertised" } else { "not advertised" }
                    ));
                }
                None => {
                    self.log.push(format!(
                        "{}: /info capability probe unavailable; keeping baseline policy until next discovery.",
                        result.fullname
                    ));
                }
            }
        }
    }

    fn pump_discovery(&mut self) {
        let Some(rx) = &self.discovery_rx else {
            return;
        };

        // Drain first so processing an event may mutate other app state
        // (including starting an async /info capability probe) without holding
        // an immutable borrow of discovery_rx across the loop body.
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        for event in events {
            match event {
                DiscoveryEvent::Upsert(service) => {
                    let kind = match service.kind {
                        ServiceKind::AirPlay => "AirPlay",
                        ServiceKind::Raop => "RAOP",
                    };
                    if service.kind == ServiceKind::AirPlay {
                        self.log.push(format!(
                            "mDNS {kind}: {} @ {}:{} · model={}",
                            service.display_name,
                            service.host,
                            service.port,
                            service.txt.model.as_deref().unwrap_or("-"),
                        ));
                    } else {
                        self.log.push(format!(
                            "mDNS {kind}: {} @ {}:{}",
                            service.display_name, service.host, service.port
                        ));
                    }
                    if service.kind == ServiceKind::AirPlay {
                        self.start_hires_probe(&service);
                    }
                    self.catalog.upsert(service);
                }
                DiscoveryEvent::Removed { kind, fullname } => {
                    self.catalog.remove(kind, &fullname);
                    if kind == ServiceKind::AirPlay {
                        self.hires_capabilities.remove(&fullname);
                        self.hires_probe_pending.remove(&fullname);
                    }
                    self.selected_fullnames.remove(&fullname);
                    if self
                        .selected_stereo_pair
                        .as_ref()
                        .is_some_and(|pair| pair.contains(&fullname))
                    {
                        self.selected_stereo_pair = None;
                    }
                    self.active_fullnames.remove(&fullname);
                    self.log.push(format!("mDNS removed: {fullname}"));
                }
                DiscoveryEvent::Error(err) => {
                    self.log.push(format!("mDNS error: {err}"));
                }
            }
        }
    }

    fn rescan_devices(&mut self) {
        self.discovery.take();
        self.discovery_rx = None;
        self.catalog = DeviceCatalog::default();
        self.hires_capabilities.clear();
        self.hires_probe_pending.clear();
        self.selected_fullnames.clear();
        self.selected_stereo_pair = None;
        self.log.push("Manual rescan requested from app logo.".into());

        match MdnsBrowser::start() {
            Ok((browser, rx)) => {
                self.discovery = Some(browser);
                self.discovery_rx = Some(rx);
                self.log.push("Scanning AirPlay receivers...".into());
            }
            Err(err) => {
                self.log.push(format!("mDNS discovery unavailable: {err}"));
            }
        }
    }

    fn t(&self, vi: &'static str, en: &'static str) -> &'static str {
        match self.language {
            UiLanguage::Vi => vi,
            UiLanguage::En => en,
        }
    }

    fn hires_enabled_for_fullname(&self, fullname: &str) -> bool {
        // Product policy: 24-bit is explicit opt-in for every receiver.
        // Capability controls whether the switch is offered; capability alone
        // never enables the high-resolution path.
        self.hires_overrides.get(fullname).copied().unwrap_or(false)
    }

    fn pump_connect_result(&mut self) {
        let Some(rx) = &self.connect_rx else {
            return;
        };

        match rx.try_recv() {
            Ok(Ok(success)) => {
                for (member, volume) in success.session.initial_volume_results() {
                    self.log.push(format!(
                        "{member}: receiver volume {}% = {:.2} dB · RTSP {}.",
                        volume.percent, volume.db, volume.status
                    ));
                }

                if success.session.audio_running() {
                    if let Some(format) = success.session.audio_format() {
                        let rate = if format.sample_rate == 44_100 {
                            "44.1".to_owned()
                        } else {
                            format.sample_rate.to_string()
                        };
                        self.log.push(format!(
                            "{}: negotiated stream format = ALAC {}-bit / {} kHz.",
                            success.label,
                            format.bit_depth,
                            rate
                        ));
                    }
                    self.log.push(format!(
                        "{}: transport Ready, Windows audio running on {} receiver(s).",
                        success.label,
                        success.active_fullnames.len()
                    ));
                    self.last_retransmit_stats = success.session.retransmit_stats();
                    self.hires_quality_warning_open = false;
                    self.hires_quality_warning_shown = false;
                    self.hires_quality_warning_text.clear();
                    self.active_fullnames = success.active_fullnames;
                    self.active_mode = Some(success.mode);
                    self.playback = PlaybackUiState::Playing(success.label);
                    self.session = Some(success.session);
                } else {
                    let message =
                        "native transport returned without a running Windows audio path".to_owned();
                    self.log.push(message.clone());
                    self.active_fullnames.clear();
                    self.playback = PlaybackUiState::Error(message);
                }
                self.connect_rx = None;
            }
            Ok(Err(error)) => {
                self.log.push(format!("Connect failed: {error}"));
                self.active_fullnames.clear();
                self.active_mode = None;
                self.playback = PlaybackUiState::Error(error);
                self.connect_rx = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.log.push("Connect worker ended unexpectedly.".into());
                self.active_fullnames.clear();
                self.active_mode = None;
                self.playback =
                    PlaybackUiState::Error("Connect worker ended unexpectedly".into());
                self.connect_rx = None;
            }
        }
    }

    fn pump_membership_result(&mut self) {
        let Some(rx) = &self.membership_rx else {
            return;
        };

        match rx.try_recv() {
            Ok(Ok(added)) => {
                let mut adopted = 0usize;
                if let Some(ActiveSession::Group(group)) = self.session.as_mut() {
                    for (fullname, session) in added.members {
                        group.adopt_member(fullname.clone(), session);
                        self.active_fullnames.insert(fullname.clone());
                        self.selected_fullnames.insert(fullname);
                        adopted += 1;
                    }
                }
                self.log.push(format!(
                    "MultiRoom live join complete: {adopted} receiver(s) added without stopping the running group."
                ));
                self.membership_pending.clear();
                self.membership_rx = None;
            }
            Ok(Err(error)) => {
                self.log.push(format!("MultiRoom live join failed: {error}"));
                for fullname in std::mem::take(&mut self.membership_pending) {
                    self.selected_fullnames.remove(&fullname);
                }
                self.membership_rx = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.log.push("MultiRoom membership worker ended unexpectedly.".into());
                for fullname in std::mem::take(&mut self.membership_pending) {
                    self.selected_fullnames.remove(&fullname);
                }
                self.membership_rx = None;
            }
        }
    }

    fn request_live_add(&mut self, members: &[String]) {
        if self.membership_rx.is_some() {
            return;
        }
        let Some(ActiveSession::Group(group)) = self.session.as_ref() else {
            return;
        };
        let Some(join_handle) = group.join_handle() else {
            self.log.push("MultiRoom live join unavailable: audio worker is not running.".into());
            return;
        };

        let initial_volume = match parse_volume_text(&self.initial_volume_text) {
            Ok(volume) => volume,
            Err(message) => {
                self.log.push(message);
                return;
            }
        };

        let devices = self.catalog.devices().to_vec();
        let mut requests = Vec::<(String, NativeSessionConfig)>::new();
        for fullname in members {
            if self.active_fullnames.contains(fullname) {
                continue;
            }
            let Some(device) = devices.iter().find(|device| {
                device
                    .airplay
                    .as_ref()
                    .is_some_and(|service| service.fullname == *fullname)
            }) else {
                self.log.push(format!("Live join skipped: {fullname} is no longer discovered."));
                continue;
            };
            if device.route(false, false) != Route::AirPlay2Native {
                self.log.push(format!(
                    "Live join skipped: {} is not a native AirPlay 2 route.",
                    device.display_name
                ));
                continue;
            }
            let hires_override = self.hires_overrides.get(fullname).copied();
            match native_config_for_device(device, initial_volume, hires_override) {
                Ok(config) => requests.push((fullname.clone(), config)),
                Err(error) => self.log.push(error),
            }
        }

        if requests.is_empty() {
            return;
        }

        self.membership_pending = requests
            .iter()
            .map(|(fullname, _)| fullname.clone())
            .collect();
        self.log.push(format!(
            "MultiRoom live join: connecting {} receiver(s) while the current group keeps playing.",
            requests.len()
        ));

        let (tx, rx) = mpsc::sync_channel(1);
        self.membership_rx = Some(rx);
        thread::Builder::new()
            .name("sairplay-live-join".into())
            .spawn(move || {
                let mut added = Vec::<(String, NativeSession)>::new();
                for (fullname, config) in requests {
                    match join_handle.connect_member(fullname.clone(), config) {
                        Ok(member) => added.push(member),
                        Err(error) => {
                            for (joined, _) in &added {
                                let _ = join_handle.remove_audio_member(joined.clone());
                            }
                            let _ = tx.send(Err(error.to_string()));
                            return;
                        }
                    }
                }
                let _ = tx.send(Ok(MembershipAdded { members: added }));
            })
            .expect("failed to spawn MultiRoom live-join worker");
    }

    fn remove_live_members(&mut self, members: &[String]) {
        let active_to_remove: Vec<String> = members
            .iter()
            .filter(|fullname| self.active_fullnames.contains(*fullname))
            .cloned()
            .collect();
        if active_to_remove.is_empty() {
            return;
        }

        if active_to_remove.len() >= self.active_fullnames.len() {
            self.stop_playback();
            return;
        }

        let Some(ActiveSession::Group(group)) = self.session.as_mut() else {
            return;
        };

        for fullname in active_to_remove {
            match group.remove_member(&fullname) {
                Ok(()) => {
                    self.active_fullnames.remove(&fullname);
                    self.log.push(format!(
                        "{fullname}: removed from MultiRoom while the remaining receivers keep playing."
                    ));
                }
                Err(error) => {
                    self.log.push(format!("{fullname}: remove from MultiRoom failed: {error}"));
                    self.selected_fullnames.insert(fullname);
                }
            }
        }
    }

    fn pump_volume_result(&mut self) {
        let Some(rx) = &self.volume_rx else {
            return;
        };

        let finished = match rx.try_recv() {
            Ok(Ok(results)) => {
                for result in results {
                    self.log.push(format!(
                        "Receiver volume {}% = {:.2} dB · RTSP {}.",
                        result.percent, result.db, result.status
                    ));
                }
                true
            }
            Ok(Err(error)) => {
                self.log.push(format!("Volume update failed: {error}"));
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.log.push("Volume worker ended unexpectedly.".into());
                true
            }
        };

        if finished {
            self.volume_rx = None;
            if let Some(volume) = self.pending_volume.take() {
                self.apply_volume_value(volume);
            }
        }
    }

    fn apply_volume_value(&mut self, volume: u8) {
        if self.volume_rx.is_some() {
            self.pending_volume = Some(volume);
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };

        let controls = session.volume_controls();
        let (tx, rx) = mpsc::sync_channel(1);
        self.volume_rx = Some(rx);
        thread::Builder::new()
            .name("sairplay-volume".into())
            .spawn(move || {
                let result = controls
                    .into_iter()
                    .map(|control| control.set(volume).map_err(|e| format!("{e:?}")))
                    .collect::<Result<Vec<_>, _>>();
                let _ = tx.send(result);
            })
            .expect("failed to spawn volume worker");
    }

    fn legacy_pairing_key(device: &DeviceRecord) -> Option<String> {
        device
            .raop
            .as_ref()
            .or(device.airplay.as_ref())
            .map(|service| service.fullname.clone())
    }

    fn legacy_pairing_required(&self, device: &DeviceRecord) -> bool {
        let Some(service) = device.airplay.as_ref().or(device.raop.as_ref()) else {
            return false;
        };

        // Follow the source route semantics: full legacy PIN pairing is only
        // demanded when the receiver's status flags explicitly advertise
        // PIN-required (0x8) or legacy-pairing (0x200). A pk field by itself
        // is not sufficient evidence; many TV/projector AirPlay clones expose
        // AppleTV-like model/pk TXT records without any on-screen pairing UI.
        if !(service.txt.pin_required() || service.txt.legacy_pairing()) {
            return false;
        }

        let Some(key) = Self::legacy_pairing_key(device) else {
            return true;
        };
        !self.legacy_secrets.contains_key(&key)
    }

    fn begin_legacy_pairing(&mut self, device: &DeviceRecord) {
        let Some(service) = device.raop.as_ref().or(device.airplay.as_ref()) else {
            return;
        };
        let Some(device_key) = Self::legacy_pairing_key(device) else {
            return;
        };

        let host = preferred_service_address(service);
        let port = service.port;
        let et = service
            .txt
            .fields
            .get("et")
            .cloned()
            .unwrap_or_else(|| "0,4".into());
        let md = service
            .txt
            .fields
            .get("md")
            .cloned()
            .unwrap_or_else(|| "0,1,2".into());
        let name = device.display_name.clone();

        let helper = match std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("cliraop.exe")))
        {
            Some(path) if path.is_file() => path,
            _ => {
                let error = "cliraop.exe is missing next to SAirplay2.".to_owned();
                self.log.push(error.clone());
                self.playback = PlaybackUiState::Error(error);
                return;
            }
        };

        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let (pin_tx, pin_rx) = mpsc::sync_channel::<String>(1);
        self.pairing_rx = Some(result_rx);
        self.pairing_pin_tx = Some(pin_tx);
        self.pairing_open = true;
        self.pairing_pin.clear();
        self.pairing_name = name.clone();
        self.pairing_pin_sent = false;
        self.pairing_retry_start = true;
        self.playback = PlaybackUiState::Idle;
        self.log.push(format!(
            "{name}: starting source AppleTV PIN pairing on {host}:{port}."
        ));

        thread::Builder::new()
            .name("sairplay-appletv-pairing".into())
            .spawn(move || {
                let mut command = Command::new(&helper);
                command
                    .arg("-r")
                    .arg("-p")
                    .arg(port.to_string())
                    .arg("-a")
                    .arg("-t")
                    .arg(&et)
                    .arg("-m")
                    .arg(&md)
                    // cliraop pairing mode still parses the normal positional
                    // player/file arguments after pairing. Use the selected
                    // receiver and NUL so it exits cleanly after validating.
                    .arg(&host)
                    .arg("NUL")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .creation_flags(0x08000000);

                let mut child = match command.spawn() {
                    Ok(child) => child,
                    Err(error) => {
                        let _ = result_tx.send(LegacyPairingResult::Failed {
                            device_name: name,
                            error: format!("cannot start source pairing helper: {error}"),
                        });
                        return;
                    }
                };

                let Some(mut stdin) = child.stdin.take() else {
                    let _ = child.kill();
                    let _ = result_tx.send(LegacyPairingResult::Failed {
                        device_name: name,
                        error: "pairing helper stdin was not created".into(),
                    });
                    return;
                };
                let stdout = child.stdout.take();
                let stderr = child.stderr.take();

                // AppleTVpairing() performs a 5 s mDNS scan and then scanf()s
                // the selected IP. Feeding it now is safe; the pipe buffers it.
                if writeln!(stdin, "{host}").is_err() || stdin.flush().is_err() {
                    let _ = child.kill();
                    let _ = result_tx.send(LegacyPairingResult::Failed {
                        device_name: name,
                        error: "cannot send receiver address to pairing helper".into(),
                    });
                    return;
                }

                let pin = match pin_rx.recv() {
                    Ok(pin) => pin,
                    Err(_) => {
                        let _ = child.kill();
                        return;
                    }
                };
                if writeln!(stdin, "{pin}").is_err() || stdin.flush().is_err() {
                    let _ = child.kill();
                    let _ = result_tx.send(LegacyPairingResult::Failed {
                        device_name: name,
                        error: "cannot send PIN to pairing helper".into(),
                    });
                    return;
                }
                drop(stdin);

                let stderr_thread = stderr.map(|stderr| {
                    thread::spawn(move || {
                        BufReader::new(stderr)
                            .lines()
                            .map_while(Result::ok)
                            .collect::<Vec<_>>()
                    })
                });

                let output = stdout
                    .map(|stdout| {
                        BufReader::new(stdout)
                            .lines()
                            .map_while(Result::ok)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();

                let status = child.wait();
                let stderr_lines = stderr_thread
                    .and_then(|join| join.join().ok())
                    .unwrap_or_default();

                let secret = output.iter().find_map(|line| {
                    line.trim()
                        .strip_prefix("secret is ")
                        .map(|value| value.trim().to_owned())
                });

                match secret {
                    Some(secret) if !secret.is_empty() => {
                        let _ = result_tx.send(LegacyPairingResult::Success {
                            device_key,
                            device_name: name,
                            secret,
                        });
                    }
                    _ => {
                        let detail = stderr_lines
                            .iter()
                            .rev()
                            .find(|line| !line.trim().is_empty())
                            .cloned()
                            .or_else(|| {
                                output
                                    .iter()
                                    .rev()
                                    .find(|line| !line.trim().is_empty())
                                    .cloned()
                            })
                            .unwrap_or_else(|| format!("pairing helper exited with {status:?}"));
                        let _ = result_tx.send(LegacyPairingResult::Failed {
                            device_name: name,
                            error: detail,
                        });
                    }
                }
            })
            .expect("failed to spawn AppleTV pairing worker");
    }

    fn pump_legacy_pairing(&mut self) {
        let Some(rx) = &self.pairing_rx else {
            return;
        };

        let result = match rx.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(LegacyPairingResult::Failed {
                device_name: self.pairing_name.clone(),
                error: "pairing worker ended unexpectedly".into(),
            }),
        };

        let Some(result) = result else {
            return;
        };

        self.pairing_rx = None;
        self.pairing_pin_tx = None;
        self.pairing_open = false;
        self.pairing_pin_sent = false;

        match result {
            LegacyPairingResult::Success {
                device_key,
                device_name,
                secret,
            } => {
                self.legacy_secrets.insert(device_key, secret);
                self.log.push(format!(
                    "{device_name}: AppleTV PIN pairing success; session credential is ready."
                ));
                self.playback = PlaybackUiState::Idle;
                if std::mem::take(&mut self.pairing_retry_start) {
                    self.start_selected_inner();
                }
            }
            LegacyPairingResult::Failed { device_name, error } => {
                self.pairing_retry_start = false;
                let message = format!("{device_name}: AppleTV pairing failed: {error}");
                self.log.push(message.clone());
                self.playback = PlaybackUiState::Error(message);
            }
        }
    }

    fn render_hires_quality_warning(&mut self, ctx: &egui::Context) {
        if !self.hires_quality_warning_open {
            return;
        }

        let title = self.t("Cảnh báo chất lượng 24-bit", "24-bit Quality Warning");
        let close_label = self.t("Đã hiểu", "Got it");
        let mut open = self.hires_quality_warning_open;

        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(430.0)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(self.t(
                        "Phát hiện mất gói không thể khôi phục",
                        "Unrecoverable packet loss detected",
                    ))
                    .size(15.0)
                    .strong()
                    .color(UiTheme::amber()),
                );
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(&self.hires_quality_warning_text)
                        .size(12.5)
                        .color(UiTheme::text()),
                );
                ui.add_space(12.0);
                if ui
                    .add(
                        egui::Button::new(close_label)
                            .min_size(egui::vec2(96.0, 32.0)),
                    )
                    .clicked()
                {
                    self.hires_quality_warning_open = false;
                }
            });

        self.hires_quality_warning_open &= open;
    }

    fn render_pairing_window(&mut self, ctx: &egui::Context) {
        if !self.pairing_open {
            return;
        }

        let title = self.t("Ghép nối AirPlay", "AirPlay Pairing");
        let instruction = self.t(
            "Chờ mã PIN 4 số xuất hiện trên TV, sau đó nhập mã vào đây.",
            "Wait for the 4-digit PIN to appear on the TV, then enter it here.",
        );
        let waiting = self.t(
            "Đang chờ hoàn tất ghép nối...",
            "Waiting for pairing to complete...",
        );

        let mut open = self.pairing_open;
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(390.0)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(&self.pairing_name)
                        .size(15.0)
                        .strong(),
                );
                ui.add_space(5.0);
                ui.label(egui::RichText::new(instruction).size(12.5));
                ui.add_space(10.0);

                let edit = ui.add_enabled(
                    !self.pairing_pin_sent,
                    egui::TextEdit::singleline(&mut self.pairing_pin)
                        .hint_text("1234")
                        .desired_width(110.0),
                );
                if edit.changed() {
                    self.pairing_pin.retain(|c| c.is_ascii_digit());
                    self.pairing_pin.truncate(4);
                }

                ui.add_space(8.0);
                if self.pairing_pin_sent {
                    ui.label(
                        egui::RichText::new(waiting)
                            .size(12.0)
                            .color(UiTheme::text_soft()),
                    );
                } else if ui
                    .add_enabled(
                        self.pairing_pin.len() == 4,
                        egui::Button::new(self.t("Ghép nối", "Pair"))
                            .min_size(egui::vec2(100.0, 32.0)),
                    )
                    .clicked()
                {
                    if let Some(tx) = &self.pairing_pin_tx {
                        if tx.send(self.pairing_pin.clone()).is_ok() {
                            self.pairing_pin_sent = true;
                            self.log.push(format!(
                                "{}: PIN submitted to source pairing helper.",
                                self.pairing_name
                            ));
                        }
                    }
                }
            });

        if !open && !self.pairing_pin_sent {
            self.pairing_open = false;
            self.pairing_pin_tx = None;
            self.pairing_rx = None;
            self.pairing_retry_start = false;
            self.log.push(format!("{}: pairing cancelled.", self.pairing_name));
        } else {
            self.pairing_open = open;
        }
    }

    fn monitor_running_session(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };

        for event in session.drain_startup_events() {
            self.log.push(event);
        }

        let rtx = session.retransmit_stats();
        if rtx != self.last_retransmit_stats {
            let prev = self.last_retransmit_stats;
            let requested_delta = rtx.requested.saturating_sub(prev.requested);
            let answered_delta = rtx.answered.saturating_sub(prev.answered);
            let expired_delta = rtx.expired.saturating_sub(prev.expired);
            if requested_delta != 0 || expired_delta != 0 {
                self.log.push(format!(
                    "Diagnostic: retransmit activity · requested +{} (total {}) · answered +{} (total {}) · expired +{} (total {}).",
                    requested_delta,
                    rtx.requested,
                    answered_delta,
                    rtx.answered,
                    expired_delta,
                    rtx.expired
                ));
            }

            // Runtime HomePod evidence showed audible crackle with a burst of
            // retransmit requests even when every request was still answered.
            // Upstream also documents HomePod 24-bit realtime UDP as MTU-sensitive.
            // Treat a burst (>=4 requests in one monitor interval) as degraded,
            // while any expired packet remains an immediate hard warning.
            // This is a SAirplay2 runtime-health policy, not an AirPlay protocol rule.
            let is_hires_24 = session
                .audio_format()
                .is_some_and(|format| format.bit_depth > 16);
            let retransmit_burst = requested_delta >= 4;
            let degraded_24bit = expired_delta > 0 || retransmit_burst;
            if is_hires_24 && degraded_24bit && !self.hires_quality_warning_shown {
                self.hires_quality_warning_shown = true;
                self.hires_quality_warning_open = true;
                self.hires_quality_warning_text = match self.language {
                    UiLanguage::Vi if expired_delta > 0 => format!(
                        "Đường truyền 24-bit đang không đạt độ ổn định cần thiết. Có {} gói RTP không thể khôi phục (tổng {}).\n\nNếu có mất tiếng, rè hoặc ngắt quãng, nên chuyển thiết bị này về 16-bit.",
                        expired_delta,
                        rtx.expired
                    ),
                    UiLanguage::Vi => format!(
                        "Đường truyền 24-bit vừa xuất hiện một đợt yêu cầu truyền lại cao: {} gói trong một chu kỳ đo. Tất cả vẫn được khôi phục, nhưng kiểu burst này đã trùng với hiện tượng bụp/rè khi thử HomePod 24-bit.\n\nBạn có thể tiếp tục nghe; nếu hiện tượng lặp lại, nên chuyển thiết bị này về 16-bit.",
                        requested_delta
                    ),
                    UiLanguage::En if expired_delta > 0 => format!(
                        "The 24-bit path is not meeting the required stability. {} RTP packet(s) could not be recovered ({} total).\n\nIf you hear dropouts, crackle, or silence, switch this receiver back to 16-bit.",
                        expired_delta,
                        rtx.expired
                    ),
                    UiLanguage::En => format!(
                        "The 24-bit path just produced a high retransmit burst: {} packet(s) in one measurement interval. All were recovered, but this burst pattern matched audible crackle during HomePod 24-bit testing.\n\nYou can keep listening; if it repeats, switch this receiver back to 16-bit.",
                        requested_delta
                    ),
                };
                self.log.push(format!(
                    "24-bit quality warning: retransmit burst +{} · expired +{} · totals requested={} answered={} expired={}.",
                    requested_delta,
                    expired_delta,
                    rtx.requested,
                    rtx.answered,
                    rtx.expired
                ));
            }
            self.last_retransmit_stats = rtx;
        }

        if let Some(error) = session.audio_error() {
            self.log.push(format!("Audio worker stopped: {error}"));
            self.playback = PlaybackUiState::Error(error);
            self.session = None;
        } else if !session.audio_running() {
            self.log.push("Audio worker stopped.".into());
            self.playback = PlaybackUiState::Error("Audio worker stopped".into());
            self.session = None;
        } else {
            let feedback_error = session.feedback_error();
            let feedback_running = session.feedback_running();

            match feedback_error {
                Some(error) => {
                    if self.last_feedback_error.as_deref() != Some(error.as_str()) {
                        self.log.push(format!("Feedback keepalive: {error}"));
                        self.last_feedback_error = Some(error.clone());
                    }
                    if !feedback_running {
                        let is_hard_close = error.contains("hard failure")
                            && error.contains("peer/control channel closed");
                        let native_session = matches!(
                            self.session,
                            Some(ActiveSession::Single(_)) | Some(ActiveSession::Group(_))
                        );

                        self.session = None;
                        self.active_fullnames.clear();

                        if is_hard_close && native_session && self.native_retry_available {
                            self.native_retry_available = false;
                            self.log.push(
                                "Native control channel closed during initial keepalive; rebuilding the same session once automatically."
                                    .into(),
                            );
                            self.playback = PlaybackUiState::Idle;
                            std::thread::sleep(std::time::Duration::from_millis(250));
                            self.start_selected_inner();
                        } else {
                            self.playback = PlaybackUiState::Error(error);
                        }
                    }
                }
                None => {
                    self.last_feedback_error = None;
                    if !feedback_running {
                        let error = "Feedback keepalive worker stopped".to_string();
                        self.log.push(error.clone());
                        self.playback = PlaybackUiState::Error(error);
                        self.session = None;
                    }
                }
            }
        }
    }

    fn start_selected(&mut self) {
        self.native_retry_available = true;
        self.start_selected_inner();
    }

    fn start_selected_inner(&mut self) {
        if !matches!(self.playback, PlaybackUiState::Idle | PlaybackUiState::Error(_)) {
            return;
        }

        if self.selected_fullnames.is_empty() {
            self.playback =
                PlaybackUiState::Error("Select at least one AirPlay 2 receiver first".into());
            return;
        }

        let initial_volume = match parse_volume_text(&self.initial_volume_text) {
            Ok(volume) => volume,
            Err(message) => {
                self.log.push(message.clone());
                self.playback = PlaybackUiState::Error(message);
                return;
            }
        };

        let devices = self.catalog.devices().to_vec();
        let selected_devices: Vec<(String, DeviceRecord)> = devices
            .into_iter()
            .filter_map(|device| {
                let fullname = device_primary_fullname(&device)?;
                self.selected_fullnames
                    .contains(&fullname)
                    .then_some((fullname, device))
            })
            .collect();

        if selected_devices.len() != self.selected_fullnames.len() {
            let message =
                "One or more selected AirPlay receivers disappeared; rescan and select again"
                    .to_owned();
            self.log.push(message.clone());
            self.playback = PlaybackUiState::Error(message);
            return;
        }

        let routes = selected_devices
            .iter()
            .map(|(_, device)| device.route(false, false))
            .collect::<Vec<_>>();
        let all_native = routes
            .iter()
            .all(|route| *route == Route::AirPlay2Native);
        let all_legacy = routes
            .iter()
            .all(|route| matches!(route, Route::Raop | Route::AirPlay2Compat));

        if !all_native && !all_legacy {
            let message = "Mixed native AirPlay 2 + RAOP groups need one shared cross-transport timeline; select receivers from the same transport family for this build.".to_owned();
            self.log.push(message.clone());
            self.playback = PlaybackUiState::Error(message);
            return;
        }

        if all_legacy {
            if let Some((_, device)) = selected_devices
                .iter()
                .find(|(_, device)| self.legacy_pairing_required(device))
            {
                self.begin_legacy_pairing(device);
                return;
            }
        }

        let active_fullnames: BTreeSet<String> =
            selected_devices.iter().map(|(fullname, _)| fullname.clone()).collect();
        let member_count = selected_devices.len();

        let pair_name = if all_native && member_count == 2 {
            let mut tsids = selected_devices.iter().filter_map(|(_, device)| {
                device
                    .airplay
                    .as_ref()
                    .and_then(|service| service.txt.fields.get("tsid"))
            });
            let first = tsids.next().cloned();
            let second = tsids.next().cloned();
            if first.is_some() && first == second {
                selected_devices
                    .iter()
                    .filter_map(|(_, device)| {
                        device
                            .airplay
                            .as_ref()
                            .and_then(|service| service.txt.fields.get("gpn"))
                    })
                    .find(|name| !name.trim().is_empty())
                    .cloned()
            } else {
                None
            }
        } else {
            None
        };

        let requested_mode = if self.selected_stereo_pair.is_some() && member_count == 2 {
            PlaybackMode::StereoPair
        } else if self.multiroom_enabled && member_count > 1 {
            PlaybackMode::MultiRoom
        } else {
            PlaybackMode::Single
        };

        let label = match requested_mode {
            PlaybackMode::StereoPair => pair_name.unwrap_or_else(|| match self.language {
                UiLanguage::Vi => "Cặp HomePod Stereo".to_owned(),
                UiLanguage::En => "HomePod Stereo Pair".to_owned(),
            }),
            PlaybackMode::MultiRoom => match self.language {
                UiLanguage::Vi => format!("MultiRoom · {member_count} thiết bị"),
                UiLanguage::En => format!("MultiRoom · {member_count} receivers"),
            },
            PlaybackMode::Single => selected_devices[0].1.display_name.clone(),
        };

        let (tx, rx) = mpsc::sync_channel(1);
        self.connect_rx = Some(rx);
        self.playback = PlaybackUiState::Connecting(label.clone());
        self.session = None;
        self.active_fullnames.clear();
        self.last_feedback_error = None;

        if all_native {
            let mut configs = Vec::<NativeGroupMemberConfig>::with_capacity(member_count);
            for (fullname, device) in &selected_devices {
                let hires_override = self.hires_overrides.get(fullname).copied();
                let config = match native_config_for_device(
                    device,
                    initial_volume,
                    hires_override,
                ) {
                    Ok(config) => config,
                    Err(message) => {
                        self.log.push(message.clone());
                        self.playback = PlaybackUiState::Error(message);
                        self.connect_rx = None;
                        return;
                    }
                };
                configs.push(NativeGroupMemberConfig::new(
                    fullname.clone(),
                    config,
                ));
            }

            thread::Builder::new()
                .name("sairplay-native-connect".into())
                .spawn(move || {
                    let result = NativeGroupSession::connect(configs)
                        .map(ActiveSession::Group)
                        .map_err(|e| e.to_string())
                        .map(|session| ConnectSuccess {
                            session,
                            active_fullnames,
                            label,
                            mode: requested_mode,
                        });
                    let _ = tx.send(result);
                })
                .expect("failed to spawn native connect worker");
        } else {
            let mut configs = Vec::<LegacyMemberConfig>::with_capacity(member_count);
            for (_, device) in &selected_devices {
                let pairing_key = Self::legacy_pairing_key(device);
                let secret = pairing_key
                    .as_ref()
                    .and_then(|key| self.legacy_secrets.get(key))
                    .map(String::as_str);
                match legacy_config_for_device(device, initial_volume, secret) {
                    Ok(config) => {
                        configs.push(config);
                    }
                    Err(message) => {
                        self.log.push(message.clone());
                        self.playback = PlaybackUiState::Error(message);
                        self.connect_rx = None;
                        return;
                    }
                }
            }

            thread::Builder::new()
                .name("sairplay-legacy-connect".into())
                .spawn(move || {
                    let result = LegacyGroupSession::connect(configs)
                        .map(ActiveSession::Legacy)
                        .map_err(|e| e.to_string())
                        .map(|session| ConnectSuccess {
                            session,
                            active_fullnames,
                            label,
                            mode: requested_mode,
                        });
                    let _ = tx.send(result);
                })
                .expect("failed to spawn legacy connect worker");
        }
    }

    fn stop_playback(&mut self) {
        if self.session.take().is_some() {
            self.log.push("Playback stopped; AirPlay session resources released.".into());
        }
        self.active_fullnames.clear();
        self.active_mode = None;
        self.membership_pending.clear();
        self.membership_rx = None;
        self.playback = PlaybackUiState::Idle;
        self.native_retry_available = true;
        self.last_retransmit_stats = RetransmitStats::default();
    }

    fn header_status(&self) -> (&'static str, String, egui::Color32) {
        match &self.playback {
            PlaybackUiState::Idle => (
                self.t("Sẵn sàng", "Ready"),
                self.t("Đang chờ phát nhạc...", "Waiting for playback...").to_owned(),
                UiTheme::green(),
            ),
            PlaybackUiState::Connecting(_) => (
                self.t("Đang kết nối", "Connecting"),
                self.t("Đang chuẩn bị thiết bị...", "Preparing receiver...").to_owned(),
                UiTheme::amber(),
            ),
            PlaybackUiState::Playing(_) => {
                let detail = match self.session.as_ref().and_then(ActiveSession::audio_format) {
                    Some(format) => format!(
                        "AirPlay 2 · ALAC · {}-bit / {} kHz",
                        format.bit_depth,
                        if format.sample_rate == 44_100 { "44.1".to_owned() } else { (format.sample_rate / 1000).to_string() }
                    ),
                    None => self.t(
                        "Đang truyền âm thanh qua AirPlay",
                        "Streaming via AirPlay",
                    ).to_owned(),
                };
                (
                    self.t("Đang chạy", "Running"),
                    detail,
                    UiTheme::blue(),
                )
            },
            PlaybackUiState::Error(_) => (
                self.t("Lỗi kết nối", "Connection Error"),
                self.t("Xem Log để kiểm tra", "Open Log for details").to_owned(),
                UiTheme::red(),
            ),
        }
    }

    fn ensure_device_textures(&mut self, ctx: &egui::Context) {
        if self.device_textures_initialized {
            return;
        }
        self.device_textures_initialized = true;

        for artwork in ALL_DEVICE_ARTWORK {
            let bytes = device_artwork_bytes(artwork);
            match image::load_from_memory(bytes) {
                Ok(decoded) => {
                    let rgba = decoded.to_rgba8();
                    let size = [rgba.width() as usize, rgba.height() as usize];
                    if size != [256, 256] {
                        self.log.push(format!(
                            "Device artwork {} has unexpected size {}x{}.",
                            device_artwork_name(artwork),
                            size[0],
                            size[1],
                        ));
                        continue;
                    }
                    let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                    let texture = ctx.load_texture(
                        format!("sairplay-device-{}", device_artwork_name(artwork)),
                        color,
                        egui::TextureOptions::LINEAR,
                    );
                    self.device_textures.insert(artwork, texture);
                }
                Err(err) => {
                    self.log.push(format!(
                        "Device artwork {} decode failed: {err}",
                        device_artwork_name(artwork),
                    ));
                }
            }
        }

        self.log.push(format!(
            "Device artwork cache ready: {}/{} textures.",
            self.device_textures.len(),
            ALL_DEVICE_ARTWORK.len(),
        ));
    }

    fn device_status(&self, device: &DeviceRecord, stereo_pair: bool) -> (&'static str, StatusTone) {
        let members = device_selection_members(device, stereo_pair);
        let member_set = members.iter().cloned().collect::<BTreeSet<_>>();
        let selected = if stereo_pair {
            self.selected_stereo_pair
                .as_ref()
                .is_some_and(|pair| *pair == member_set)
        } else if self.selected_stereo_pair.is_some() {
            false
        } else {
            !members.is_empty()
                && members
                    .iter()
                    .all(|fullname| self.selected_fullnames.contains(fullname))
        };
        let active = if stereo_pair {
            self.active_mode == Some(PlaybackMode::StereoPair)
                && !members.is_empty()
                && members
                    .iter()
                    .all(|fullname| self.active_fullnames.contains(fullname))
        } else {
            !members.is_empty()
                && members
                    .iter()
                    .all(|fullname| self.active_fullnames.contains(fullname))
        };
        let pending = members
            .iter()
            .any(|fullname| self.membership_pending.contains(fullname));

        if pending {
            return (self.t("Đang kết nối", "Connecting"), StatusTone::Orange);
        }

        match &self.playback {
            PlaybackUiState::Connecting(_) if selected => (
                self.t("Đang kết nối", "Connecting"),
                StatusTone::Orange,
            ),
            PlaybackUiState::Playing(_) if active => (
                self.t("Đang chạy", "Running"),
                StatusTone::Blue,
            ),
            PlaybackUiState::Error(_) if selected => (
                self.t("Lỗi kết nối", "Connection Error"),
                StatusTone::Red,
            ),
            _ if device.route(false, false) == Route::AirPlay2Native => {
                (self.t("Sẵn sàng", "Ready"), StatusTone::Green)
            }
            _ if matches!(
                device.route(false, false),
                Route::Raop | Route::AirPlay2Compat
            ) => (self.t("Sẵn sàng", "Ready"), StatusTone::Green),
            _ => (self.t("Chờ", "Standby"), StatusTone::Gray),
        }
    }

    fn render_device_row(
        &mut self,
        ui: &mut egui::Ui,
        device: &DeviceRecord,
        stereo_pair: bool,
    ) {
        const ROW_H: f32 = 72.0;
        const SELECTOR_W: f32 = 24.0;
        const ART_W: f32 = 70.0;
        const STATUS_W: f32 = 166.0;

        let members = device_selection_members(device, stereo_pair);
        let member_set = members.iter().cloned().collect::<BTreeSet<_>>();
        let selected = if stereo_pair {
            self.selected_stereo_pair
                .as_ref()
                .is_some_and(|pair| *pair == member_set)
        } else if self.selected_stereo_pair.is_some() {
            false
        } else {
            !members.is_empty()
                && members
                    .iter()
                    .all(|fullname| self.selected_fullnames.contains(fullname))
        };
        let legacy_live = matches!(
            (&self.session, &self.playback),
            (Some(ActiveSession::Legacy(_)), PlaybackUiState::Playing(_))
        );
        let selectable = !members.is_empty()
            && !matches!(self.playback, PlaybackUiState::Connecting(_))
            && self.membership_rx.is_none()
            && !legacy_live;

        let address = device
            .airplay
            .as_ref()
            .map(preferred_service_address)
            .or_else(|| device.raop.as_ref().map(preferred_service_address))
            .unwrap_or_else(|| "-".into());

        let artwork = classify_device_artwork(device);
        let (status, status_tone) = self.device_status(device, stereo_pair);
        let sense = if selectable { egui::Sense::click() } else { egui::Sense::hover() };
        let (row_rect, response) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), sense);
        // Keep a real vertical gutter between adjacent rows so selected/hover
        // rounded cards can never visually overlap the next receiver.
        let body = row_rect.shrink2(egui::vec2(2.0, 5.0));

        if selected {
            ui.painter().rect_filled(
                body,
                egui::CornerRadius::same(11),
                egui::Color32::from_rgb(238, 247, 255),
            );
            ui.painter().rect_stroke(
                body,
                egui::CornerRadius::same(11),
                egui::Stroke::new(1.5, UiTheme::border_active()),
                egui::StrokeKind::Inside,
            );
        } else if response.hovered() {
            ui.painter().rect_filled(
                body,
                egui::CornerRadius::same(10),
                egui::Color32::from_rgb(247, 251, 255),
            );
        } else {
            ui.painter().line_segment(
                [
                    egui::pos2(body.left() + 4.0, body.bottom()),
                    egui::pos2(body.right() - 4.0, body.bottom()),
                ],
                egui::Stroke::new(1.0, UiTheme::guide()),
            );
        }

        let content = body.shrink2(egui::vec2(10.0, 5.0));
        let row_y = content.center().y;
        let selector_rect = egui::Rect::from_min_size(
            egui::pos2(content.left(), row_y - 21.0),
            egui::vec2(SELECTOR_W, 42.0),
        );
        let art_rect = egui::Rect::from_min_size(
            egui::pos2(selector_rect.right() + 8.0, row_y - 21.0),
            egui::vec2(ART_W, 42.0),
        );
        let status_rect = egui::Rect::from_min_size(
            egui::pos2(content.right() - STATUS_W, row_y - 22.0),
            egui::vec2(STATUS_W, 44.0),
        );
        let text_left = art_rect.right() + 12.0;
        let text_right = (status_rect.left() - 8.0).max(text_left + 110.0);
        let text_rect = egui::Rect::from_min_max(
            egui::pos2(text_left, row_y - 21.0),
            egui::pos2(text_right, row_y + 21.0),
        );

        ui.allocate_ui_at_rect(selector_rect, |ui| {
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                draw_selector(ui, selected, selectable);
            });
        });
        ui.allocate_ui_at_rect(art_rect, |ui| {
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                draw_device_art(ui, artwork, stereo_pair, egui::vec2(54.0, 54.0));
            });
        });
        ui.allocate_ui_at_rect(text_rect, |ui| {
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.add_space(3.0);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&device.display_name)
                            .size(14.0)
                            .strong()
                            .color(UiTheme::text()),
                    )
                    .truncate(),
                );
                ui.add_space(1.0);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(address)
                            .size(11.5)
                            .color(UiTheme::text_soft()),
                    )
                    .truncate(),
                );
            });
        });
        let hires_available = !members.is_empty()
            && members.iter().all(|fullname| {
                self.hires_capabilities.get(fullname).copied() == Some(true)
            });
        let mut hires_enabled = hires_available
            && members
                .iter()
                .all(|fullname| self.hires_enabled_for_fullname(fullname));
        let hires_editable = hires_available
            && !matches!(
                self.playback,
                PlaybackUiState::Connecting(_) | PlaybackUiState::Playing(_)
            );
        let mut hires_clicked = false;
        let tooltip_title = self.t("Phát nhạc 24-bit", "24-bit Playback").to_owned();
        let tooltip_body = self.t(
            "Bật để ưu tiên phát 24-bit trên thiết bị hỗ trợ. Chế độ này có thể gây lách tách, mất tiếng ngắt quãng hoặc lỗi đa vùng trên một số thiết bị / mạng Wi‑Fi. Nếu có lỗi, hãy tắt 24-bit và phát lại.",
            "Enable to prefer 24-bit playback on supported receivers. This mode may cause crackling, brief dropouts, or multi-room issues on some devices or Wi-Fi networks. If problems occur, turn off 24-bit and start playback again.",
        ).to_owned();

        let badge_rect = egui::Rect::from_min_size(
            egui::pos2(status_rect.left(), row_y - 13.0),
            egui::vec2(110.0, 26.0),
        );
        let bit_rect = egui::Rect::from_min_size(
            egui::pos2(status_rect.right() - 44.0, row_y - 19.0),
            egui::vec2(44.0, 38.0),
        );

        ui.allocate_ui_at_rect(badge_rect, |ui| {
            draw_status_badge(ui, status, status_tone);
        });

        ui.allocate_ui_at_rect(bit_rect, |ui| {
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                if hires_available {
                    let label = ui.label(
                        egui::RichText::new("24-bit")
                            .size(10.5)
                            .color(UiTheme::text_soft()),
                    );
                    show_hires_tooltip(label, &tooltip_title, &tooltip_body);
                    ui.add_space(2.0);
                    let toggle = draw_compact_switch(ui, &mut hires_enabled, hires_editable);
                    hires_clicked = toggle.clicked();
                    show_hires_tooltip(toggle, &tooltip_title, &tooltip_body);
                } else {
                    ui.add_space(12.0);
                    ui.label(
                        egui::RichText::new("16-bit")
                            .size(10.5)
                            .color(UiTheme::text_soft()),
                    );
                }
            });
        });

        if hires_clicked {
            for fullname in &members {
                self.hires_overrides.insert(fullname.clone(), hires_enabled);
            }
            self.log.push(format!(
                "{}: 24-bit hi-res {} for next native session.",
                device.display_name,
                if hires_enabled { "enabled" } else { "disabled" }
            ));
        }

        if response.clicked() && selectable && !hires_clicked {
            let all_selected = members
                .iter()
                .all(|fullname| self.selected_fullnames.contains(fullname));

            if stereo_pair {
                // Pair and MultiRoom are mutually exclusive presentation modes.
                // The pair still resolves to the same two physical endpoints
                // internally, but the GUI keeps one logical selection identity.
                if matches!(self.playback, PlaybackUiState::Playing(_)) {
                    return;
                }
                self.multiroom_enabled = false;
                self.selected_fullnames.clear();
                if selected {
                    self.selected_stereo_pair = None;
                } else {
                    for fullname in &members {
                        self.selected_fullnames.insert(fullname.clone());
                    }
                    self.selected_stereo_pair =
                        Some(members.iter().cloned().collect::<BTreeSet<_>>());
                }
            } else {
                // Clicking an individual receiver exits Pair selection.
                self.selected_stereo_pair = None;

                if self.multiroom_enabled {
                    if all_selected {
                        for fullname in &members {
                            self.selected_fullnames.remove(fullname);
                        }
                        if matches!(self.playback, PlaybackUiState::Playing(_)) {
                            self.remove_live_members(&members);
                        }
                    } else {
                        for fullname in &members {
                            self.selected_fullnames.insert(fullname.clone());
                        }
                        if matches!(self.playback, PlaybackUiState::Playing(_)) {
                            self.request_live_add(&members);
                        }
                    }
                } else {
                    self.selected_fullnames.clear();
                    if !all_selected {
                        for fullname in &members {
                            self.selected_fullnames.insert(fullname.clone());
                        }
                    }
                }
            }

            if matches!(self.playback, PlaybackUiState::Error(_)) {
                self.playback = PlaybackUiState::Idle;
            }
        }
    }

    fn render_empty_device_state(
        &self,
        ui: &mut egui::Ui,
        stereo_pair: bool,
        height: f32,
    ) {
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), height),
            egui::Layout::top_down(egui::Align::Center),
            |ui| {
                // Fixed-size group keeps icon and text from ever overlapping
                // regardless of panel height or window scaling.
                let block_h = 126.0;
                ui.add_space(((height - block_h) * 0.46).max(24.0));

                let (icon_rect, _) =
                    ui.allocate_exact_size(egui::vec2(54.0, 54.0), egui::Sense::hover());
                let center = icon_rect.center();

                ui.painter().circle_filled(
                    center,
                    26.0,
                    egui::Color32::from_rgb(45, 137, 255),
                );
                ui.painter().circle_stroke(
                    center + egui::vec2(-3.5, -3.5),
                    9.0,
                    egui::Stroke::new(2.8, egui::Color32::WHITE),
                );
                ui.painter().line_segment(
                    [
                        center + egui::vec2(3.0, 3.0),
                        center + egui::vec2(11.0, 11.0),
                    ],
                    egui::Stroke::new(2.8, egui::Color32::WHITE),
                );

                ui.add_space(14.0);
                let title = if stereo_pair {
                    self.t(
                        "Chưa phát hiện cặp HomePod Stereo",
                        "No HomePod stereo pair detected",
                    )
                } else {
                    self.t("Chưa phát hiện thiết bị", "No devices detected")
                };
                ui.label(
                    egui::RichText::new(title)
                        .size(16.0)
                        .strong()
                        .color(UiTheme::text()),
                );

                ui.add_space(5.0);
                ui.label(
                    egui::RichText::new(self.t(
                        "Ấn vào biểu tượng / logo ứng dụng để quét lại thiết bị.",
                        "Click the app icon / logo to scan for devices again.",
                    ))
                    .size(11.8)
                    .color(UiTheme::text_soft()),
                );
            },
        );
    }

    fn render_device_panel(
        &mut self,
        ui: &mut egui::Ui,
        title: &'static str,
        devices: &[DeviceRecord],
        stereo_pair: bool,
        panel_content_height: f32,
    ) {
        egui::Frame::new()
            .fill(UiTheme::surface())
            .stroke(egui::Stroke::new(1.0, UiTheme::border()))
            .corner_radius(egui::CornerRadius::same(15))
            .shadow(egui::epaint::Shadow {
                offset: [0, 3],
                blur: 14,
                spread: 0,
                color: egui::Color32::from_black_alpha(16),
            })
            .inner_margin(egui::Margin::same(11))
            .show(ui, |ui| {
                ui.set_min_height(panel_content_height);

                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 36.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        draw_small_airplay_mark(ui);
                        ui.add_space(5.0);
                        ui.label(
                            egui::RichText::new(title)
                                .size(18.0)
                                .strong()
                                .color(UiTheme::text()),
                        );

                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                let info = draw_info_button(ui);
                                if info.clicked() {
                                    if stereo_pair {
                                        self.show_pair_info = !self.show_pair_info;
                                        self.show_multiroom_info = false;
                                    } else {
                                        self.show_multiroom_info = !self.show_multiroom_info;
                                        self.show_pair_info = false;
                                    }
                                }

                                if stereo_pair {
                                    if self.show_pair_info {
                                        draw_info_popover(
                                            ui.ctx(),
                                            "pair_info_popover",
                                            info.rect,
                                            self.t(
                                                "Chọn cặp HomePod đã ghép nối trong ứng dụng Nhà (Home) của Apple để phát âm thanh stereo đồng bộ.",
                                                "Select a HomePod pair already configured in Apple's Home app for synchronized stereo playback.",
                                            ),
                                        );
                                    }
                                } else if !devices.is_empty() {
                                    ui.add_space(5.0);
                                    let label =
                                        self.t("Phát âm thanh đa vùng", "MultiRoom Audio");
                                    if draw_multiroom_toggle(
                                        ui,
                                        label,
                                        self.multiroom_enabled,
                                    )
                                    .clicked()
                                    {
                                        self.multiroom_enabled = !self.multiroom_enabled;
                                        if self.multiroom_enabled {
                                            self.selected_stereo_pair = None;
                                        }
                                        if !self.multiroom_enabled
                                            && self.selected_fullnames.len() > 1
                                            && !matches!(self.playback, PlaybackUiState::Playing(_))
                                        {
                                            let keep = self.selected_fullnames.iter().next().cloned();
                                            self.selected_fullnames.clear();
                                            if let Some(fullname) = keep {
                                                self.selected_fullnames.insert(fullname);
                                            }
                                        }
                                        self.log.push(format!(
                                            "MultiRoom {} · {} receiver(s) selected.",
                                            if self.multiroom_enabled { "enabled" } else { "disabled" },
                                            self.selected_fullnames.len()
                                        ));
                                    }

                                    if self.show_multiroom_info {
                                        draw_info_popover(
                                            ui.ctx(),
                                            "multiroom_info_popover",
                                            info.rect,
                                            self.t(
                                                "Khi bật chế độ này, bạn có thể chọn nhiều thiết bị trong danh sách để phát âm thanh đồng thời.",
                                                "When this mode is enabled, you can select multiple devices in the list to play audio simultaneously.",
                                            ),
                                        );
                                    }
                                }
                            },
                        );
                    },
                );

                ui.add_space(5.0);
                ui.painter().line_segment(
                    [
                        egui::pos2(ui.min_rect().left() + 1.0, ui.cursor().top()),
                        egui::pos2(ui.max_rect().right() - 1.0, ui.cursor().top()),
                    ],
                    egui::Stroke::new(1.0, UiTheme::guide()),
                );
                ui.add_space(6.0);

                ui.scope(|ui| {
                    let style = ui.style_mut();
                    style.spacing.scroll = egui::style::ScrollStyle::solid();
                    style.spacing.scroll.bar_width = 8.0;
                    style.spacing.scroll.handle_min_length = 34.0;
                    style.spacing.scroll.bar_inner_margin = 6.0;
                    style.spacing.scroll.bar_outer_margin = 2.0;

                    style.visuals.extreme_bg_color =
                        egui::Color32::from_rgb(239, 244, 251);
                    style.visuals.widgets.inactive.bg_fill =
                        egui::Color32::from_rgb(200, 212, 230);
                    style.visuals.widgets.inactive.corner_radius =
                        egui::CornerRadius::same(8);
                    style.visuals.widgets.hovered.bg_fill =
                        egui::Color32::from_rgb(178, 195, 219);
                    style.visuals.widgets.hovered.corner_radius =
                        egui::CornerRadius::same(8);
                    style.visuals.widgets.active.bg_fill =
                        egui::Color32::from_rgb(158, 180, 211);
                    style.visuals.widgets.active.corner_radius =
                        egui::CornerRadius::same(8);

                    let scroll_height = (panel_content_height - 62.0).max(224.0);
                    egui::ScrollArea::vertical()
                        .id_salt(if stereo_pair { "pair_scroll" } else { "receiver_scroll" })
                        .max_height(scroll_height)
                        .min_scrolled_height(scroll_height)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if devices.is_empty() {
                                self.render_empty_device_state(
                                    ui,
                                    stereo_pair,
                                    (scroll_height - 18.0).max(190.0),
                                );
                            } else {
                                for device in devices {
                                    self.render_device_row(ui, device, stereo_pair);
                                }
                            }
                        });
                });
            });
    }

    fn render_controls(&mut self, ui: &mut egui::Ui) {
        const CARD_OUTER_H: f32 = 58.0;
        const CARD_INNER_H: f32 = 42.0;
        const CARD_GAP: f32 = 7.0;
        const CARD_HORIZONTAL_MARGIN: f32 = 20.0;

        // Own one exact full-width row and split it geometrically into three
        // cards. Do not use horizontal() here: egui adds item_spacing between
        // children, which previously made the second/third cards overflow even
        // though their nominal widths summed to available_width().
        let controls_width = ui.available_width();
        let (controls_rect, _) = ui.allocate_exact_size(
            egui::vec2(controls_width, CARD_OUTER_H),
            egui::Sense::hover(),
        );
        let card_outer_w = ((controls_rect.width() - CARD_GAP * 2.0) / 3.0).max(1.0);
        let card_inner_w = (card_outer_w - CARD_HORIZONTAL_MARGIN).max(1.0);
        let volume_rect = egui::Rect::from_min_size(
            controls_rect.min,
            egui::vec2(card_outer_w, CARD_OUTER_H),
        );
        let actions_rect = egui::Rect::from_min_size(
            egui::pos2(volume_rect.right() + CARD_GAP, controls_rect.top()),
            egui::vec2(card_outer_w, CARD_OUTER_H),
        );
        let airplay_rect = egui::Rect::from_min_max(
            egui::pos2(actions_rect.right() + CARD_GAP, controls_rect.top()),
            controls_rect.max,
        );

        let card_frame = || {
            egui::Frame::new()
                .fill(UiTheme::surface())
                .stroke(egui::Stroke::new(1.0, UiTheme::border()))
                .corner_radius(egui::CornerRadius::same(13))
                .shadow(egui::epaint::Shadow {
                    offset: [0, 2],
                    blur: 9,
                    spread: 0,
                    color: egui::Color32::from_black_alpha(14),
                })
                .inner_margin(egui::Margin::symmetric(10, 8))
        };

        ui.allocate_ui_at_rect(volume_rect, |ui| {
                card_frame().show(ui, |ui| {
                    ui.set_min_size(egui::vec2(card_inner_w, CARD_INNER_H));
                    ui.set_max_size(egui::vec2(card_inner_w, CARD_INNER_H));

                    ui.allocate_ui_with_layout(
                        egui::vec2(card_inner_w, CARD_INNER_H),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            draw_speaker_icon(ui, egui::vec2(25.0, 25.0));
                            ui.add_space(6.0);

                            ui.vertical(|ui| {
                                ui.label(
                                    egui::RichText::new(self.t("Âm lượng", "Receiver Volume"))
                                        .size(12.0)
                                        .strong()
                                        .color(UiTheme::text()),
                                );

                                let mut volume = parse_volume_text(&self.initial_volume_text)
                                    .ok()
                                    .flatten()
                                    .unwrap_or(50);

                                ui.horizontal(|ui| {
                                    let response =
                                        draw_volume_slider(ui, &mut volume, egui::vec2(140.0, 18.0));

                                    if response.changed() {
                                        self.initial_volume_text = volume.to_string();
                                        save_volume(volume);
                                        self.apply_volume_value(volume);
                                    }

                                    let (badge_rect, _) = ui.allocate_exact_size(
                                        egui::vec2(42.0, 22.0),
                                        egui::Sense::hover(),
                                    );
                                    ui.painter().rect_filled(
                                        badge_rect,
                                        egui::CornerRadius::same(6),
                                        egui::Color32::from_rgb(242, 246, 251),
                                    );
                                    ui.painter().text(
                                        badge_rect.center(),
                                        egui::Align2::CENTER_CENTER,
                                        format!("{volume}%"),
                                        egui::FontId::proportional(11.0),
                                        UiTheme::text_soft(),
                                    );
                                });
                            });
                        },
                    );
                });
        });

        ui.allocate_ui_at_rect(actions_rect, |ui| {
                card_frame().show(ui, |ui| {
                    ui.set_min_size(egui::vec2(card_inner_w, CARD_INNER_H));
                    ui.set_max_size(egui::vec2(card_inner_w, CARD_INNER_H));

                    ui.allocate_ui_with_layout(
                        egui::vec2(card_inner_w, CARD_INNER_H),
                        egui::Layout::left_to_right(egui::Align::Center)
                            .with_main_align(egui::Align::Center),
                        |ui| {
                            let start_enabled = !self.selected_fullnames.is_empty()
                                && matches!(
                                    self.playback,
                                    PlaybackUiState::Idle | PlaybackUiState::Error(_)
                                );

                            if draw_action_button(
                                ui,
                                &format!("▶  {}", self.t("Bắt đầu", "Start")),
                                egui::vec2(112.0, 36.0),
                                start_enabled,
                                true,
                            )
                            .clicked()
                            {
                                self.start_selected();
                            }

                            ui.add_space(8.0);

                            let stop_enabled = self.session.is_some();
                            if draw_action_button(
                                ui,
                                &format!("■  {}", self.t("Dừng", "Stop")),
                                egui::vec2(92.0, 36.0),
                                stop_enabled,
                                false,
                            )
                            .clicked()
                            {
                                self.stop_playback();
                            }
                        },
                    );
                });
        });

        ui.allocate_ui_at_rect(airplay_rect, |ui| {
                card_frame().show(ui, |ui| {
                    ui.set_min_size(egui::vec2(card_inner_w, CARD_INNER_H));
                    ui.set_max_size(egui::vec2(card_inner_w, CARD_INNER_H));

                    ui.allocate_ui_with_layout(
                        egui::vec2(card_inner_w, CARD_INNER_H),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            draw_airplay_wave_icon(ui, egui::vec2(34.0, 34.0));
                            ui.add_space(9.0);
                            ui.vertical(|ui| {
                                ui.label(
                                    egui::RichText::new(self.t(
                                        "Kết nối qua AirPlay 2",
                                        "Connect via AirPlay 2",
                                    ))
                                    .size(12.0)
                                    .strong()
                                    .color(UiTheme::text()),
                                );

                                let display_mode = self.active_mode.unwrap_or_else(|| {
                                    if self.selected_stereo_pair.is_some() {
                                        PlaybackMode::StereoPair
                                    } else if self.multiroom_enabled
                                        && self.selected_fullnames.len() > 1
                                    {
                                        PlaybackMode::MultiRoom
                                    } else {
                                        PlaybackMode::Single
                                    }
                                });

                                let detail = match display_mode {
                                    PlaybackMode::StereoPair => self.t(
                                        "Stereo Pair · 2 HomePod",
                                        "Stereo Pair · 2 HomePods",
                                    ).to_owned(),
                                    PlaybackMode::MultiRoom => match self.language {
                                        UiLanguage::Vi => format!(
                                            "MultiRoom · {} thiết bị đã chọn",
                                            self.selected_fullnames.len()
                                        ),
                                        UiLanguage::En => format!(
                                            "MultiRoom · {} receivers selected",
                                            self.selected_fullnames.len()
                                        ),
                                    },
                                    PlaybackMode::Single => self.t(
                                        "Sẵn sàng truyền · ALAC",
                                        "Ready to stream · ALAC",
                                    ).to_owned(),
                                };

                                ui.label(
                                    egui::RichText::new(detail)
                                        .size(10.8)
                                        .color(UiTheme::text_soft()),
                                );
                            });
                        },
                    );
                });
        });
    }

    fn render_trial_row(&mut self, ui: &mut egui::Ui) {
        let text = self.t("Dùng thử · còn 3 ngày", "Trial · 3 days left");
        let activate = self.t("Nhấn để kích hoạt", "Click to activate");

        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 34.0),
            egui::Sense::click(),
        );
        let fill = if response.hovered() {
            egui::Color32::from_rgb(244, 250, 255)
        } else {
            egui::Color32::from_rgb(249, 252, 255)
        };
        ui.painter().rect_filled(rect, egui::CornerRadius::same(11), fill);
        ui.painter().rect_stroke(
            rect,
            egui::CornerRadius::same(11),
            egui::Stroke::new(
                1.0,
                if response.hovered() { UiTheme::border_hover() } else { UiTheme::border() },
            ),
            egui::StrokeKind::Inside,
        );

        draw_key_icon_at(ui, egui::pos2(rect.left() + 20.0, rect.center().y));

        ui.painter().text(
            egui::pos2(rect.left() + 40.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            text,
            egui::FontId::proportional(12.3),
            UiTheme::text(),
        );
        ui.painter().line_segment(
            [
                egui::pos2(rect.left() + 164.0, rect.center().y - 7.0),
                egui::pos2(rect.left() + 164.0, rect.center().y + 7.0),
            ],
            egui::Stroke::new(1.0, UiTheme::border()),
        );
        ui.painter().text(
            egui::pos2(rect.left() + 177.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            activate,
            egui::FontId::proportional(11.8),
            if response.hovered() { UiTheme::blue() } else { UiTheme::text_soft() },
        );
        ui.painter().text(
            egui::pos2(rect.right() - 16.0, rect.center().y),
            egui::Align2::CENTER_CENTER,
            "›",
            egui::FontId::proportional(21.0),
            if response.hovered() { UiTheme::blue() } else { UiTheme::text_soft() },
        );

        if response.clicked() {
            self.activation_open = true;
        }
    }

    fn render_activation_window(&mut self, ctx: &egui::Context) {
        if !self.activation_open {
            return;
        }
        let title = self.t("Kích hoạt S-Airplay2", "Activate S-Airplay2");
        let key_label = self.t("Mã kích hoạt", "Activation key");
        let note = self.t(
            "Giao diện kích hoạt đã sẵn sàng. Cơ chế bản quyền thật sẽ được nối ở bước sau.",
            "Activation UI is ready. The licensing backend will be connected in a later step.",
        );
        let close_label = self.t("Đóng", "Close");

        let mut open = self.activation_open;
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(430.0)
            .show(ctx, |ui| {
                ui.label(egui::RichText::new(note).size(13.5));
                ui.add_space(10.0);
                ui.label(key_label);
                ui.add(
                    egui::TextEdit::singleline(&mut self.activation_key)
                        .hint_text("XXXX-XXXX-XXXX-XXXX")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.add_enabled(
                        false,
                        egui::Button::new(self.t("Kích hoạt", "Activate"))
                            .min_size(egui::vec2(120.0, 34.0)),
                    );
                    if ui.button(close_label).clicked() {
                        self.activation_open = false;
                    }
                });
            });
        self.activation_open &= open;
    }
}

impl eframe::App for SairplayApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_discovery();
        self.pump_hires_probes();
        self.pump_connect_result();
        self.pump_membership_result();
        self.pump_volume_result();
        self.pump_legacy_pairing();
        self.monitor_running_session();

        let mut visuals = egui::Visuals::light();
        visuals.panel_fill = UiTheme::bg();
        visuals.window_fill = UiTheme::surface();
        visuals.extreme_bg_color = egui::Color32::from_rgb(238, 245, 253);
        visuals.selection.bg_fill = UiTheme::blue();
        visuals.selection.stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);
        visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(239, 247, 255);
        visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, UiTheme::border_hover());
        visuals.widgets.active.bg_fill = egui::Color32::from_rgb(231, 242, 255);
        visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, UiTheme::border_active());
        visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(10);
        visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(10);
        visuals.widgets.active.corner_radius = egui::CornerRadius::same(10);
        visuals.hyperlink_color = UiTheme::blue();
        ctx.set_visuals(visuals);

        egui::TopBottomPanel::bottom("app_footer")
            .exact_height(28.0)
            .frame(
                egui::Frame::new()
                    .fill(UiTheme::surface())
                    .stroke(egui::Stroke::new(
                        1.0,
                        egui::Color32::from_rgb(218, 229, 242),
                    ))
                    .inner_margin(egui::Margin::symmetric(14, 4)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("@2026 SolYan S-Airplay2")
                            .small()
                            .color(egui::Color32::from_rgb(73, 91, 122)),
                    );
                    ui.label(egui::RichText::new("·").small());
                    ui.add(
                        egui::Hyperlink::from_label_and_url(
                            egui::RichText::new("Website").small().underline(),
                            "https://youtube.com/@solyan-music",
                        )
                        .open_in_new_tab(true),
                    );

                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if ui
                                .link(egui::RichText::new("Log").small().underline())
                                .on_hover_text(self.t("Sao chép toàn bộ Log", "Copy full log"))
                                .clicked()
                            {
                                ui.ctx().copy_text(self.log.join("\n"));
                            }
                        },
                    );
                });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(UiTheme::bg())
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ctx, |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 76.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        let logo = draw_app_logo(ui, egui::vec2(74.0, 74.0));
                        if logo
                            .on_hover_text(self.t(
                                "Nhấn để quét lại thiết bị",
                                "Click to scan devices again",
                            ))
                            .clicked()
                        {
                            self.rescan_devices();
                        }

                        ui.add_space(14.0);

                        ui.allocate_ui_with_layout(
                            egui::vec2(500.0, 72.0),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                ui.add_space(4.0);
                                ui.label(
                                    egui::RichText::new("SAirplay2")
                                        .size(32.0)
                                        .strong()
                                        .color(UiTheme::text()),
                                );
                                ui.add_space(1.0);
                                ui.label(
                                    egui::RichText::new(self.t(
                                        "Giao thức truyền tải âm thanh không dây của Apple",
                                        "Native AirPlay — Apple's lossless wireless audio transport",
                                    ))
                                    .size(13.0)
                                    .color(UiTheme::text_soft()),
                                );
                            },
                        );

                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.allocate_ui_with_layout(
                                    egui::vec2(50.0, 68.0),
                                    egui::Layout::top_down(egui::Align::Center),
                                    |ui| {
                                        ui.add_space(2.0);
                                        if draw_lang_button(
                                            ui,
                                            "VI",
                                            self.language == UiLanguage::Vi,
                                        )
                                        .clicked()
                                        {
                                            self.language = UiLanguage::Vi;
                                        }
                                        ui.add_space(4.0);
                                        if draw_lang_button(
                                            ui,
                                            "EN",
                                            self.language == UiLanguage::En,
                                        )
                                        .clicked()
                                        {
                                            self.language = UiLanguage::En;
                                        }
                                    },
                                );

                                ui.add_space(10.0);

                                let (status, detail, color) = self.header_status();
                                draw_header_status_card(ui, status, &detail, color);
                            },
                        );
                    },
                );
                ui.add_space(8.0);
                ui.add_space(8.0);

                let all_devices = self.catalog.devices().to_vec();

                // Keep every physical/discovered receiver in the Receivers column.
                // A HomePod member advertising a tight-sync ID is still an
                // individual mDNS endpoint. Only synthesize a Stereo Pair row
                // when at least two distinct HomePods share that tight-sync ID.
                let receivers: Vec<DeviceRecord> = all_devices.clone();
                let stereo_pairs = build_homepod_stereo_pairs(&all_devices);

                let receivers_title = self.t("THIẾT BỊ", "RECEIVERS");
                let pairs_title =
                    self.t("CẶP LOA HOMEPOD STEREO", "HOMEPOD STEREO PAIRS");

                // Consume the vertical space that was previously left blank
                // under the trial row. Reserve only the fixed controls/trial
                // block below the two device panels.
                const LOWER_CONTROLS_RESERVE: f32 = 105.0;
                const PANEL_FRAME_VERTICAL_MARGIN: f32 = 22.0;
                let panel_content_height = (
                    ui.available_height()
                        - LOWER_CONTROLS_RESERVE
                        - PANEL_FRAME_VERTICAL_MARGIN
                )
                    .max(286.0);

                ui.columns(2, |columns| {
                    self.render_device_panel(
                        &mut columns[0],
                        receivers_title,
                        &receivers,
                        false,
                        panel_content_height,
                    );
                    self.render_device_panel(
                        &mut columns[1],
                        pairs_title,
                        &stereo_pairs,
                        true,
                        panel_content_height,
                    );
                });

                ui.add_space(7.0);
                self.render_controls(ui);
                ui.add_space(6.0);
                self.render_trial_row(ui);
            });

        self.render_pairing_window(ctx);
        self.render_activation_window(ctx);
        self.render_hires_quality_warning(ctx);
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
}

fn draw_header_status_card(
    ui: &mut egui::Ui,
    status: &str,
    detail: &str,
    color: egui::Color32,
) {
    // One compact, language-independent geometry for VI and EN.
    // Do not let the right-to-left header layout stretch this card into the
    // remaining header width.
    const CARD_W: f32 = 286.0;
    const CARD_H: f32 = 60.0;
    const INNER_W: f32 = 258.0;
    const INNER_H: f32 = 42.0;

    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(CARD_W, CARD_H), egui::Sense::hover());

    ui.allocate_ui_at_rect(rect, |ui| {
        ui.set_min_size(egui::vec2(CARD_W, CARD_H));
        ui.set_max_size(egui::vec2(CARD_W, CARD_H));

        egui::Frame::new()
            .fill(UiTheme::surface())
            .stroke(egui::Stroke::new(1.0, UiTheme::border()))
            .corner_radius(egui::CornerRadius::same(22))
            .shadow(egui::epaint::Shadow {
                offset: [0, 2],
                blur: 10,
                spread: 0,
                color: egui::Color32::from_black_alpha(14),
            })
            .inner_margin(egui::Margin::symmetric(14, 9))
            .show(ui, |ui| {
                ui.set_min_size(egui::vec2(INNER_W, INNER_H));
                ui.set_max_size(egui::vec2(INNER_W, INNER_H));

                ui.horizontal_centered(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(216.0);
                        ui.label(
                            egui::RichText::new(status)
                                .size(13.5)
                                .strong()
                                .color(UiTheme::text()),
                        );
                        ui.label(
                            egui::RichText::new(detail)
                                .size(10.2)
                                .color(UiTheme::text_soft()),
                        );
                    });

                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let (dot_rect, _) = ui.allocate_exact_size(
                                egui::vec2(18.0, 18.0),
                                egui::Sense::hover(),
                            );
                            ui.painter().circle_filled(dot_rect.center(), 7.0, color);
                        },
                    );
                });
            });
    });
}

fn draw_volume_slider(ui: &mut egui::Ui, value: &mut u8, size: egui::Vec2) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    let rail = egui::Rect::from_center_size(rect.center(), egui::vec2(rect.width(), 5.0));

    if (response.clicked() || response.dragged()) && response.interact_pointer_pos().is_some() {
        let pointer = response.interact_pointer_pos().unwrap();
        let fraction = ((pointer.x - rail.left()) / rail.width()).clamp(0.0, 1.0);
        let next = (fraction * 100.0).round() as u8;
        if next != *value {
            *value = next;
            response.mark_changed();
        }
    }

    let fraction = *value as f32 / 100.0;
    let thumb_x = egui::lerp(rail.left()..=rail.right(), fraction);
    let active = egui::Rect::from_min_max(
        rail.left_top(),
        egui::pos2(thumb_x, rail.bottom()),
    );

    ui.painter().rect_filled(
        rail,
        egui::CornerRadius::same(3),
        egui::Color32::from_rgb(220, 229, 242),
    );
    ui.painter().rect_filled(
        active,
        egui::CornerRadius::same(3),
        UiTheme::blue(),
    );

    let thumb = egui::pos2(thumb_x, rail.center().y);
    let thumb_fill = if response.is_pointer_button_down_on() {
        UiTheme::blue_pressed()
    } else if response.hovered() {
        UiTheme::blue_hover()
    } else {
        UiTheme::blue()
    };
    ui.painter().circle_filled(thumb, 7.0, egui::Color32::WHITE);
    ui.painter().circle_filled(thumb, 5.5, thumb_fill);
    response
}

fn draw_airplay_wave_icon(ui: &mut egui::Ui, size: egui::Vec2) {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let color = if response.hovered() { UiTheme::blue_hover() } else { UiTheme::blue() };
    let center = rect.center();
    let heights = [10.0, 18.0, 26.0, 32.0, 26.0, 18.0, 10.0];
    for (i, h) in heights.iter().enumerate() {
        let x = center.x + (i as f32 - 3.0) * 4.0;
        ui.painter().line_segment(
            [egui::pos2(x, center.y - h / 2.0), egui::pos2(x, center.y + h / 2.0)],
            egui::Stroke::new(2.3, color),
        );
    }
}

fn draw_key_icon_at(ui: &mut egui::Ui, center: egui::Pos2) {
    let color = UiTheme::blue();
    let ring_center = center + egui::vec2(-3.0, -1.0);
    ui.painter().circle_stroke(ring_center, 5.0, egui::Stroke::new(1.7, color));
    ui.painter().line_segment(
        [ring_center + egui::vec2(3.7, 3.7), center + egui::vec2(7.0, 7.0)],
        egui::Stroke::new(1.7, color),
    );
    ui.painter().line_segment(
        [center + egui::vec2(4.0, 4.0), center + egui::vec2(7.0, 1.0)],
        egui::Stroke::new(1.7, color),
    );
}

fn draw_lang_button(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(48.0, 30.0), egui::Sense::click());
    let button_rect = rect.shrink(1.0);

    let fill = if selected {
        if response.is_pointer_button_down_on() {
            UiTheme::blue_pressed()
        } else if response.hovered() {
            UiTheme::blue_hover()
        } else {
            UiTheme::blue()
        }
    } else if response.is_pointer_button_down_on() {
        egui::Color32::from_rgb(239, 247, 255)
    } else if response.hovered() {
        egui::Color32::from_rgb(247, 251, 255)
    } else {
        egui::Color32::WHITE
    };

    let stroke = if selected {
        egui::Stroke::new(1.0, fill)
    } else if response.hovered() {
        egui::Stroke::new(1.0, UiTheme::border_hover())
    } else {
        egui::Stroke::new(1.0, UiTheme::border())
    };

    ui.painter()
        .rect_filled(button_rect, egui::CornerRadius::same(11), fill);
    ui.painter().rect_stroke(
        button_rect,
        egui::CornerRadius::same(11),
        stroke,
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        button_rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(12.0),
        if selected {
            egui::Color32::WHITE
        } else {
            UiTheme::text()
        },
    );

    response
}

fn draw_action_button(
    ui: &mut egui::Ui,
    label: &str,
    size: egui::Vec2,
    enabled: bool,
    primary: bool,
) -> egui::Response {
    let sense = if enabled { egui::Sense::click() } else { egui::Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(size, sense);

    let (fill, text) = if !enabled {
        (
            egui::Color32::from_rgb(228, 235, 243),
            egui::Color32::from_rgb(142, 155, 173),
        )
    } else if primary {
        (
            if response.is_pointer_button_down_on() {
                UiTheme::blue_pressed()
            } else if response.hovered() {
                UiTheme::blue_hover()
            } else {
                UiTheme::blue()
            },
            egui::Color32::WHITE,
        )
    } else {
        (
            if response.is_pointer_button_down_on() {
                egui::Color32::from_rgb(224, 234, 246)
            } else if response.hovered() {
                egui::Color32::from_rgb(240, 247, 255)
            } else {
                UiTheme::surface_soft()
            },
            UiTheme::text(),
        )
    };

    ui.painter().rect_filled(rect, egui::CornerRadius::same(11), fill);
    ui.painter().rect_stroke(
        rect,
        egui::CornerRadius::same(10),
        egui::Stroke::new(
            1.0,
            if primary && enabled {
                fill
            } else if response.hovered() && enabled {
                UiTheme::border_hover()
            } else {
                UiTheme::border()
            },
        ),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(13.0),
        text,
    );

    response
}

fn draw_speaker_icon(ui: &mut egui::Ui, size: egui::Vec2) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.put(
        rect.shrink(3.0),
        egui::Image::new(egui::include_image!("../assets/fluent_speaker_2_24_filled.svg"))
            .fit_to_exact_size(rect.shrink(3.0).size())
            .tint(UiTheme::text_soft()),
    );
}

fn homepod_tsid(device: &DeviceRecord) -> Option<&str> {
    let service = device.airplay.as_ref()?;
    let model = service.txt.model.as_deref().unwrap_or("");
    if !model.starts_with("AudioAccessory") {
        return None;
    }

    service
        .txt
        .fields
        .get("tsid")
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn homepod_member_identity(device: &DeviceRecord) -> Option<String> {
    let service = device.airplay.as_ref()?;
    Some(
        service
            .txt
            .fields
            .get("deviceid")
            .cloned()
            .unwrap_or_else(|| service.fullname.clone()),
    )
}

fn build_homepod_stereo_pairs(devices: &[DeviceRecord]) -> Vec<DeviceRecord> {
    let mut grouped: BTreeMap<String, Vec<&DeviceRecord>> = BTreeMap::new();

    for device in devices {
        if let Some(tsid) = homepod_tsid(device) {
            grouped.entry(tsid.to_owned()).or_default().push(device);
        }
    }

    let mut pairs = Vec::new();

    for (_tsid, members) in grouped {
        let mut unique_members = HashSet::new();
        let mut distinct = Vec::new();

        for member in members {
            let Some(identity) = homepod_member_identity(member) else {
                continue;
            };
            if unique_members.insert(identity) {
                distinct.push(member);
            }
        }

        if distinct.len() < 2 {
            continue;
        }

        // tsm=1 is the tight-sync master when advertised. Prefer it as the
        // representative endpoint for the synthetic pair row, otherwise keep
        // discovery order. Catalog data itself remains unchanged.
        let representative = distinct
            .iter()
            .copied()
            .find(|device| {
                device
                    .airplay
                    .as_ref()
                    .and_then(|service| service.txt.fields.get("tsm"))
                    .is_some_and(|value| value == "1")
            })
            .unwrap_or(distinct[0]);

        let pair_name = distinct
            .iter()
            .filter_map(|device| {
                device
                    .airplay
                    .as_ref()
                    .and_then(|service| service.txt.fields.get("gpn"))
            })
            .find(|name| !name.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| "HomePod Stereo Pair".to_owned());

        let pair_art = classify_homepod_pair_artwork(&distinct);
        let pair_members = distinct
            .iter()
            .filter_map(|device| device.airplay.as_ref().map(|service| service.fullname.clone()))
            .collect::<Vec<_>>()
            .join("\u{1f}");

        let mut pair = representative.clone();
        pair.display_name = pair_name;
        if let Some(service) = pair.airplay.as_mut() {
            service
                .txt
                .fields
                .insert("sairplay-pair-art".to_owned(), pair_art.to_owned());
            service
                .txt
                .fields
                .insert("sairplay-pair-members".to_owned(), pair_members);
        }
        pairs.push(pair);
    }

    pairs
}

fn device_primary_fullname(device: &DeviceRecord) -> Option<String> {
    let service = match device.route(false, false) {
        Route::Raop => device.raop.as_ref().or(device.airplay.as_ref()),
        Route::AirPlay2Compat | Route::AirPlay2Native => {
            device.airplay.as_ref().or(device.raop.as_ref())
        }
    }?;
    Some(service.fullname.clone())
}

fn device_selection_members(device: &DeviceRecord, stereo_pair: bool) -> Vec<String> {
    if stereo_pair {
        if let Some(service) = device.airplay.as_ref() {
            if let Some(value) = service.txt.fields.get("sairplay-pair-members") {
                let members: Vec<String> = value
                    .split('\u{1f}')
                    .filter(|member| !member.trim().is_empty())
                    .map(ToOwned::to_owned)
                    .collect();
                if !members.is_empty() {
                    return members;
                }
            }
        }
    }

    device_primary_fullname(device).into_iter().collect()
}

fn device_model(device: &DeviceRecord) -> String {
    device
        .airplay
        .as_ref()
        .and_then(|service| service.txt.model.as_deref())
        .or_else(|| {
            device
                .raop
                .as_ref()
                .and_then(|service| service.txt.model.as_deref())
        })
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn device_color_is_dark(device: &DeviceRecord) -> bool {
    let name = device.display_name.to_ascii_lowercase();
    let model = device_model(device);
    [
        "black", "space gray", "space grey", "midnight", "đen", "den",
        "dark", "graphite",
    ]
    .iter()
    .any(|needle| name.contains(needle) || model.contains(needle))
}

fn homepod_is_mini(device: &DeviceRecord) -> bool {
    let model = device_model(device);
    let name = device.display_name.to_ascii_lowercase();
    model.starts_with("audioaccessory5,") || name.contains("homepod mini")
}

fn classify_homepod_pair_artwork(members: &[&DeviceRecord]) -> &'static str {
    let mini = members.iter().all(|device| homepod_is_mini(device));
    let dark_count = members
        .iter()
        .filter(|device| device_color_is_dark(device))
        .count();

    match (mini, dark_count) {
        (true, 0) => "mini-white",
        (true, n) if n == members.len() => "mini-black",
        (true, _) => "mini-mixed",
        (false, 0) => "homepod-white",
        (false, n) if n == members.len() => "homepod-black",
        (false, _) => "homepod-mixed",
    }
}

fn classify_device_artwork(device: &DeviceRecord) -> DeviceArtwork {
    if let Some(tag) = device
        .airplay
        .as_ref()
        .and_then(|service| service.txt.fields.get("sairplay-pair-art"))
        .map(String::as_str)
    {
        return match tag {
            "mini-white" => DeviceArtwork::HomePodMiniPairWhite,
            "mini-black" => DeviceArtwork::HomePodMiniPairBlack,
            "mini-mixed" => DeviceArtwork::HomePodMiniPairMixed,
            "homepod-white" => DeviceArtwork::HomePodPairWhite,
            "homepod-black" => DeviceArtwork::HomePodPairBlack,
            "homepod-mixed" => DeviceArtwork::HomePodPairMixed,
            _ => DeviceArtwork::HomePodPairMixed,
        };
    }

    let model = device_model(device);
    let name = device.display_name.to_ascii_lowercase();

    if model.starts_with("airport") || name.contains("airport") {
        DeviceArtwork::AirportExpress
    } else if model.starts_with("audioaccessory") || name.contains("homepod") {
        match (homepod_is_mini(device), device_color_is_dark(device)) {
            (true, true) => DeviceArtwork::HomePodMiniBlack,
            (true, false) => DeviceArtwork::HomePodMiniWhite,
            (false, true) => DeviceArtwork::HomePodBlack,
            (false, false) => DeviceArtwork::HomePodWhite,
        }
    } else if model.starts_with("appletv") || name.contains("apple tv") || name.contains("appletv") {
        DeviceArtwork::AppleTv
    } else if name.contains("mac mini") || model.contains("macmini") {
        DeviceArtwork::MacMini
    } else if model.starts_with("mac") || name.contains("macbook") || name.contains("mac book") {
        DeviceArtwork::MacBook
    } else if name.contains("television")
        || name.contains("smart tv")
        || name.contains(" tivi")
        || name.starts_with("tivi")
        || name.contains("bravia")
        || name.contains("project")
        || model.contains("television")
    {
        DeviceArtwork::Tv
    } else if [
        "speaker", "loa", "sonos", "bose", "bluesound", "naim",
        "denon", "marantz", "bowers", "devialet",
    ]
    .iter()
    .any(|needle| name.contains(needle) || model.contains(needle))
    {
        DeviceArtwork::AirplaySpeakers
    } else if name.contains("server")
        || name.contains("streamer")
        || name.contains("music server")
        || model.contains("server")
    {
        DeviceArtwork::MusicServer
    } else {
        // User-approved fallback for an unknown AirPlay receiver.
        DeviceArtwork::MusicServer
    }
}

const ALL_DEVICE_ARTWORK: [DeviceArtwork; 17] = [
    DeviceArtwork::HomePodMiniWhite,
    DeviceArtwork::HomePodMiniBlack,
    DeviceArtwork::HomePodWhite,
    DeviceArtwork::HomePodBlack,
    DeviceArtwork::HomePodMiniPairWhite,
    DeviceArtwork::HomePodMiniPairBlack,
    DeviceArtwork::HomePodMiniPairMixed,
    DeviceArtwork::HomePodPairWhite,
    DeviceArtwork::HomePodPairBlack,
    DeviceArtwork::HomePodPairMixed,
    DeviceArtwork::MacBook,
    DeviceArtwork::MacMini,
    DeviceArtwork::MusicServer,
    DeviceArtwork::AirportExpress,
    DeviceArtwork::Tv,
    DeviceArtwork::AppleTv,
    DeviceArtwork::AirplaySpeakers,
];

fn device_artwork_name(artwork: DeviceArtwork) -> &'static str {
    match artwork {
        DeviceArtwork::HomePodMiniWhite => "homepod_mini_white",
        DeviceArtwork::HomePodMiniBlack => "homepod_mini_black",
        DeviceArtwork::HomePodWhite => "homepod_white",
        DeviceArtwork::HomePodBlack => "homepod_black",
        DeviceArtwork::HomePodMiniPairWhite => "homepod_mini_pair_white",
        DeviceArtwork::HomePodMiniPairBlack => "homepod_mini_pair_black",
        DeviceArtwork::HomePodMiniPairMixed => "homepod_mini_pair_mixed",
        DeviceArtwork::HomePodPairWhite => "homepod_pair_white",
        DeviceArtwork::HomePodPairBlack => "homepod_pair_black",
        DeviceArtwork::HomePodPairMixed => "homepod_pair_mixed",
        DeviceArtwork::MacBook => "macbook",
        DeviceArtwork::MacMini => "mac_mini",
        DeviceArtwork::MusicServer => "music_server",
        DeviceArtwork::AirportExpress => "airport_express",
        DeviceArtwork::Tv => "tv",
        DeviceArtwork::AppleTv => "apple_tv",
        DeviceArtwork::AirplaySpeakers => "airplay_speakers",
    }
}

fn device_artwork_bytes(artwork: DeviceArtwork) -> &'static [u8] {
    match artwork {
        DeviceArtwork::HomePodMiniWhite => {
            include_bytes!("../assets/devices/homepod_mini_white.png")
        }
        DeviceArtwork::HomePodMiniBlack => {
            include_bytes!("../assets/devices/homepod_mini_black.png")
        }
        DeviceArtwork::HomePodWhite => {
            include_bytes!("../assets/devices/homepod_white.png")
        }
        DeviceArtwork::HomePodBlack => {
            include_bytes!("../assets/devices/homepod_black.png")
        }
        DeviceArtwork::HomePodMiniPairWhite => {
            include_bytes!("../assets/devices/homepod_mini_pair_white.png")
        }
        DeviceArtwork::HomePodMiniPairBlack => {
            include_bytes!("../assets/devices/homepod_mini_pair_black.png")
        }
        DeviceArtwork::HomePodMiniPairMixed => {
            include_bytes!("../assets/devices/homepod_mini_pair_mixed.png")
        }
        DeviceArtwork::HomePodPairWhite => {
            include_bytes!("../assets/devices/homepod_pair_white.png")
        }
        DeviceArtwork::HomePodPairBlack => {
            include_bytes!("../assets/devices/homepod_pair_black.png")
        }
        DeviceArtwork::HomePodPairMixed => {
            include_bytes!("../assets/devices/homepod_pair_mixed.png")
        }
        DeviceArtwork::MacBook => {
            include_bytes!("../assets/devices/macbook.png")
        }
        DeviceArtwork::MacMini => {
            include_bytes!("../assets/devices/mac_mini.png")
        }
        DeviceArtwork::MusicServer => {
            include_bytes!("../assets/devices/music_server.png")
        }
        DeviceArtwork::AirportExpress => {
            include_bytes!("../assets/devices/airport_express.png")
        }
        DeviceArtwork::Tv => {
            include_bytes!("../assets/devices/tv.png")
        }
        DeviceArtwork::AppleTv => {
            include_bytes!("../assets/devices/apple_tv.png")
        }
        DeviceArtwork::AirplaySpeakers => {
            include_bytes!("../assets/devices/airplay_speakers.png")
        }
    }
}

fn draw_app_logo(ui: &mut egui::Ui, size: egui::Vec2) -> egui::Response {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let response = ui.put(
        rect,
        egui::Image::new(egui::include_image!("../assets/sairplay2-logo.png"))
            .fit_to_exact_size(size)
            .corner_radius(egui::CornerRadius::same(18))
            .sense(egui::Sense::click()),
    );

    if response.hovered() {
        ui.painter().rect_stroke(
            rect.expand(2.0),
            egui::CornerRadius::same(18),
            egui::Stroke::new(
                1.5,
                egui::Color32::from_rgba_unmultiplied(20, 126, 246, 80),
            ),
            egui::StrokeKind::Outside,
        );
    }

    response
}

fn draw_small_airplay_mark(ui: &mut egui::Ui) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(32.0, 32.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let bg = if response.hovered() {
        egui::Color32::from_rgb(225, 242, 255)
    } else {
        egui::Color32::from_rgb(236, 246, 255)
    };
    painter.circle_filled(rect.center(), 16.0, bg);

    let icon_rect = egui::Rect::from_center_size(rect.center(), egui::vec2(19.0, 19.0));
    ui.put(
        icon_rect,
        egui::Image::new(egui::include_image!("../assets/fluent_cast_24_filled.svg"))
            .fit_to_exact_size(icon_rect.size())
            .tint(UiTheme::blue()),
    );
}

fn draw_selector(ui: &mut egui::Ui, selected: bool, enabled: bool) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::hover());
    let center = rect.center();

    let ring = if selected {
        UiTheme::blue()
    } else if response.hovered() && enabled {
        UiTheme::border_active()
    } else {
        egui::Color32::from_rgb(124, 153, 197)
    };

    ui.painter()
        .circle_stroke(center, 9.0, egui::Stroke::new(if selected { 2.0 } else { 1.5 }, ring));

    if selected {
        ui.painter().circle_filled(center, 5.2, UiTheme::blue());
    }

    response
}

fn draw_multiroom_toggle(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
    let width = if label.chars().count() > 12 { 158.0 } else { 126.0 };
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, 26.0), egui::Sense::click());
    let circle = egui::pos2(rect.left() + 12.0, rect.center().y);

    ui.painter().circle_stroke(
        circle,
        8.5,
        egui::Stroke::new(
            if selected { 2.0 } else { 1.5 },
            if selected {
                UiTheme::blue()
            } else if response.hovered() {
                UiTheme::border_active()
            } else {
                egui::Color32::from_rgb(124, 153, 197)
            },
        ),
    );
    if selected {
        ui.painter().circle_filled(circle, 4.7, UiTheme::blue());
    }
    ui.painter().text(
        egui::pos2(rect.left() + 27.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(13.0),
        UiTheme::text(),
    );
    response
}

fn draw_compact_switch(
    ui: &mut egui::Ui,
    value: &mut bool,
    enabled: bool,
) -> egui::Response {
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(38.0, 18.0), sense);
    if enabled && response.clicked() {
        *value = !*value;
    }

    let track = if *value {
        UiTheme::blue()
    } else {
        egui::Color32::from_rgb(202, 212, 226)
    };
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(9), track);
    let knob_x = if *value {
        rect.right() - 8.5
    } else {
        rect.left() + 8.5
    };
    ui.painter().circle_filled(
        egui::pos2(knob_x, rect.center().y),
        6.3,
        egui::Color32::WHITE,
    );

    response
}

fn show_hires_tooltip(
    response: egui::Response,
    title: &str,
    body: &str,
) -> egui::Response {
    response.on_hover_ui(|ui| {
        ui.set_max_width(330.0);
        ui.label(
            egui::RichText::new(title)
                .size(13.0)
                .strong()
                .color(UiTheme::text()),
        );
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(body)
                .size(11.5)
                .color(UiTheme::text_soft()),
        );
    })
}

fn draw_info_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
    let color = if response.is_pointer_button_down_on() {
        UiTheme::blue_pressed()
    } else if response.hovered() {
        UiTheme::blue()
    } else {
        egui::Color32::from_rgb(120, 150, 194)
    };

    if response.hovered() {
        ui.painter().circle_filled(
            rect.center(),
            10.0,
            egui::Color32::from_rgba_unmultiplied(22, 119, 255, 18),
        );
    }

    ui.painter()
        .circle_stroke(rect.center(), 8.0, egui::Stroke::new(1.4, color));
    ui.painter().text(
        rect.center() + egui::vec2(0.0, 0.4),
        egui::Align2::CENTER_CENTER,
        "i",
        egui::FontId::proportional(11.0),
        color,
    );
    response
}

fn draw_info_popover(
    ctx: &egui::Context,
    id: &'static str,
    anchor: egui::Rect,
    text: &str,
) {
    let width = 350.0;
    let screen = ctx.screen_rect();
    let mut x = anchor.right() - width;
    x = x.clamp(screen.left() + 10.0, (screen.right() - width - 10.0).max(screen.left() + 10.0));
    let pos = egui::pos2(x, anchor.bottom() + 7.0);

    egui::Area::new(egui::Id::new(id))
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(253, 254, 255))
                .stroke(egui::Stroke::new(1.0, UiTheme::border()))
                .corner_radius(egui::CornerRadius::same(11))
                .shadow(egui::epaint::Shadow {
                    offset: [0, 4],
                    blur: 16,
                    spread: 0,
                    color: egui::Color32::from_black_alpha(22),
                })
                .inner_margin(egui::Margin::symmetric(12, 9))
                .show(ui, |ui| {
                    ui.set_width(width - 24.0);
                    ui.label(
                        egui::RichText::new(text)
                            .size(11.8)
                            .color(UiTheme::text()),
                    );
                });
        });
}

fn draw_status_badge(ui: &mut egui::Ui, text: &str, tone: StatusTone) {
    let (bg, dot, text_color) = tone.colors();
    let width = ui.available_width().clamp(104.0, 122.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 28.0), egui::Sense::hover());

    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(14), bg);

    let dot_center = egui::pos2(rect.left() + 14.0, rect.center().y);
    ui.painter().circle_filled(dot_center, 4.5, dot);

    ui.painter().text(
        egui::pos2(rect.left() + 25.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        egui::FontId::proportional(12.0),
        text_color,
    );
}

fn draw_device_art(
    ui: &mut egui::Ui,
    artwork: DeviceArtwork,
    _stereo_pair: bool,
    size: egui::Vec2,
) {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let center = rect.center() + egui::vec2(0.0, if response.hovered() { -1.0 } else { 0.0 });

    if response.hovered() {
        ui.painter().circle_filled(
            center,
            25.0,
            egui::Color32::from_rgba_unmultiplied(22, 119, 255, 10),
        );
    }

    let light = egui::Color32::from_rgb(136, 148, 166);
    let dark = egui::Color32::from_rgb(45, 52, 64);
    let neutral = egui::Color32::from_rgb(82, 100, 126);
    let silver = egui::Color32::from_rgb(104, 118, 139);

    macro_rules! put_svg {
        ($source:expr, $rect:expr, $tint:expr) => {{
            let image_rect = $rect;
            ui.put(
                image_rect,
                egui::Image::new($source)
                    .fit_to_exact_size(image_rect.size())
                    .tint($tint),
            );
        }};
    }

    match artwork {
        DeviceArtwork::HomePodMiniWhite => {
            let r = egui::Rect::from_center_size(center, egui::vec2(31.0, 31.0));
            put_svg!(egui::include_image!("../assets/mingcute_homepod_mini_filled.svg"), r, light);
        }
        DeviceArtwork::HomePodMiniBlack => {
            let r = egui::Rect::from_center_size(center, egui::vec2(31.0, 31.0));
            put_svg!(egui::include_image!("../assets/mingcute_homepod_mini_filled.svg"), r, dark);
        }
        DeviceArtwork::HomePodWhite => {
            let r = egui::Rect::from_center_size(center, egui::vec2(27.0, 34.0));
            put_svg!(egui::include_image!("../assets/sairplay_homepod_filled.svg"), r, light);
        }
        DeviceArtwork::HomePodBlack => {
            let r = egui::Rect::from_center_size(center, egui::vec2(27.0, 34.0));
            put_svg!(egui::include_image!("../assets/sairplay_homepod_filled.svg"), r, dark);
        }
        DeviceArtwork::HomePodMiniPairWhite
        | DeviceArtwork::HomePodMiniPairBlack
        | DeviceArtwork::HomePodMiniPairMixed => {
            let icon_size = egui::vec2(22.0, 22.0);
            let left = egui::Rect::from_center_size(center + egui::vec2(-11.5, 0.0), icon_size);
            let right = egui::Rect::from_center_size(center + egui::vec2(11.5, 0.0), icon_size);
            let (left_tint, right_tint) = match artwork {
                DeviceArtwork::HomePodMiniPairWhite => (light, light),
                DeviceArtwork::HomePodMiniPairBlack => (dark, dark),
                _ => (light, dark),
            };
            put_svg!(egui::include_image!("../assets/mingcute_homepod_mini_filled.svg"), left, left_tint);
            put_svg!(egui::include_image!("../assets/mingcute_homepod_mini_filled.svg"), right, right_tint);
        }
        DeviceArtwork::HomePodPairWhite
        | DeviceArtwork::HomePodPairBlack
        | DeviceArtwork::HomePodPairMixed => {
            let icon_size = egui::vec2(19.0, 27.0);
            let left = egui::Rect::from_center_size(center + egui::vec2(-10.5, 0.0), icon_size);
            let right = egui::Rect::from_center_size(center + egui::vec2(10.5, 0.0), icon_size);
            let (left_tint, right_tint) = match artwork {
                DeviceArtwork::HomePodPairWhite => (light, light),
                DeviceArtwork::HomePodPairBlack => (dark, dark),
                _ => (light, dark),
            };
            put_svg!(egui::include_image!("../assets/sairplay_homepod_filled.svg"), left, left_tint);
            put_svg!(egui::include_image!("../assets/sairplay_homepod_filled.svg"), right, right_tint);
        }
        DeviceArtwork::MacBook => {
            let r = egui::Rect::from_center_size(center, egui::vec2(34.0, 34.0));
            put_svg!(egui::include_image!("../assets/mingcute_laptop_filled.svg"), r, neutral);
        }
        DeviceArtwork::MacMini => {
            let r = egui::Rect::from_center_size(center, egui::vec2(36.0, 29.0));
            put_svg!(egui::include_image!("../assets/sairplay_mac_mini_filled.svg"), r, silver);
        }
        DeviceArtwork::MusicServer => {
            let r = egui::Rect::from_center_size(center, egui::vec2(30.0, 30.0));
            put_svg!(egui::include_image!("../assets/fluent_server_24_filled.svg"), r, neutral);
        }
        DeviceArtwork::AirportExpress => {
            let r = egui::Rect::from_center_size(center, egui::vec2(35.0, 28.0));
            put_svg!(egui::include_image!("../assets/sairplay_airport_express_filled.svg"), r, light);
        }
        DeviceArtwork::Tv => {
            let r = egui::Rect::from_center_size(center, egui::vec2(36.0, 32.0));
            put_svg!(egui::include_image!("../assets/sairplay_tv_filled.svg"), r, neutral);
        }
        DeviceArtwork::AppleTv => {
            let r = egui::Rect::from_center_size(center, egui::vec2(36.0, 28.0));
            put_svg!(egui::include_image!("../assets/sairplay_apple_tv_filled.svg"), r, dark);
        }
        DeviceArtwork::AirplaySpeakers => {
            let icon_size = egui::vec2(17.0, 28.0);
            let left = egui::Rect::from_center_size(center + egui::vec2(-9.5, 0.0), icon_size);
            let right = egui::Rect::from_center_size(center + egui::vec2(9.5, 0.0), icon_size);
            put_svg!(egui::include_image!("../assets/sairplay_bookshelf_speaker_filled.svg"), left, dark);
            put_svg!(egui::include_image!("../assets/sairplay_bookshelf_speaker_filled.svg"), right, dark);
        }
    }
}

fn legacy_config_for_device(
    device: &DeviceRecord,
    initial_volume: Option<u8>,
    secret: Option<&str>,
) -> Result<LegacyMemberConfig, String> {
    let route = device.route(false, false);
    if !matches!(route, Route::Raop | Route::AirPlay2Compat) {
        return Err(format!(
            "{} is not a source-compatible RAOP route",
            device.display_name
        ));
    }

    let service = device
        .endpoint_for_route(route)
        .ok_or_else(|| format!("{} has no endpoint for {route:?}", device.display_name))?;
    let props = device.raop.as_ref().unwrap_or(service);
    let host = preferred_service_address(service);

    let mut config = LegacyMemberConfig::new(
        device.display_name.clone(),
        host,
        service.port,
    );
    config.volume = initial_volume.unwrap_or(50).min(100);
    config.et = props
        .txt
        .fields
        .get("et")
        .cloned()
        .unwrap_or_else(|| "0,4".into());
    config.md = props
        .txt
        .fields
        .get("md")
        .cloned()
        .unwrap_or_else(|| "0,1,2".into());
    config.am = props
        .txt
        .fields
        .get("am")
        .or(props.txt.fields.get("model"))
        .cloned()
        .unwrap_or_default();
    config.pk = props
        .txt
        .fields
        .get("pk")
        .cloned()
        .unwrap_or_default();

    config.secret = secret
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned);

    let pairing_required = device
        .airplay
        .as_ref()
        .or(device.raop.as_ref())
        .is_some_and(|service| service.txt.pin_required() || service.txt.legacy_pairing());

    if pairing_required && config.secret.is_none() {
        return Err(format!(
            "{} requires legacy AirPlay PIN pairing (status flags advertise PIN/legacy pairing).",
            device.display_name
        ));
    }

    // Do not infer PIN/pairing requirements from AppleTV model + pk alone.
    // Embedded receivers can advertise AppleTV-class identity and a public key
    // while exposing no PIN UI and no PIN/legacy-pairing status flags. Only the
    // explicit pairing_required check above is authoritative for blocking.
    config.mfi_auth = config.am.to_ascii_lowercase().contains("airport");

    config.cn = props
        .txt
        .fields
        .get("cn")
        .cloned()
        .unwrap_or_default();
    if !config.cn.is_empty() {
        config.compressed_alac = config
            .cn
            .split(',')
            .any(|value| value.trim() == "1");
    }

    Ok(config)
}

fn native_config_for_device(
    device: &DeviceRecord,
    initial_volume: Option<u8>,
    hires_override: Option<bool>,
) -> Result<NativeSessionConfig, String> {
    let service = device
        .airplay
        .as_ref()
        .ok_or_else(|| format!("{} has no AirPlay service", device.display_name))?;
    let host = preferred_service_address(service);
    let mut config = NativeSessionConfig::new(host, service.port);
    config.dacp_id = "A1B2C3D4E5F60708".into();
    config.active_remote = "123456789".into();
    config.supports_ptp = service.txt.supports_ptp();
    config.follow_receiver_clock = service.txt.follows_receiver_clock();
    config.apple_model = service.txt.is_apple_model();
    config.receiver_name = device.display_name.clone();
    config.initial_volume = initial_volume;
    config.hires_enabled = hires_override.unwrap_or(false);

    // Source-aligned rate policy ported from Music Assistant:
    // - non-hi-res AirPlay stays at the 44.1/16 baseline;
    // - a hi-res AirPlay 2 stream follows the shared/source session rate when
    //   that rate is one of the supported 44.1/48 kHz rates;
    // - any other source rate falls back to 44.1 kHz.
    //
    // SAirplay2's source is Windows system audio, so the shared-mode render
    // engine mix rate is the Windows equivalent of MA's session PCM rate.
    let source_mix_rate = WasapiLoopbackCapture::default_render_mix_sample_rate()
        .unwrap_or(44_100);
    config.session_sample_rate = if config.hires_enabled {
        match source_mix_rate {
            44_100 | 48_000 => source_mix_rate,
            _ => 44_100,
        }
    } else {
        44_100
    };

    Ok(config)
}

fn volume_settings_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(base)
            .join("SolYan")
            .join("SAirplay2")
            .join("settings.txt"),
    )
}

fn load_saved_volume() -> Option<u8> {
    let path = volume_settings_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    text.lines().find_map(|line| {
        let value = line.strip_prefix("volume=")?.trim().parse::<u8>().ok()?;
        (value <= 100).then_some(value)
    })
}

fn save_volume(volume: u8) {
    let Some(path) = volume_settings_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let _ = std::fs::write(path, format!("volume={}\n", volume.min(100)));
}

fn parse_volume_text(value: &str) -> Result<Option<u8>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    match value.parse::<u8>() {
        Ok(volume) if volume <= 100 => Ok(Some(volume)),
        _ => Err("Receiver volume must be 0–100 or blank".into()),
    }
}

fn preferred_service_address(service: &DiscoveredService) -> String {
    service
        .addresses
        .iter()
        .filter_map(|raw| raw.parse::<IpAddr>().ok().map(|ip| (address_rank(ip), raw)))
        .min_by_key(|(rank, _)| *rank)
        .and_then(|(rank, raw)| {
            // Link-local/loopback addresses are not valid receiver targets for
            // a normal LAN session. Let the mDNS hostname resolve instead.
            (rank < 40).then(|| raw.clone())
        })
        .unwrap_or_else(|| service.host.trim_end_matches('.').to_string())
}

fn address_rank(ip: IpAddr) -> u8 {
    match ip {
        IpAddr::V4(ip) if ip.is_private() && !ip.is_link_local() => 0,
        IpAddr::V4(ip)
            if !ip.is_link_local() && !ip.is_loopback() && !ip.is_unspecified() =>
        {
            10
        }
        IpAddr::V6(ip)
            if !ip.is_unicast_link_local() && !ip.is_loopback() && !ip.is_unspecified() =>
        {
            20
        }
        _ => 40,
    }
}

#[cfg(test)]
mod gui_tests {
    use super::*;
    use sairplay_engine::AirPlayTxt;

    #[test]
    fn all_device_artwork_assets_are_independent_valid_pngs() {
        assert_eq!(ALL_DEVICE_ARTWORK.len(), 17);

        for artwork in ALL_DEVICE_ARTWORK {
            let decoded = image::load_from_memory(device_artwork_bytes(artwork))
                .unwrap_or_else(|err| panic!("{} failed to decode: {err}", device_artwork_name(artwork)));
            assert_eq!(
                (decoded.width(), decoded.height()),
                (256, 256),
                "{} has wrong dimensions",
                device_artwork_name(artwork),
            );
        }
    }

    fn service(addresses: &[&str]) -> DiscoveredService {
        DiscoveredService {
            kind: ServiceKind::AirPlay,
            fullname: "Test._airplay._tcp.local.".into(),
            display_name: "Test".into(),
            host: "Test.local.".into(),
            port: 7000,
            addresses: addresses.iter().map(|v| (*v).to_string()).collect(),
            txt: AirPlayTxt::default(),
        }
    }

    #[test]
    fn volume_parser_preserves_source_zero_and_blank_semantics() {
        assert_eq!(parse_volume_text("").unwrap(), None);
        assert_eq!(parse_volume_text("0").unwrap(), Some(0));
        assert_eq!(parse_volume_text("50").unwrap(), Some(50));
        assert_eq!(parse_volume_text("100").unwrap(), Some(100));
        assert!(parse_volume_text("101").is_err());
        assert!(parse_volume_text("-1").is_err());
    }

    #[test]
    fn lan_ipv4_beats_windows_link_local_address() {
        let s = service(&["169.254.2.72", "192.168.88.72"]);
        assert_eq!(preferred_service_address(&s), "192.168.88.72");
    }

    #[test]
    fn hostname_is_used_when_only_link_local_addresses_exist() {
        let s = service(&["169.254.2.72"]);
        assert_eq!(preferred_service_address(&s), "Test.local");
    }

    fn homepod_device(
        name: &str,
        address: &str,
        device_id: &str,
        tsid: Option<&str>,
        group_name: Option<&str>,
        tsm: Option<&str>,
    ) -> DeviceRecord {
        let mut fields = vec![
            ("model", "AudioAccessory5,1"),
            ("features", "274877906944"),
            ("deviceid", device_id),
        ];
        if let Some(value) = tsid {
            fields.push(("tsid", value));
        }
        if let Some(value) = group_name {
            fields.push(("gpn", value));
        }
        if let Some(value) = tsm {
            fields.push(("tsm", value));
        }

        DeviceRecord {
            display_name: name.into(),
            airplay: Some(DiscoveredService {
                kind: ServiceKind::AirPlay,
                fullname: format!("{name}._airplay._tcp.local."),
                display_name: name.into(),
                host: format!("{}.local.", name.replace(' ', "-")),
                port: 7000,
                addresses: vec![address.into()],
                txt: AirPlayTxt::parse(fields).unwrap(),
            }),
            raop: None,
        }
    }

    #[test]
    fn single_homepod_with_tsid_does_not_create_pair_row() {
        let device = homepod_device(
            "White",
            "192.168.1.30",
            "AA:BB:CC:DD:EE:01",
            Some("stereo-group-1"),
            Some("Living Room"),
            None,
        );

        assert_eq!(build_homepod_stereo_pairs(&[device]).len(), 0);
    }

    #[test]
    fn two_distinct_homepods_with_same_tsid_create_one_pair_row() {
        let left = homepod_device(
            "White",
            "192.168.1.30",
            "AA:BB:CC:DD:EE:01",
            Some("stereo-group-1"),
            Some("Living Room"),
            Some("0"),
        );
        let right = homepod_device(
            "Black",
            "192.168.1.31",
            "AA:BB:CC:DD:EE:02",
            Some("stereo-group-1"),
            Some("Living Room"),
            Some("1"),
        );

        let pairs = build_homepod_stereo_pairs(&[left, right]);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].display_name, "Living Room");
        assert_eq!(
            pairs[0]
                .airplay
                .as_ref()
                .and_then(|service| service.txt.fields.get("deviceid"))
                .map(String::as_str),
            Some("AA:BB:CC:DD:EE:02")
        );
    }

    #[test]
    fn homepods_with_different_tsid_do_not_create_pair_row() {
        let one = homepod_device(
            "White",
            "192.168.1.30",
            "AA:BB:CC:DD:EE:01",
            Some("stereo-group-1"),
            Some("Living Room"),
            None,
        );
        let two = homepod_device(
            "Black",
            "192.168.1.31",
            "AA:BB:CC:DD:EE:02",
            Some("stereo-group-2"),
            Some("Bedroom"),
            None,
        );

        assert_eq!(build_homepod_stereo_pairs(&[one, two]).len(), 0);
    }

    #[test]
    fn unknown_model_uses_music_server_fallback_artwork() {
        let device = DeviceRecord {
            display_name: "Unknown Receiver".into(),
            airplay: Some(service(&["192.168.1.44"])),
            raop: None,
        };
        assert_eq!(
            classify_device_artwork(&device),
            DeviceArtwork::MusicServer
        );
    }
}

fn approved_app_icon_data() -> egui::IconData {
    let decoded = image::load_from_memory(include_bytes!("../assets/sairplay2-logo.png"))
        .expect("decode approved SAirplay2 logo")
        .resize_exact(256, 256, image::imageops::FilterType::Lanczos3)
        .into_rgba8();
    let mut rgba = decoded.into_raw();
    apply_round_alpha_mask(&mut rgba, 256, 256, 0.205);
    egui::IconData {
        rgba,
        width: 256,
        height: 256,
    }
}

fn apply_round_alpha_mask(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    radius_fraction: f32,
) {
    let radius = width.min(height) as f32 * radius_fraction;
    let max_x = width as f32 - 1.0;
    let max_y = height as f32 - 1.0;

    for y in 0..height {
        for x in 0..width {
            let fx = x as f32;
            let fy = y as f32;
            let dx = if fx < radius {
                radius - fx
            } else if fx > max_x - radius {
                fx - (max_x - radius)
            } else {
                0.0
            };
            let dy = if fy < radius {
                radius - fy
            } else if fy > max_y - radius {
                fy - (max_y - radius)
            } else {
                0.0
            };
            if dx > 0.0 && dy > 0.0 && dx * dx + dy * dy > radius * radius {
                rgba[((y * width + x) * 4 + 3) as usize] = 0;
            }
        }
    }
}

fn install_windows_ui_font(ctx: &egui::Context) {
    #[cfg(windows)]
    {
        let candidates = [
            r"C:\Windows\Fonts\segoeui.ttf",
            r"C:\Windows\Fonts\arial.ttf",
        ];

        for path in candidates {
            if let Ok(bytes) = std::fs::read(path) {
                let mut fonts = egui::FontDefinitions::default();
                fonts.font_data.insert(
                    "windows_ui".to_owned(),
                    egui::FontData::from_owned(bytes).into(),
                );
                fonts
                    .families
                    .entry(egui::FontFamily::Proportional)
                    .or_default()
                    .insert(0, "windows_ui".to_owned());
                ctx.set_fonts(fonts);
                return;
            }
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SAirplay2")
            .with_icon(approved_app_icon_data())
            .with_inner_size([960.0, 620.0])
            .with_min_inner_size([960.0, 620.0])
            .with_max_inner_size([960.0, 620.0])
            .with_resizable(false),
        ..Default::default()
    };

    eframe::run_native(
        "SAirplay2",
        options,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            install_windows_ui_font(&cc.egui_ctx);
            Ok(Box::new(SairplayApp::default()))
        }),
    )
}
