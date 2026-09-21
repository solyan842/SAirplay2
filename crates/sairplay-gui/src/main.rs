#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use eframe::egui;
use sairplay_engine::{
    DeviceCatalog, DeviceRecord, DiscoveredService, DiscoveryEvent, MdnsBrowser, NativeSession,
    NativeSessionConfig, Route, ServiceKind, VolumeSetResult,
};
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
    AirportExpress,
    HomePodLight,
    HomePodDark,
    MacBook,
    MusicServer,
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
        } else if let Some(error) = session.feedback_error() {
            self.log.push(format!("Feedback keepalive: {error}"));
            if !session.feedback_running() {
                self.playback = PlaybackUiState::Error(error);
                self.session = None;
            }
        } else if !session.feedback_running() {
            let error = "Feedback keepalive worker stopped".to_string();
            self.log.push(error.clone());
            self.playback = PlaybackUiState::Error(error);
            self.session = None;
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
                egui::Color32::from_rgb(18, 185, 91),
            ),
            PlaybackUiState::Connecting(_) => (
                self.t("Đang kết nối", "Connecting"),
                self.t("Đang chuẩn bị thiết bị...", "Preparing receiver..."),
                egui::Color32::from_rgb(241, 158, 0),
            ),
            PlaybackUiState::Playing(_) => (
                self.t("Đang chạy", "Running"),
                self.t("Đang truyền âm thanh qua AirPlay", "Streaming via AirPlay"),
                egui::Color32::from_rgb(18, 185, 91),
            ),
            PlaybackUiState::Error(_) => (
                self.t("Lỗi kết nối", "Connection Error"),
                self.t("Xem Log để kiểm tra", "Open Log for details"),
                egui::Color32::from_rgb(229, 57, 68),
            ),
        }
    }

    fn device_status(&self, device: &DeviceRecord, stereo_pair: bool) -> (&'static str, egui::Color32) {
        let selected = device
            .airplay
            .as_ref()
            .is_some_and(|s| self.selected_fullname.as_deref() == Some(s.fullname.as_str()));

        match &self.playback {
            PlaybackUiState::Connecting(name) if name == &device.display_name => (
                self.t("Đang kết nối", "Connecting"),
                egui::Color32::from_rgb(241, 158, 0),
            ),
            PlaybackUiState::Playing(name) if name == &device.display_name => (
                self.t("Đang chạy", "Running"),
                egui::Color32::from_rgb(18, 185, 91),
            ),
            PlaybackUiState::Error(_) if selected => (
                self.t("Lỗi kết nối", "Connection Error"),
                egui::Color32::from_rgb(229, 57, 68),
            ),
            _ if device.route(false, false) == Route::AirPlay2Native => {
                if stereo_pair {
                    (self.t("Sẵn sàng", "Ready"), egui::Color32::from_rgb(18, 185, 91))
                } else {
                    (self.t("Đang chờ", "Waiting"), egui::Color32::from_rgb(241, 158, 0))
                }
            }
            _ => (
                self.t("Chờ", "Standby"),
                egui::Color32::from_rgb(104, 124, 154),
            ),
        }
    }

    fn render_device_row(&mut self, ui: &mut egui::Ui, device: &DeviceRecord, stereo_pair: bool) {
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
        let (status, status_color) = self.device_status(device, stereo_pair);

        egui::Frame::new()
            .fill(if selected {
                egui::Color32::from_rgb(240, 248, 255)
            } else {
                egui::Color32::WHITE
            })
            .stroke(egui::Stroke::new(
                if selected { 1.4 } else { 1.0 },
                if selected {
                    egui::Color32::from_rgb(165, 211, 255)
                } else {
                    egui::Color32::from_rgb(223, 232, 243)
                },
            ))
            .corner_radius(egui::CornerRadius::same(12))
            .inner_margin(egui::Margin::symmetric(12, 9))
            .show(ui, |ui| {
                ui.set_min_height(56.0);
                ui.horizontal(|ui| {
                    let response = ui.add_enabled(
                        selectable,
                        egui::RadioButton::new(selected, ""),
                    );
                    if response.clicked() {
                        self.selected_fullname = fullname.clone();
                        if matches!(self.playback, PlaybackUiState::Error(_)) {
                            self.playback = PlaybackUiState::Idle;
                        }
                    }

                    ui.add_space(2.0);
                    draw_device_art(ui, artwork, stereo_pair, egui::vec2(68.0, 44.0));
                    ui.add_space(6.0);

                    ui.vertical(|ui| {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(&device.display_name)
                                .size(14.5)
                                .strong()
                                .color(egui::Color32::from_rgb(20, 31, 51)),
                        );
                        ui.label(
                            egui::RichText::new(address)
                                .size(12.0)
                                .color(egui::Color32::from_rgb(70, 106, 165)),
                        );
                    });

                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            draw_status_badge(ui, status, status_color);
                        },
                    );
                });
            });

        ui.add_space(7.0);
    }

    fn render_device_panel(
        &mut self,
        ui: &mut egui::Ui,
        title: &'static str,
        devices: &[DeviceRecord],
        stereo_pair: bool,
    ) {
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(252, 254, 255))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(210, 225, 242)))
            .corner_radius(egui::CornerRadius::same(14))
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    draw_small_airplay_mark(ui);
                    ui.label(
                        egui::RichText::new(title)
                            .size(18.0)
                            .strong()
                            .color(egui::Color32::from_rgb(13, 28, 54)),
                    );

                    if !stereo_pair {
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                let label = self.t("MultiRoom", "MultiRoom");
                                let toggle = ui.add(
                                    egui::RadioButton::new(
                                        self.multiroom_enabled,
                                        egui::RichText::new(label).size(15.0),
                                    ),
                                );
                                if toggle.clicked() {
                                    self.multiroom_enabled = !self.multiroom_enabled;
                                    self.log.push(format!(
                                        "MultiRoom GUI mode {}. Transport wiring is intentionally unchanged.",
                                        if self.multiroom_enabled { "enabled" } else { "disabled" }
                                    ));
                                }
                                toggle.on_hover_text(self.t(
                                    "Giao diện MultiRoom đã chuẩn bị; transport sẽ nối sau khi đường phát đơn ổn định.",
                                    "MultiRoom UI is prepared; transport wiring follows after single-room playback is stable.",
                                ));
                            },
                        );
                    }
                });

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(6.0);

                egui::ScrollArea::vertical()
                    .id_salt(if stereo_pair { "pair_scroll" } else { "receiver_scroll" })
                    .max_height(214.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if devices.is_empty() {
                            ui.add_space(20.0);
                            ui.vertical_centered(|ui| {
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
                                        .size(14.0)
                                        .color(egui::Color32::from_rgb(113, 130, 154)),
                                );
                            });
                        } else {
                            for device in devices {
                                self.render_device_row(ui, device, stereo_pair);
                            }
                        }
                    });
            });
    }

    fn render_controls(&mut self, ui: &mut egui::Ui) {
        egui::Frame::new()
            .fill(egui::Color32::WHITE)
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(210, 225, 242)))
            .corner_radius(egui::CornerRadius::same(13))
            .inner_margin(egui::Margin::symmetric(16, 10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("◖))")
                            .size(23.0)
                            .color(egui::Color32::from_rgb(67, 92, 131)),
                    );
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(self.t("Âm lượng Receiver", "Receiver Volume"))
                                .size(14.0)
                                .color(egui::Color32::from_rgb(38, 52, 75)),
                        );
                        let mut volume = parse_volume_text(&self.initial_volume_text)
                            .ok()
                            .flatten()
                            .unwrap_or(50);
                        let response = ui.add_sized(
                            [205.0, 20.0],
                            egui::Slider::new(&mut volume, 0..=100)
                                .show_value(true)
                                .suffix("%"),
                        );
                        if response.changed() {
                            self.initial_volume_text = volume.to_string();
                            self.apply_volume_value(volume);
                        }
                    });

                    if self.volume_rx.is_some() {
                        ui.spinner();
                    }

                    ui.separator();

                    let start_enabled = self.selected_fullname.is_some()
                        && !self.multiroom_enabled
                        && matches!(self.playback, PlaybackUiState::Idle | PlaybackUiState::Error(_));
                    let start_text = format!("▶  {}", self.t("Bắt đầu", "Start"));
                    if ui
                        .add_enabled(
                            start_enabled,
                            egui::Button::new(
                                egui::RichText::new(start_text)
                                    .size(16.0)
                                    .color(egui::Color32::WHITE),
                            )
                            .fill(egui::Color32::from_rgb(15, 128, 247))
                            .min_size(egui::vec2(112.0, 38.0)),
                        )
                        .clicked()
                    {
                        self.start_selected();
                    }

                    let stop_enabled = self.session.is_some();
                    let stop_text = format!("■  {}", self.t("Dừng", "Stop"));
                    if ui
                        .add_enabled(
                            stop_enabled,
                            egui::Button::new(egui::RichText::new(stop_text).size(16.0))
                                .min_size(egui::vec2(100.0, 38.0)),
                        )
                        .clicked()
                    {
                        self.stop_playback();
                    }

                    ui.separator();
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(self.t(
                                "Kết nối qua AirPlay 2",
                                "Connect via AirPlay 2",
                            ))
                            .size(15.0)
                            .strong()
                            .color(egui::Color32::from_rgb(22, 38, 65)),
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
                                .size(13.0)
                                .color(egui::Color32::from_rgb(82, 113, 162)),
                        );
                    });
                });
            });
    }

    fn render_trial_row(&mut self, ui: &mut egui::Ui) {
        let text = self.t("Dùng thử · còn 3 ngày", "Trial · 3 days left");
        let activate = self.t("Nhấn để kích hoạt", "Click to activate");
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(248, 252, 255))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(211, 226, 244)))
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(egui::Margin::symmetric(14, 7))
            .show(ui, |ui| {
                let inner = ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("⌕")
                            .size(18.0)
                            .color(egui::Color32::from_rgb(10, 120, 246)),
                    );
                    ui.label(egui::RichText::new(text).strong().size(14.0));
                    ui.separator();
                    ui.label(
                        egui::RichText::new(activate)
                            .size(14.0)
                            .color(egui::Color32::from_rgb(71, 106, 160)),
                    );
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.label(
                                egui::RichText::new("›")
                                    .size(24.0)
                                    .color(egui::Color32::from_rgb(55, 93, 151)),
                            );
                        },
                    );
                });
                let response = ui.interact(
                    inner.response.rect,
                    ui.make_persistent_id("activation_row"),
                    egui::Sense::click(),
                );
                if response.clicked() {
                    self.activation_open = true;
                }
            });
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
        visuals.panel_fill = egui::Color32::from_rgb(244, 249, 255);
        visuals.window_fill = egui::Color32::WHITE;
        visuals.extreme_bg_color = egui::Color32::from_rgb(238, 245, 253);
        visuals.selection.bg_fill = egui::Color32::from_rgb(31, 139, 255);
        visuals.selection.stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);
        ctx.set_visuals(visuals);

        egui::TopBottomPanel::bottom("app_footer")
            .exact_height(31.0)
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::WHITE)
                    .stroke(egui::Stroke::new(
                        1.0,
                        egui::Color32::from_rgb(218, 229, 242),
                    ))
                    .inner_margin(egui::Margin::symmetric(16, 5)),
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
                    .fill(egui::Color32::from_rgb(244, 249, 255))
                    .inner_margin(egui::Margin::same(18)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let logo = draw_app_logo(ui, egui::vec2(70.0, 70.0));
                    if logo
                        .on_hover_text(self.t(
                            "Nhấn để quét lại thiết bị",
                            "Click to scan devices again",
                        ))
                        .clicked()
                    {
                        self.rescan_devices();
                    }

                    ui.add_space(10.0);

                    ui.allocate_ui_with_layout(
                        egui::vec2(455.0, 70.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new("SAirplay2")
                                    .size(28.0)
                                    .strong()
                                    .color(egui::Color32::from_rgb(7, 19, 52)),
                            );
                            ui.label(
                                egui::RichText::new(
                                    "Native AirPlay — Apple's lossless wireless audio transport using ALAC",
                                )
                                .size(12.5)
                                .color(egui::Color32::from_rgb(79, 105, 154)),
                            );
                        },
                    );

                    ui.add_space(6.0);

                    let (status, detail, color) = self.header_status();
                    ui.allocate_ui_with_layout(
                        egui::vec2(205.0, 70.0),
                        egui::Layout::top_down(egui::Align::Center),
                        |ui| {
                            egui::Frame::new()
                                .fill(egui::Color32::WHITE)
                                .stroke(egui::Stroke::new(
                                    1.0,
                                    egui::Color32::from_rgb(211, 226, 243),
                                ))
                                .corner_radius(egui::CornerRadius::same(24))
                                .inner_margin(egui::Margin::symmetric(12, 9))
                                .show(ui, |ui| {
                                    ui.set_min_width(176.0);
                                    ui.horizontal(|ui| {
                                        let (dot_rect, _) = ui.allocate_exact_size(
                                            egui::vec2(14.0, 14.0),
                                            egui::Sense::hover(),
                                        );
                                        ui.painter().circle_filled(dot_rect.center(), 6.0, color);
                                        ui.vertical(|ui| {
                                            ui.label(
                                                egui::RichText::new(status)
                                                    .size(14.0)
                                                    .strong()
                                                    .color(egui::Color32::from_rgb(18, 29, 49)),
                                            );
                                            ui.label(
                                                egui::RichText::new(detail)
                                                    .size(10.5)
                                                    .color(egui::Color32::from_rgb(79, 105, 154)),
                                            );
                                        });
                                    });
                                });
                        },
                    );

                    ui.add_space(6.0);

                    ui.allocate_ui_with_layout(
                        egui::vec2(46.0, 70.0),
                        egui::Layout::top_down(egui::Align::Center),
                        |ui| {
                            let vi_selected = self.language == UiLanguage::Vi;
                            let en_selected = self.language == UiLanguage::En;

                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("VI")
                                            .size(12.0)
                                            .strong()
                                            .color(if vi_selected {
                                                egui::Color32::WHITE
                                            } else {
                                                egui::Color32::from_rgb(24, 46, 86)
                                            }),
                                    )
                                    .fill(if vi_selected {
                                        egui::Color32::from_rgb(18, 126, 246)
                                    } else {
                                        egui::Color32::from_rgb(238, 244, 251)
                                    })
                                    .min_size(egui::vec2(42.0, 28.0)),
                                )
                                .clicked()
                            {
                                self.language = UiLanguage::Vi;
                            }

                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("EN")
                                            .size(12.0)
                                            .strong()
                                            .color(if en_selected {
                                                egui::Color32::WHITE
                                            } else {
                                                egui::Color32::from_rgb(24, 46, 86)
                                            }),
                                    )
                                    .fill(if en_selected {
                                        egui::Color32::from_rgb(18, 126, 246)
                                    } else {
                                        egui::Color32::from_rgb(238, 244, 251)
                                    })
                                    .min_size(egui::vec2(42.0, 28.0)),
                                )
                                .clicked()
                            {
                                self.language = UiLanguage::En;
                            }
                        },
                    );
                });
                ui.add_space(8.0);

                let all_devices = self.catalog.devices().to_vec();
                let receivers: Vec<DeviceRecord> = all_devices
                    .iter()
                    .filter(|device| !is_homepod_stereo_pair(device))
                    .cloned()
                    .collect();
                let stereo_pairs: Vec<DeviceRecord> = all_devices
                    .iter()
                    .filter(|device| is_homepod_stereo_pair(device))
                    .cloned()
                    .collect();

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

