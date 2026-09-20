use eframe::egui;
use sairplay_engine::{
    DeviceCatalog, DiscoveredService, DiscoveryEvent, MdnsBrowser, NativeSession,
    NativeSessionConfig, Route, ServiceKind,
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

struct SairplayApp {
    log: Vec<String>,
    catalog: DeviceCatalog,
    discovery: Option<MdnsBrowser>,
    discovery_rx: Option<Receiver<DiscoveryEvent>>,
    selected_fullname: Option<String>,
    playback: PlaybackUiState,
    connect_rx: Option<Receiver<Result<NativeSession, String>>>,
    session: Option<NativeSession>,
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
                    self.log.push(format!(
                        "mDNS {kind}: {} @ {}:{}",
                        service.display_name, service.host, service.port
                    ));
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

    fn monitor_running_session(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };

        if let Some(error) = session.audio_error() {
            self.log.push(format!("Audio worker stopped: {error}"));
            self.playback = PlaybackUiState::Error(error);
            self.session = None;
        } else if !session.audio_running() {
            self.log.push("Audio worker stopped.".into());
            self.playback = PlaybackUiState::Error("Audio worker stopped".into());
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

        let mut config = NativeSessionConfig::new(host.clone(), port);
        // Fixed app identity for the clean alpha path; credentials remain absent
        // unless a later UI explicitly supplies them.
        config.dacp_id = "A1B2C3D4E5F60708".into();
        config.active_remote = "123456789".into();

        let (tx, rx) = mpsc::sync_channel(1);
        self.connect_rx = Some(rx);
        self.playback = PlaybackUiState::Connecting(name.clone());
        self.session = None;
        self.log.push(format!(
            "{name}: preflight starting on {host}:{port}; Playing will wait for Ready + audio."
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
    }

    fn status_text(&self) -> String {
        match &self.playback {
            PlaybackUiState::Idle => "Idle".into(),
            PlaybackUiState::Connecting(name) => format!("Preparing · {name}"),
            PlaybackUiState::Playing(name) => format!("Playing · {name}"),
            PlaybackUiState::Error(_) => "Error".into(),
        }
    }
}

impl eframe::App for SairplayApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_discovery();
        self.pump_connect_result();
        self.monitor_running_session();

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("SAirplay2");
            ui.label("Native AirPlay 2 · Windows system audio · 16-bit / 44.1 kHz");
            ui.separator();

            ui.horizontal(|ui| {
                ui.label("Status:");
                ui.strong(self.status_text());
                ui.separator();
                ui.label("Discovery:");
                ui.strong(if self.discovery.is_some() { "Running" } else { "Unavailable" });
            });

            if let PlaybackUiState::Error(error) = &self.playback {
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    ui.strong("Last error:");
                    if ui.button("Copy Error").clicked() {
                        ui.ctx().copy_text(error.clone());
                    }
                });
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(error).monospace()
                    )
                    .selectable(true)
                    .wrap(),
                );
            }

            ui.add_space(8.0);
            ui.heading("Receivers");

            let devices = self.catalog.devices().to_vec();
            if devices.is_empty() {
                ui.label("Scanning _airplay._tcp.local. and _raop._tcp.local. ...");
            } else {
                egui::Grid::new("receivers_grid")
                    .striped(true)
                    .min_col_width(105.0)
                    .show(ui, |ui| {
                        ui.strong("");
                        ui.strong("Device");
                        ui.strong("Route");
                        ui.strong("Address");
                        ui.strong("Services");
                        ui.end_row();

                        for device in &devices {
                            let route = device.route(false, false);
                            let address = device
                                .airplay
                                .as_ref()
                                .map(preferred_service_address)
                                .or_else(|| device.raop.as_ref().map(preferred_service_address))
                                .unwrap_or_else(|| "-".into());
                            let services = match (device.airplay.is_some(), device.raop.is_some()) {
                                (true, true) => "AirPlay + RAOP",
                                (true, false) => "AirPlay",
                                (false, true) => "RAOP",
                                (false, false) => "-",
                            };

                            let fullname = device
                                .airplay
                                .as_ref()
                                .map(|service| service.fullname.clone());
                            let selected = fullname
                                .as_deref()
                                .is_some_and(|name| self.selected_fullname.as_deref() == Some(name));

                            let selectable = route == Route::AirPlay2Native && fullname.is_some();
                            let radio = ui.add_enabled(
                                selectable && !matches!(self.playback, PlaybackUiState::Connecting(_)),
                                egui::RadioButton::new(selected, ""),
                            );
                            if radio.clicked() {
                                self.selected_fullname = fullname;
                                if matches!(self.playback, PlaybackUiState::Error(_)) {
                                    self.playback = PlaybackUiState::Idle;
                                }
                            }

                            ui.label(&device.display_name);
                            ui.monospace(format!("{route:?}"));
                            ui.monospace(address);
                            ui.label(services);
                            ui.end_row();
                        }
                    });
            }

            ui.add_space(12.0);
            ui.separator();

            ui.horizontal(|ui| {
                let start_enabled = self.selected_fullname.is_some()
                    && matches!(self.playback, PlaybackUiState::Idle | PlaybackUiState::Error(_));
                if ui
                    .add_enabled(start_enabled, egui::Button::new("Start"))
                    .clicked()
                {
                    self.start_selected();
                }

                let stop_enabled = self.session.is_some();
                if ui
                    .add_enabled(stop_enabled, egui::Button::new("Stop"))
                    .clicked()
                {
                    self.stop_playback();
                }

                if matches!(self.playback, PlaybackUiState::Connecting(_)) {
                    ui.spinner();
                    ui.label("Checking receiver, pairing, timing and media transport...");
                }
            });

            ui.add_space(6.0);
            ui.label(
                "Playing is shown only after native transport is Ready and Windows audio capture has started.",
            );

            ui.separator();
            ui.horizontal(|ui| {
                ui.strong("Log");
                if ui.button("Copy Log").clicked() {
                    ui.ctx().copy_text(self.log.join("\n"));
                }
            });
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                for line in self.log.iter().rev().take(80).rev() {
                    ui.add(
                        egui::Label::new(egui::RichText::new(line).monospace())
                            .selectable(true)
                            .wrap(),
                    );
                }
            });
        });

        ctx.request_repaint_after(std::time::Duration::from_millis(100));
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
    fn lan_ipv4_beats_windows_link_local_address() {
        let s = service(&["169.254.2.72", "192.168.88.72"]);
        assert_eq!(preferred_service_address(&s), "192.168.88.72");
    }

    #[test]
    fn hostname_is_used_when_only_link_local_addresses_exist() {
        let s = service(&["169.254.2.72"]);
        assert_eq!(preferred_service_address(&s), "Test.local");
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SAirplay2")
            .with_inner_size([980.0, 680.0])
            .with_min_inner_size([820.0, 560.0]),
        ..Default::default()
    };

    eframe::run_native(
        "SAirplay2",
        options,
        Box::new(|_| Ok(Box::new(SairplayApp::default()))),
    )
}
