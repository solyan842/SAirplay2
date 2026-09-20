use eframe::egui;
use sairplay_engine::{
    DeviceCatalog, DiscoveryEvent, EngineState, MdnsBrowser, Route, ServiceKind, SessionCore,
};
use std::sync::mpsc::Receiver;

struct SairplayApp {
    engine: SessionCore,
    log: Vec<String>,
    catalog: DeviceCatalog,
    discovery: Option<MdnsBrowser>,
    discovery_rx: Option<Receiver<DiscoveryEvent>>,
}

impl Default for SairplayApp {
    fn default() -> Self {
        let (discovery, discovery_rx, mut log) = match MdnsBrowser::start() {
            Ok((browser, rx)) => (
                Some(browser),
                Some(rx),
                vec!["SAirplay2 clean GUI initialized.".into(), "mDNS discovery started.".into()],
            ),
            Err(err) => (
                None,
                None,
                vec![
                    "SAirplay2 clean GUI initialized.".into(),
                    format!("mDNS discovery unavailable: {err}"),
                ],
            ),
        };

        Self {
            engine: SessionCore::new(Route::AirPlay2Native, 44_100 * 2 * 2),
            log: {
                log.shrink_to_fit();
                log
            },
            catalog: DeviceCatalog::default(),
            discovery,
            discovery_rx,
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
                    self.log.push(format!("mDNS removed: {fullname}"));
                }
                DiscoveryEvent::Error(err) => {
                    self.log.push(format!("mDNS error: {err}"));
                }
            }
        }
    }
}

impl eframe::App for SairplayApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_discovery();

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("SAirplay2");
            ui.label("Clean Windows AirPlay engine");
            ui.separator();

            ui.horizontal(|ui| {
                ui.label("Engine:");
                ui.strong(format!("{:?}", self.engine.state));
                ui.separator();
                ui.label("Discovery:");
                ui.strong(if self.discovery.is_some() { "Running" } else { "Unavailable" });
            });

            ui.add_space(8.0);
            ui.heading("Receivers");

            let devices = self.catalog.devices();
            if devices.is_empty() {
                ui.label("Scanning _airplay._tcp.local. and _raop._tcp.local. ...");
            } else {
                egui::Grid::new("receivers_grid")
                    .striped(true)
                    .min_col_width(120.0)
                    .show(ui, |ui| {
                        ui.strong("Device");
                        ui.strong("Route");
                        ui.strong("Address");
                        ui.strong("Services");
                        ui.end_row();

                        for device in devices {
                            let route = device.route(false, false);
                            let address = device
                                .addresses()
                                .into_iter()
                                .next()
                                .unwrap_or_else(|| "-".into());
                            let services = match (device.airplay.is_some(), device.raop.is_some()) {
                                (true, true) => "AirPlay + RAOP",
                                (true, false) => "AirPlay",
                                (false, true) => "RAOP",
                                (false, false) => "-",
                            };

                            ui.label(&device.display_name);
                            ui.monospace(format!("{route:?}"));
                            ui.monospace(address);
                            ui.label(services);
                            ui.end_row();
                        }
                    });
            }

            ui.add_space(10.0);
            ui.separator();
            ui.label("Engine lifecycle test (no real AirPlay transport yet)");

            ui.horizontal(|ui| {
                if ui.button("Start engine test").clicked() && self.engine.state == EngineState::Idle {
                    match self.engine.arm(0, 1) {
                        Ok(()) => self.log.push("Engine core armed. Timeline is running.".into()),
                        Err(e) => self.log.push(format!("Engine error: {e}")),
                    }
                }

                if ui.button("Inject 15s silence").clicked()
                    && self.engine.state == EngineState::Running
                {
                    for _ in 0..((44_100usize * 15) / 352) {
                        let _ = self.engine.packet_pcm(704);
                    }
                    self.log.push("15s silence crossed without ending session.".into());
                }
            });

            ui.separator();
            ui.label("No legacy AirPlay transport code is linked.");
            ui.label("Discovery and route selection are real; transport remains intentionally disconnected.");

            ui.separator();
            egui::ScrollArea::vertical().max_height(190.0).show(ui, |ui| {
                for line in self.log.iter().rev().take(40).rev() {
                    ui.monospace(line);
                }
            });
        });

        ctx.request_repaint_after(std::time::Duration::from_millis(200));
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
