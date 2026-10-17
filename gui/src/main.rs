mod app;
mod audio;
mod config_file;
mod devices;
mod files;
mod pages;
mod preview;
mod service;
mod settings;
mod setup_task;
mod theme;
mod video;
mod widgets;

use eframe::{egui, egui_glow};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("broadcast-linux")
            .with_title("broadcast-linux")
            .with_inner_size([1080.0, 720.0])
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
