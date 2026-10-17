use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use broadcast_linux::nvml::{Nvml, Sample};
use broadcast_linux::status::{Load, Status};
use eframe::egui::{self, RichText};

use crate::app::App;
use crate::service::Link;
use crate::theme;

/// Samples NVML only while effects run, so an idle laptop GPU can sleep. Reads the status on
/// its own thread, since a hidden window stops drawing.
pub struct WholeGpu {
    sample: Arc<Mutex<Option<Sample>>>,
}

impl WholeGpu {
    pub fn start(ctx: egui::Context, link: Link) -> Self {
        let sample: Arc<Mutex<Option<Sample>>> = Arc::default();
        thread::spawn({
            let sample = Arc::clone(&sample);
            move || {
                let mut nvml = None;
                loop {
                    let uuid = link.status().and_then(|status| {
                        let gpu = status.gpu.as_ref().filter(|_| !loads(&status).is_empty())?;
                        Some(gpu.uuid.clone())
                    });
                    if uuid.is_none() {
                        nvml = None;
                    } else if nvml.is_none() {
                        nvml = Nvml::open();
                    }
                    let new = nvml
                        .as_ref()
                        .zip(uuid)
                        .and_then(|(nvml, uuid)| nvml.sample(&uuid));
                    let mut current = sample.lock().unwrap();
                    if *current != new {
                        *current = new;
                        ctx.request_repaint();
                    }
                    drop(current);
                    thread::sleep(Duration::from_secs(1));
                }
            }
        });
        Self { sample }
    }

    fn sample(&self) -> Option<Sample> {
        *self.sample.lock().unwrap()
    }
}

fn loads(status: &Status) -> Vec<(&'static str, Load)> {
    [
        ("Camera", &status.camera),
        ("Mic", &status.mic),
        ("Speaker", &status.speaker),
    ]
    .into_iter()
    .filter_map(|(name, device)| Some((name, device.load?)))
    .collect()
}

pub fn draw(app: &App, ui: &mut egui::Ui) {
    let Some(status) = app.status() else {
        return;
    };
    let loads = loads(&status);
    egui::Panel::bottom("gpu").show(ui, |ui| {
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            let worst = loads.iter().map(|(_, load)| permille(load)).max();
            let fill = match worst {
                Some(0..800) => theme::GOOD,
                Some(800..=1000) => theme::WARN,
                Some(_) => theme::BAD,
                None => theme::MUTED,
            };
            ui.label("GPU");
            ui.add(
                egui::ProgressBar::new(fraction(worst.unwrap_or(0)))
                    .desired_width(140.0)
                    .fill(fill)
                    .text(worst.map_or_else(String::new, |w| format!("{}%", (w + 5) / 10))),
            )
            .on_hover_text(
                "How long the effects take for each frame, against the time between frames. \
                 Over 100%, the camera drops frames or the audio falls behind.",
            );
            if loads.is_empty() {
                ui.label(RichText::new("No effects running").color(theme::MUTED));
            }
            for (name, load) in &loads {
                ui.label(format!(
                    "{name} {} of {} ms",
                    millis(load.busy_us),
                    millis(load.budget_us)
                ));
                if load.target_fps_x10 > 0 {
                    let fps = format!(
                        "{} of {} fps",
                        tenths(load.fps_x10),
                        tenths(load.target_fps_x10)
                    );
                    let low = u64::from(load.fps_x10) * 10 < u64::from(load.target_fps_x10) * 9;
                    ui.label(RichText::new(fps).color(if low {
                        theme::WARN
                    } else {
                        theme::MUTED
                    }));
                }
            }
            if let Some(sample) = app.whole_gpu.sample() {
                let clock = if sample.max_clock_mhz > 0 {
                    format!(" at {} of {} MHz", sample.clock_mhz, sample.max_clock_mhz)
                } else {
                    String::new()
                };
                ui.label(
                    RichText::new(format!("Whole GPU {}%{clock}", sample.busy_percent))
                        .color(theme::MUTED),
                )
                .on_hover_text(
                    "How much of the time the GPU was busy, with games and the desktop. \
                     A light load runs at a lower clock, so it reads higher than its real share.",
                );
            }
        });
        ui.add_space(2.0);
    });
}

fn permille(load: &Load) -> u32 {
    u32::try_from(u64::from(load.busy_us) * 1000 / u64::from(load.budget_us.max(1)))
        .unwrap_or(u32::MAX)
}

fn fraction(permille: u32) -> f32 {
    f32::from(u16::try_from(permille.min(1000)).unwrap_or(1000)) / 1000.0
}

fn millis(us: u32) -> String {
    format!("{:.1}", f64::from(us) / 1000.0)
}

fn tenths(x10: u32) -> String {
    if x10.is_multiple_of(10) {
        (x10 / 10).to_string()
    } else {
        format!("{:.1}", f64::from(x10) / 10.0)
    }
}
