use eframe::egui;
use sairplay_engine::{EngineState, Route, SessionCore};

struct SairplayApp {
    engine: SessionCore,
    log: Vec<String>,
}

impl Default for SairplayApp {
    fn default() -> Self {
        Self {
            engine: SessionCore::new(Route::AirPlay2Native, 44_100 * 2 * 2),
            log: vec!["SAirplay2 clean GUI initialized.".into()],
        }
    }
}

impl eframe::App for SairplayApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("SAirplay2");
            ui.label("Clean Windows AirPlay engine");
            ui.separator();

            ui.horizontal(|ui| {
                ui.label("Engine:");
                ui.strong(format!("{:?}", self.engine.state));
            });

            ui.horizontal(|ui| {
                if ui.button("Start engine test").clicked() && self.engine.state == EngineState::Idle {
                    match self.engine.arm(0, 1) {
                        Ok(()) => self.log.push("Engine core armed. Timeline is running.".into()),
                        Err(e) => self.log.push(format!("Engine error: {e}")),
                    }
                }

                if ui.button("Inject 15s silence boundary").clicked()
                    && self.engine.state == EngineState::Running
                {
                    for _ in 0..((44_100usize * 15) / 352) {
                        let _ = self.engine.packet_pcm(704);
                    }
                    self.log.push("15s silence crossed without ending session.".into());
                }
            });

            ui.separator();
            ui.label("This GUI intentionally has no legacy AirPlay transport code.");
            ui.label("Device discovery, WASAPI and real transports will be added behind the new engine.");

            ui.separator();
            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                for line in self.log.iter().rev().take(20).rev() {
                    ui.monospace(line);
                }
            });
        });
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SAirplay2")
            .with_inner_size([900.0, 620.0])
            .with_min_inner_size([760.0, 520.0]),
        ..Default::default()
    };

    eframe::run_native(
        "SAirplay2",
        options,
        Box::new(|_| Ok(Box::new(SairplayApp::default()))),
    )
}