fn is_homepod_stereo_pair(device: &DeviceRecord) -> bool {
    let Some(service) = device.airplay.as_ref() else {
        return false;
    };
    let model = service.txt.model.as_deref().unwrap_or("");
    model.starts_with("AudioAccessory") && service.txt.fields.contains_key("tsid")
}

fn classify_device_artwork(device: &DeviceRecord) -> DeviceArtwork {
    let model = device
        .airplay
        .as_ref()
        .and_then(|s| s.txt.model.as_deref())
        .unwrap_or("")
        .to_ascii_lowercase();
    let name = device.display_name.to_ascii_lowercase();

    if model.starts_with("airport") || name.contains("airport") {
        DeviceArtwork::AirportExpress
    } else if model.starts_with("audioaccessory") || name.contains("homepod") {
        if name.contains("black")
            || name.contains("space gray")
            || name.contains("space grey")
            || name.contains("đen")
        {
            DeviceArtwork::HomePodDark
        } else {
            DeviceArtwork::HomePodLight
        }
    } else if model.starts_with("mac") || name.contains("macbook") || name.contains("mac ") {
        DeviceArtwork::MacBook
    } else {
        DeviceArtwork::MusicServer
    }
}

fn draw_app_logo(ui: &mut egui::Ui, size: egui::Vec2) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter_at(rect);
    painter.rect_filled(
        rect,
        egui::CornerRadius::same(18),
        egui::Color32::from_rgb(18, 132, 248),
    );

    let center = rect.center() + egui::vec2(0.0, 3.0);
    painter.circle_stroke(
        center,
        26.0,
        egui::Stroke::new(4.0, egui::Color32::WHITE),
    );
    painter.circle_stroke(
        center,
        17.0,
        egui::Stroke::new(3.0, egui::Color32::WHITE),
    );
    painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(rect.left() + 8.0, center.y + 5.0),
            egui::pos2(rect.right() - 8.0, rect.bottom() - 7.0),
        ),
        egui::CornerRadius::same(0),
        egui::Color32::from_rgb(18, 132, 248),
    );
    painter.line_segment(
        [
            egui::pos2(center.x, center.y - 24.0),
            egui::pos2(center.x, center.y + 20.0),
        ],
        egui::Stroke::new(3.2, egui::Color32::WHITE),
    );
    painter.circle_filled(
        egui::pos2(center.x, center.y + 19.0),
        5.0,
        egui::Color32::WHITE,
    );
    response
}

