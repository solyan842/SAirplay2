#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use eframe::egui;
use sairplay_engine::{
    DeviceCatalog, DeviceRecord, DiscoveredService, DiscoveryEvent, MdnsBrowser, NativeSession,
    NativeSessionConfig, Route, ServiceKind, VolumeSetResult,
};
use std::collections::{BTreeMap, HashSet};
use std::net::IpAddr;
use std::sync::mpsc::{self, Receiver};
use std::thread;

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlaybackUiState {
    Idle,
    Connecting(String),
    Playing(String),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiLanguage {
    Vi,
    En,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    selected_fullname: Option<String>,
    playback: PlaybackUiState,
    connect_rx: Option<Receiver<Result<NativeSession, String>>>,
    session: Option<NativeSession>,
    initial_volume_text: String,
    volume_rx: Option<Receiver<Result<VolumeSetResult, String>>>,
    pending_volume: Option<u8>,
    last_audio_discontinuities: u64,
    last_rtx: (u64, u64, u64),
    language: UiLanguage,
    multiroom_enabled: bool,
    activation_open: bool,
    activation_key: String,
    last_feedback_error: Option<String>,
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

        Self {
            log: {
                log.shrink_to_fit();
                log
            },
            catalog: DeviceCatalog::default(),
            discovery,
            discovery_rx,
            selected_fullname: None,
            playback: PlaybackUiState::Idle,
            connect_rx: None,
            session: None,
            initial_volume_text: "50".into(),
            volume_rx: None,
            pending_volume: None,
            last_audio_discontinuities: 0,
            last_rtx: (0, 0, 0),
            language: UiLanguage::Vi,
            multiroom_enabled: false,
            activation_open: false,
            activation_key: String::new(),
            last_feedback_error: None,
        }
    }
}

