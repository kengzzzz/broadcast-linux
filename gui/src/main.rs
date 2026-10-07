mod app;
mod config_file;
mod devices;
mod pages;
mod preview;
mod service;
mod setup_task;
mod theme;

use eframe::{egui, egui_glow};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("broadcast-linux")
            .with_title("broadcast-linux")
            .with_inner_size([820.0, 680.0])
            .with_min_inner_size([640.0, 480.0]),
        // With vsync, swapping buffers can block while the window is on a hidden
        // workspace, and the compositor then reports the app as not responding.
        glow_options: egui_glow::GlowConfiguration {
            vsync: false,
            ..Default::default()
        },
        ..Default::default()
    };
    eframe::run_native(
        "broadcast-linux",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(&cc.egui_ctx)))),
    )
}