fn draw_small_airplay_mark(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(34.0, 34.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.circle_filled(
        rect.center(),
        17.0,
        egui::Color32::from_rgb(236, 246, 255),
    );
    painter.circle_stroke(
        rect.center(),
        9.0,
        egui::Stroke::new(2.0, egui::Color32::from_rgb(15, 126, 245)),
    );
    painter.circle_filled(
        rect.center() + egui::vec2(0.0, 6.0),
        3.0,
        egui::Color32::from_rgb(15, 126, 245),
    );
}

fn draw_status_badge(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    let bg = egui::Color32::from_rgba_unmultiplied(
        color.r(),
        color.g(),
        color.b(),
        28,
    );
    egui::Frame::new()
        .fill(bg)
        .corner_radius(egui::CornerRadius::same(18))
        .inner_margin(egui::Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (dot_rect, _) =
                    ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                ui.painter().circle_filled(dot_rect.center(), 5.0, color);
                ui.label(egui::RichText::new(text).size(13.5).color(color));
            });
        });
}

fn draw_device_art(
    ui: &mut egui::Ui,
    artwork: DeviceArtwork,
    stereo_pair: bool,
    size: egui::Vec2,
) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);

    match artwork {
        DeviceArtwork::AirportExpress => {
            let body = egui::Rect::from_center_size(
                rect.center() + egui::vec2(0.0, 2.0),
                egui::vec2(58.0, 32.0),
            );
            painter.rect_filled(
                body.translate(egui::vec2(0.0, 3.0)),
                egui::CornerRadius::same(8),
                egui::Color32::from_rgb(205, 213, 222),
            );
            painter.rect_filled(
                body,
                egui::CornerRadius::same(8),
                egui::Color32::from_rgb(246, 247, 248),
            );
            painter.line_segment(
                [
                    egui::pos2(body.center().x - 7.0, body.top() + 6.0),
                    egui::pos2(body.center().x + 7.0, body.top() + 6.0),
                ],
                egui::Stroke::new(1.2, egui::Color32::from_rgb(211, 216, 222)),
            );
        }
        DeviceArtwork::HomePodLight | DeviceArtwork::HomePodDark => {
            let dark = artwork == DeviceArtwork::HomePodDark;
            let fill = if dark {
                egui::Color32::from_rgb(36, 38, 43)
            } else {
                egui::Color32::from_rgb(225, 228, 232)
            };
            let top = if dark {
                egui::Color32::from_rgb(72, 75, 83)
            } else {
                egui::Color32::from_rgb(248, 249, 250)
            };

            if stereo_pair {
                let c1 = rect.center() + egui::vec2(-17.0, 1.0);
                let c2 = rect.center() + egui::vec2(17.0, 1.0);
                painter.circle_filled(c1, 22.0, fill);
                painter.circle_filled(
                    c2,
                    22.0,
                    if dark {
                        egui::Color32::from_rgb(42, 44, 49)
                    } else {
                        egui::Color32::from_rgb(218, 222, 227)
                    },
                );
                painter.circle_filled(c1 + egui::vec2(0.0, -12.0), 8.0, top);
                painter.circle_filled(c2 + egui::vec2(0.0, -12.0), 8.0, top);
            } else {
                painter.circle_filled(rect.center(), 23.0, fill);
                painter.circle_filled(rect.center() + egui::vec2(0.0, -13.0), 8.0, top);
            }
        }
        DeviceArtwork::MacBook => {
            let screen = egui::Rect::from_center_size(
                rect.center() + egui::vec2(0.0, -4.0),
                egui::vec2(58.0, 36.0),
            );
            painter.rect_filled(
                screen,
                egui::CornerRadius::same(3),
                egui::Color32::from_rgb(43, 48, 61),
            );
            painter.rect_filled(
                screen.shrink(3.0),
                egui::CornerRadius::same(2),
                egui::Color32::from_rgb(35, 96, 181),
            );
            let base_y = screen.bottom() + 4.0;
            painter.line_segment(
                [
                    egui::pos2(screen.left() - 5.0, base_y),
                    egui::pos2(screen.right() + 5.0, base_y),
                ],
                egui::Stroke::new(4.0, egui::Color32::from_rgb(139, 147, 159)),
            );
        }
        DeviceArtwork::MusicServer => {
            let body = egui::Rect::from_center_size(rect.center(), egui::vec2(66.0, 30.0));
            painter.rect_filled(
                body,
                egui::CornerRadius::same(3),
                egui::Color32::from_rgb(164, 169, 175),
            );
            let display = egui::Rect::from_center_size(
                body.center(),
                egui::vec2(18.0, 10.0),
            );
            painter.rect_filled(
                display,
                egui::CornerRadius::same(2),
                egui::Color32::from_rgb(24, 39, 45),
            );
            painter.circle_filled(
                egui::pos2(body.right() - 9.0, body.center().y),
                3.0,
                egui::Color32::from_rgb(71, 78, 86),
            );
            painter.circle_filled(
                egui::pos2(body.left() + 9.0, body.center().y),
                3.0,
                egui::Color32::from_rgb(71, 78, 86),
            );
        }
    }
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

    #[test]
    fn homepod_tsid_is_listed_as_stereo_pair() {
        let txt = AirPlayTxt::parse([
            ("model", "AudioAccessory5,1"),
            ("tsid", "stereo-group-1"),
            ("features", "274877906944"),
        ])
        .unwrap();
        let device = DeviceRecord {
            display_name: "Living Room".into(),
            airplay: Some(DiscoveredService {
                kind: ServiceKind::AirPlay,
                fullname: "Living Room._airplay._tcp.local.".into(),
                display_name: "Living Room".into(),
                host: "living-room.local.".into(),
                port: 7000,
                addresses: vec!["192.168.1.30".into()],
                txt,
            }),
            raop: None,
        };
        assert!(is_homepod_stereo_pair(&device));
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
            install_windows_ui_font(&cc.egui_ctx);
            Ok(Box::new(SairplayApp::default()))
        }),
    )
}