impl SairplayApp {
    fn pump_discovery(&mut self) {
        let Some(rx) = &self.discovery_rx else {
            return;
        };

        while let Ok(event) = rx.try_recv() {
            match event {
                DiscoveryEvent::Upsert(service) => {
                    let kind = match service.kind {
                        ServiceKind::AirPlay => "AirPlay",
                        ServiceKind::Raop => "RAOP",
                    };
                    if service.kind == ServiceKind::AirPlay {
                        self.log.push(format!(
                            "mDNS {kind}: {} @ {}:{} · model={} · features=0x{:016X} · PTP={} · buffered={}",
                            service.display_name,
                            service.host,
                            service.port,
                            service.txt.model.as_deref().unwrap_or("-"),
                            service.txt.features,
                            service.txt.supports_ptp(),
                            service.txt.supports_buffered_audio(),
                        ));
                    } else {
                        self.log.push(format!(
                            "mDNS {kind}: {} @ {}:{}",
                            service.display_name, service.host, service.port
                        ));
                    }
                    self.catalog.upsert(service);
                }
                DiscoveryEvent::Removed { kind, fullname } => {
                    self.catalog.remove(kind, &fullname);
                    if self.selected_fullname.as_deref() == Some(fullname.as_str()) {
                        self.selected_fullname = None;
                    }
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
        self.selected_fullname = None;
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

    fn pump_connect_result(&mut self) {
        let Some(rx) = &self.connect_rx else {
            return;
        };

        match rx.try_recv() {
            Ok(Ok(session)) => {
                let name = match &self.playback {
                    PlaybackUiState::Connecting(name) => name.clone(),
                    _ => "receiver".into(),
                };

                if let Some(volume) = session.initial_volume_result() {
                    self.log.push(format!(
                        "{name}: receiver volume {}% = {:.2} dB · RTSP {}.",
                        volume.percent, volume.db, volume.status
                    ));
                }

                if session.is_ready() && session.audio_running() {
                    self.log.push(format!("{name}: transport Ready, Windows audio running."));
                    self.playback = PlaybackUiState::Playing(name);
                    self.session = Some(session);
                } else {
                    let message = format!(
                        "{name}: session returned without full Ready/audio state"
                    );
                    self.log.push(message.clone());
                    self.playback = PlaybackUiState::Error(message);
                }
                self.connect_rx = None;
            }
            Ok(Err(error)) => {
                self.log.push(format!("Connect failed: {error}"));
                self.playback = PlaybackUiState::Error(error);
                self.connect_rx = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.log.push("Connect worker ended unexpectedly.".into());
                self.playback =
                    PlaybackUiState::Error("Connect worker ended unexpectedly".into());
                self.connect_rx = None;
            }
        }
    }

    fn pump_volume_result(&mut self) {
        let Some(rx) = &self.volume_rx else {
            return;
        };

        let finished = match rx.try_recv() {
            Ok(Ok(result)) => {
                self.log.push(format!(
                    "Receiver volume {}% = {:.2} dB · RTSP {}.",
                    result.percent, result.db, result.status
                ));
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

        let control = session.volume_control();
        let (tx, rx) = mpsc::sync_channel(1);
        self.volume_rx = Some(rx);
        thread::Builder::new()
            .name("sairplay-volume".into())
            .spawn(move || {
                let result = control.set(volume).map_err(|e| format!("{e:?}"));
                let _ = tx.send(result);
            })
            .expect("failed to spawn volume worker");
    }

    fn monitor_running_session(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };

        let audio_discontinuities = session.audio_discontinuities();
        let rtx = session.retransmit_stats();
        for event in session.drain_startup_events() {
            self.log.push(event);
        }

        if audio_discontinuities > self.last_audio_discontinuities {
            let at_frame = session.audio_last_discontinuity_frame();
            let first_audio = session.audio_first_non_silent_frame();
            let at_ms = at_frame.map(|f| (f as f64 * 1000.0 / 44_100.0));
            let first_audio_ms = first_audio.map(|f| (f as f64 * 1000.0 / 44_100.0));
            self.log.push(format!(
                "Diagnostic: WASAPI discontinuity count {} -> {} · at-frame={:?} (~{:.1?} ms) · first-non-silent={:?} (~{:.1?} ms).",
                self.last_audio_discontinuities,
                audio_discontinuities,
                at_frame,
                at_ms,
                first_audio,
                first_audio_ms,
            ));
            self.last_audio_discontinuities = audio_discontinuities;
        }

        let rtx_now = (rtx.requested, rtx.answered, rtx.expired);
        if rtx_now != self.last_rtx {
            self.log.push(format!(
                "Diagnostic: RTX requested={} answered={} expired={}.",
                rtx.requested, rtx.answered, rtx.expired
            ));
            self.last_rtx = rtx_now;
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
                        self.playback = PlaybackUiState::Error(error);
                        self.session = None;
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
        if !matches!(self.playback, PlaybackUiState::Idle | PlaybackUiState::Error(_)) {
            return;
        }

        let devices = self.catalog.devices().to_vec();
        let selected = self.selected_fullname.as_deref();

        let Some(device) = devices.iter().find(|device| {
            device
                .airplay
                .as_ref()
                .is_some_and(|service| Some(service.fullname.as_str()) == selected)
        }) else {
            self.playback = PlaybackUiState::Error("Select an AirPlay 2 receiver first".into());
            return;
        };

        let route = device.route(false, false);
        if route != Route::AirPlay2Native {
            let message = format!(
                "{} currently resolves to {route:?}; this alpha path only starts native AirPlay 2",
                device.display_name
            );
            self.log.push(message.clone());
            self.playback = PlaybackUiState::Error(message);
            return;
        }

        let Some(service) = device.airplay.as_ref() else {
            self.playback = PlaybackUiState::Error("Selected receiver has no AirPlay service".into());
            return;
        };

        let host = preferred_service_address(service);
        let name = device.display_name.clone();
        let port = service.port;

        let initial_volume = match parse_volume_text(&self.initial_volume_text) {
            Ok(volume) => volume,
            Err(message) => {
                self.log.push(message.clone());
                self.playback = PlaybackUiState::Error(message);
                return;
            }
        };

        let mut config = NativeSessionConfig::new(host.clone(), port);
        // Fixed app identity for the clean alpha path; credentials remain absent
        // unless a later UI explicitly supplies them.
        config.dacp_id = "A1B2C3D4E5F60708".into();
        config.active_remote = "123456789".into();
        config.supports_ptp = service.txt.supports_ptp();
        config.follow_receiver_clock = service.txt.follows_receiver_clock();
        config.apple_model = service.txt.is_apple_model();
        config.receiver_name = name.clone();
        config.initial_volume = initial_volume;

        let (tx, rx) = mpsc::sync_channel(1);
        self.connect_rx = Some(rx);
        self.playback = PlaybackUiState::Connecting(name.clone());
        self.session = None;
        self.last_audio_discontinuities = 0;
        self.last_rtx = (0, 0, 0);
        self.last_feedback_error = None;
        self.log.push(format!(
            "{name}: preflight starting on {host}:{port} · model={} · features=0x{:016X} · PTP={} · follow-clock={} · initial-volume={} · Playing waits for Ready + audio.",
            service.txt.model.as_deref().unwrap_or("-"),
            service.txt.features,
            service.txt.supports_ptp(),
            service.txt.follows_receiver_clock(),
            initial_volume
                .map(|v| format!("{v}%"))
                .unwrap_or_else(|| "unchanged".into()),
        ));
        self.log.push(format!(
            "{name}: HomePod/group TXT · igl={} · pgid={} · tsid={} · tsm={} · gpn={} · osvers={} · srcvers={}.",
            service.txt.fields.get("igl").map(String::as_str).unwrap_or("-"),
            service.txt.fields.get("pgid").map(String::as_str).unwrap_or("-"),
            service.txt.fields.get("tsid").map(String::as_str).unwrap_or("-"),
            service.txt.fields.get("tsm").map(String::as_str).unwrap_or("-"),
            service.txt.fields.get("gpn").map(String::as_str).unwrap_or("-"),
            service.txt.fields.get("osvers").map(String::as_str).unwrap_or("-"),
            service.txt.fields
                .get("srcvers")
                .or_else(|| service.txt.fields.get("vs"))
                .map(String::as_str)
                .unwrap_or("-"),
        ));

        thread::Builder::new()
            .name("sairplay-native-connect".into())
            .spawn(move || {
                let result = (|| -> Result<NativeSession, String> {
                    let mut session =
                        NativeSession::connect(&config).map_err(|e| e.to_string())?;
                    session
                        .start_windows_audio()
                        .map_err(|e| e.to_string())?;
                    if !session.is_ready() || !session.audio_running() {
                        return Err(
                            "native transport or Windows capture did not reach ready state".into(),
                        );
                    }
                    Ok(session)
                })();
                let _ = tx.send(result);
            })
            .expect("failed to spawn native connect worker");
    }

    fn stop_playback(&mut self) {
        if self.session.take().is_some() {
            self.log.push("Playback stopped; native session resources released.".into());
        }
        self.playback = PlaybackUiState::Idle;
        self.last_audio_discontinuities = 0;
        self.last_rtx = (0, 0, 0);
    }

    fn header_status(&self) -> (&'static str, &'static str, egui::Color32) {
        match &self.playback {
            PlaybackUiState::Idle => (
                self.t("Sẵn sàng", "Ready"),
                self.t("Đang chờ phát nhạc...", "Waiting for playback..."),
                UiTheme::green(),
            ),
            PlaybackUiState::Connecting(_) => (
                self.t("Đang kết nối", "Connecting"),
                self.t("Đang chuẩn bị thiết bị...", "Preparing receiver..."),
                UiTheme::amber(),
            ),
            PlaybackUiState::Playing(_) => (
                self.t("Đang chạy", "Running"),
                self.t("Đang truyền âm thanh qua AirPlay", "Streaming via AirPlay"),
                UiTheme::green(),
            ),
            PlaybackUiState::Error(_) => (
                self.t("Lỗi kết nối", "Connection Error"),
                self.t("Xem Log để kiểm tra", "Open Log for details"),
                UiTheme::red(),
            ),
        }
    }

    fn device_status(&self, device: &DeviceRecord, stereo_pair: bool) -> (&'static str, StatusTone) {
        let selected = device
            .airplay
            .as_ref()
            .is_some_and(|s| self.selected_fullname.as_deref() == Some(s.fullname.as_str()));

        match &self.playback {
            PlaybackUiState::Connecting(name) if name == &device.display_name => (
                self.t("Đang kết nối", "Connecting"),
                StatusTone::Orange,
            ),
            PlaybackUiState::Playing(name) if name == &device.display_name => (
                self.t("Đang chạy", "Running"),
                StatusTone::Green,
            ),
            PlaybackUiState::Error(_) if selected => (
                self.t("Lỗi kết nối", "Connection Error"),
                StatusTone::Red,
            ),
            _ if device.route(false, false) == Route::AirPlay2Native => {
                if stereo_pair {
                    (self.t("Sẵn sàng", "Ready"), StatusTone::Green)
                } else {
                    (self.t("Đang chờ", "Waiting"), StatusTone::Orange)
                }
            }
            _ => (self.t("Chờ", "Standby"), StatusTone::Gray),
        }
    }

    fn render_device_row(&mut self, ui: &mut egui::Ui, device: &DeviceRecord, stereo_pair: bool) {
        const ROW_H: f32 = 64.0;
        const SELECTOR_W: f32 = 24.0;
        const ART_W: f32 = 70.0;
        const STATUS_W: f32 = 126.0;

        let route = device.route(false, false);
        let fullname = device.airplay.as_ref().map(|s| s.fullname.clone());
        let selected = fullname
            .as_deref()
            .is_some_and(|name| self.selected_fullname.as_deref() == Some(name));
        let selectable = route == Route::AirPlay2Native
            && fullname.is_some()
            && !matches!(self.playback, PlaybackUiState::Connecting(_));

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
        let body = row_rect.shrink2(egui::vec2(2.0, 2.0));

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
                egui::CornerRadius::same(11),
                egui::Color32::from_rgb(247, 251, 255),
            );
            ui.painter().rect_stroke(
                body,
                egui::CornerRadius::same(11),
                egui::Stroke::new(1.0, UiTheme::border_hover()),
                egui::StrokeKind::Inside,
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
        ui.allocate_ui_at_rect(content, |ui| {
            ui.horizontal_centered(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(SELECTOR_W, 44.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        draw_selector(ui, selected, selectable);
                    },
                );

                ui.add_space(8.0);

                ui.allocate_ui_with_layout(
                    egui::vec2(ART_W, 44.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        draw_device_art(ui, artwork, stereo_pair, egui::vec2(68.0, 42.5));
                    },
                );

                ui.add_space(12.0);

                let text_width = (ui.available_width() - STATUS_W - 14.0).max(110.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(text_width, 44.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
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
                    },
                );

                ui.add_space(8.0);

                ui.allocate_ui_with_layout(
                    egui::vec2(STATUS_W, 44.0),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| draw_status_badge(ui, status, status_tone),
                );
            });
        });

        if response.clicked() && selectable {
            self.selected_fullname = fullname;
            if matches!(self.playback, PlaybackUiState::Error(_)) {
                self.playback = PlaybackUiState::Idle;
            }
        }
    }

    fn render_device_panel(
        &mut self,
        ui: &mut egui::Ui,
        title: &'static str,
        devices: &[DeviceRecord],
        stereo_pair: bool,
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
                ui.set_min_height(286.0);

                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 36.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        draw_small_airplay_mark(ui);
                        ui.add_space(5.0);
                        ui.label(
                            egui::RichText::new(title)
                                .size(19.0)
                                .strong()
                                .color(UiTheme::text()),
                        );

                        if !stereo_pair {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    draw_info_mark(ui);
                                    ui.add_space(5.0);
                                    let label = self.t("MultiRoom", "MultiRoom");
                                    if draw_multiroom_toggle(ui, label, self.multiroom_enabled).clicked() {
                                        self.multiroom_enabled = !self.multiroom_enabled;
                                        self.log.push(format!(
                                            "MultiRoom GUI mode {}. Transport wiring is intentionally unchanged.",
                                            if self.multiroom_enabled { "enabled" } else { "disabled" }
                                        ));
                                    }
                                },
                            );
                        }
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

                    egui::ScrollArea::vertical()
                        .id_salt(if stereo_pair { "pair_scroll" } else { "receiver_scroll" })
                        .max_height(224.0)
                        .min_scrolled_height(224.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if devices.is_empty() {
                                ui.allocate_ui_with_layout(
                                    egui::vec2(ui.available_width(), 190.0),
                                    egui::Layout::top_down(egui::Align::Center),
                                    |ui| {
                                        ui.add_space(48.0);
                                        let text = if stereo_pair {
                                            self.t(
                                                "Chưa phát hiện Stereo Pair HomePod",
                                                "No HomePod stereo pair detected",
                                            )
                                        } else {
                                            self.t(
                                                "Đang quét thiết bị AirPlay...",
                                                "Scanning AirPlay receivers...",
                                            )
                                        };
                                        ui.label(
                                            egui::RichText::new(text)
                                                .size(12.5)
                                                .color(UiTheme::text_soft()),
                                        );
                                    },
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
        const CARD_INNER_W: f32 = 286.0;
        const CARD_INNER_H: f32 = 42.0;
        const CARD_GAP: f32 = 7.0;

        ui.horizontal(|ui| {
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
                .show(ui, |ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(CARD_INNER_W, CARD_INNER_H),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            draw_speaker_icon(ui, egui::vec2(25.0, 25.0));
                            ui.add_space(6.0);

                            ui.vertical(|ui| {
                                ui.label(
                                    egui::RichText::new(self.t("Âm lượng Receiver", "Receiver Volume"))
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
                                        self.apply_volume_value(volume);
                                    }

                                    egui::Frame::new()
                                        .fill(egui::Color32::from_rgb(242, 246, 251))
                                        .corner_radius(egui::CornerRadius::same(6))
                                        .inner_margin(egui::Margin::symmetric(7, 3))
                                        .show(ui, |ui| {
                                            ui.label(
                                                egui::RichText::new(format!("{volume}%"))
                                                    .size(11.0)
                                                    .strong()
                                                    .color(UiTheme::text_soft()),
                                            );
                                        });

                                    if self.volume_rx.is_some() {
                                        ui.spinner();
                                    }
                                });
                            });
                        },
                    );
                });

            ui.add_space(CARD_GAP);

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
                .show(ui, |ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(CARD_INNER_W, CARD_INNER_H),
                        egui::Layout::left_to_right(egui::Align::Center)
                            .with_main_align(egui::Align::Center),
                        |ui| {
                            let start_enabled = self.selected_fullname.is_some()
                                && !self.multiroom_enabled
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

            ui.add_space(CARD_GAP);

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
                .show(ui, |ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(CARD_INNER_W, CARD_INNER_H),
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

                                let detail = if self.multiroom_enabled {
                                    self.t(
                                        "MultiRoom · giao diện đã sẵn sàng",
                                        "MultiRoom · UI prepared",
                                    )
                                } else {
                                    self.t("Sẵn sàng truyền · ALAC", "Ready to stream · ALAC")
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
        self.pump_connect_result();
        self.pump_volume_result();
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
                                    egui::RichText::new(
                                        "Native AirPlay — Apple's lossless wireless audio transport using ALAC",
                                    )
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
                                draw_header_status_card(ui, status, detail, color);
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

                let receivers_title = self.t("Receivers", "Receivers");
                let pairs_title = self.t("Stereo Pair HomePod", "Stereo Pair HomePod");
                ui.columns(2, |columns| {
                    self.render_device_panel(
                        &mut columns[0],
                        receivers_title,
                        &receivers,
                        false,
                    );
                    self.render_device_panel(
                        &mut columns[1],
                        pairs_title,
                        &stereo_pairs,
                        true,
                    );
                });

                ui.add_space(7.0);
                self.render_controls(ui);
                ui.add_space(6.0);
                self.render_trial_row(ui);
            });

        self.render_activation_window(ctx);
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
}

fn draw_header_status_card(
    ui: &mut egui::Ui,
    status: &str,
    detail: &str,
    color: egui::Color32,
) {
    egui::Frame::new()
        .fill(UiTheme::surface())
        .stroke(egui::Stroke::new(1.0, UiTheme::border()))
        .corner_radius(egui::CornerRadius::same(26))
        .shadow(egui::epaint::Shadow {
            offset: [0, 2],
            blur: 12,
            spread: 0,
            color: egui::Color32::from_black_alpha(16),
        })
        .inner_margin(egui::Margin::symmetric(14, 9))
        .show(ui, |ui| {
            ui.set_min_width(174.0);
            ui.set_min_height(38.0);
            ui.horizontal_centered(|ui| {
                let (dot_rect, _) =
                    ui.allocate_exact_size(egui::vec2(17.0, 17.0), egui::Sense::hover());
                ui.painter().circle_filled(dot_rect.center(), 7.0, color);
                ui.add_space(7.0);
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(status)
                            .size(14.0)
                            .strong()
                            .color(UiTheme::text()),
                    );
                    ui.label(
                        egui::RichText::new(detail)
                            .size(10.5)
                            .color(UiTheme::text_soft()),
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

        let mut pair = representative.clone();
        pair.display_name = pair_name;
        if let Some(service) = pair.airplay.as_mut() {
            service
                .txt
                .fields
                .insert("sairplay-pair-art".to_owned(), pair_art.to_owned());
        }
        pairs.push(pair);
    }

    pairs
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

fn device_sprite_cell(artwork: DeviceArtwork) -> usize {
    match artwork {
        DeviceArtwork::HomePodMiniWhite => 0,
        DeviceArtwork::HomePodMiniBlack => 1,
        DeviceArtwork::HomePodWhite => 2,
        DeviceArtwork::HomePodBlack => 3,
        DeviceArtwork::HomePodMiniPairWhite => 4,
        DeviceArtwork::HomePodMiniPairBlack => 5,
        DeviceArtwork::HomePodMiniPairMixed => 6,
        DeviceArtwork::MacBook => 7,
        DeviceArtwork::MacMini => 8,
        DeviceArtwork::MusicServer => 9,
        DeviceArtwork::AirportExpress => 10,
        DeviceArtwork::Tv => 11,
        DeviceArtwork::HomePodPairWhite => 12,
        DeviceArtwork::HomePodPairBlack => 13,
        DeviceArtwork::HomePodPairMixed => 14,
        DeviceArtwork::AppleTv => 15,
        DeviceArtwork::AirplaySpeakers => 16,
    }
}

fn sprite_uv(cell: usize) -> egui::Rect {
    const COLS: f32 = 5.0;
    const ROWS: f32 = 4.0;
    let col = (cell % 5) as f32;
    let row = (cell / 5) as f32;
    egui::Rect::from_min_max(
        egui::pos2(col / COLS, row / ROWS),
        egui::pos2((col + 1.0) / COLS, (row + 1.0) / ROWS),
    )
}

fn draw_app_logo(ui: &mut egui::Ui, size: egui::Vec2) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter_at(rect);
    let blue = if response.is_pointer_button_down_on() {
        UiTheme::blue_pressed()
    } else if response.hovered() {
        UiTheme::blue_hover()
    } else {
        UiTheme::blue()
    };

    if response.hovered() {
        painter.rect_filled(
            rect.expand(3.0),
            egui::CornerRadius::same(20),
            egui::Color32::from_rgba_unmultiplied(20, 126, 246, 28),
        );
    }

    painter.rect_filled(rect, egui::CornerRadius::same(17), blue);
    let center = rect.center() + egui::vec2(0.0, 2.0);
    painter.circle_stroke(center, 21.0, egui::Stroke::new(3.5, egui::Color32::WHITE));
    painter.circle_stroke(center, 13.5, egui::Stroke::new(2.6, egui::Color32::WHITE));
    painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(rect.left() + 7.0, center.y + 4.0),
            egui::pos2(rect.right() - 7.0, rect.bottom() - 6.0),
        ),
        egui::CornerRadius::same(0),
        blue,
    );
    painter.line_segment(
        [
            egui::pos2(center.x, center.y - 20.0),
            egui::pos2(center.x, center.y + 16.0),
        ],
        egui::Stroke::new(2.8, egui::Color32::WHITE),
    );
    painter.circle_filled(
        egui::pos2(center.x, center.y + 16.0),
        4.5,
        egui::Color32::WHITE,
    );
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
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(100.0, 26.0), egui::Sense::click());
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

fn draw_info_mark(ui: &mut egui::Ui) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::hover());
    let color = if response.hovered() {
        UiTheme::blue()
    } else {
        egui::Color32::from_rgb(120, 150, 194)
    };
    ui.painter()
        .circle_stroke(rect.center(), 8.0, egui::Stroke::new(1.3, color));
    ui.painter().text(
        rect.center() + egui::vec2(0.0, 0.4),
        egui::Align2::CENTER_CENTER,
        "i",
        egui::FontId::proportional(11.0),
        color,
    );
}

fn draw_status_badge(ui: &mut egui::Ui, text: &str, tone: StatusTone) {
    let (bg, dot, text_color) = tone.colors();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(122.0, 30.0), egui::Sense::hover());

    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(15), bg);

    let dot_center = egui::pos2(rect.left() + 16.0, rect.center().y);
    ui.painter().circle_filled(dot_center, 5.0, dot);

    ui.painter().text(
        egui::pos2(rect.left() + 28.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        egui::FontId::proportional(12.5),
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

    // All approved artwork was normalized into the same 64x40 sprite cell.
    // Keep aspect ratio exactly 8:5 so product proportions never stretch.
    let target = egui::Rect::from_center_size(
        rect.center() + egui::vec2(0.0, if response.hovered() { -1.0 } else { 0.0 }),
        egui::vec2(size.x.min(72.0), size.x.min(72.0) * 0.625),
    );

    if response.hovered() {
        ui.painter().rect_filled(
            target.expand(3.0),
            egui::CornerRadius::same(10),
            egui::Color32::from_rgba_unmultiplied(22, 119, 255, 12),
        );
    }

    ui.put(
        target,
        egui::Image::new(egui::include_image!("../assets/device_icons_sprite.png"))
            .uv(sprite_uv(device_sprite_cell(artwork)))
            .fit_to_exact_size(target.size()),
    );
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
