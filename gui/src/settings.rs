use broadcast_linux::doctor::Status as Check;
use broadcast_linux::gpu::{Chosen, Generation, Gpu};
use broadcast_linux::{VERSION, setup};
use eframe::egui::{self, RichText};

use crate::app::{App, release};
use crate::pages::{commands_list, fixed_id, whole_number};
use crate::setup_task::SetupTask;
use crate::widgets::{self, card, section_title};
use crate::{service, theme};

const PROJECT: &str = "https://github.com/kengzzzz/broadcast-linux";

pub fn page(app: &mut App, ui: &mut egui::Ui) {
    if let Some((left, right)) = widgets::split(ui) {
        widgets::pane(ui, left, "settings-main", |ui| main(app, ui));
        widgets::pane(ui, right, "settings-checks", |ui| side(app, ui));
    } else {
        let area = ui.available_rect_before_wrap();
        widgets::pane(ui, area, "settings", |ui| {
            main(app, ui);
            side(app, ui);
        });
    }
}

fn main(app: &mut App, ui: &mut egui::Ui) {
    section_title(ui, "NVIDIA Broadcast");
    nvidia_files(app, ui);
    section_title(ui, "Service");
    fixed_id(ui, "service", |ui| service(app, ui));
    section_title(ui, "About");
    about(ui);
}

fn side(app: &mut App, ui: &mut egui::Ui) {
    section_title(ui, "System check");
    checks(app, ui);
    section_title(ui, "Service log");
    card(ui, |ui| {
        egui::CollapsingHeader::new("Show the log")
            .id_salt("journal")
            .show(ui, |ui| {
                if app.journal.is_none() || ui.small_button("Refresh").clicked() {
                    app.journal = Some(service::journal(40));
                }
                if let Some(log) = &app.journal {
                    egui::ScrollArea::vertical()
                        .max_height(240.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(RichText::new(log).monospace().small())
                                    .wrap_mode(egui::TextWrapMode::Wrap),
                            );
                        });
                }
            });
    });
}

fn about(ui: &mut egui::Ui) {
    card(ui, |ui| {
        ui.label(RichText::new(format!("broadcast-linux {VERSION}")).strong());
        ui.label(
            RichText::new("NVIDIA Broadcast effects for Linux. Not made or endorsed by NVIDIA.")
                .color(theme::MUTED),
        );
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(PROJECT).monospace());
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(PROJECT.to_owned());
            }
        });
    });
}

fn nvidia_files(app: &mut App, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    card(ui, |ui| {
        if let Some(task) = &app.setup {
            ui.label(task.step());
            match task.progress() {
                Some((done, total)) => {
                    #[allow(clippy::cast_precision_loss)]
                    let fraction = done as f32 / total.max(1) as f32;
                    ui.add(
                        egui::ProgressBar::new(fraction)
                            .show_percentage()
                            .text(format!("{} / {} MB", done >> 20, total >> 20)),
                    );
                }
                None => {
                    ui.spinner();
                }
            }
            if ui.button("Cancel").clicked() {
                task.cancel();
            }
            return;
        }
        match &app.checks {
            None => {
                ui.horizontal_wrapped(|ui| {
                    ui.spinner();
                    ui.label("Checking…");
                });
            }
            Some(checks) => {
                if let Ok(chosen) = &checks.gpu {
                    let (chosen, gpus) = (chosen.clone(), checks.gpus.clone());
                    let installed = checks.installed_for.clone();
                    gpu_choice(app, ui, &chosen, &gpus, installed.as_deref());
                }
                let Some(checks) = &app.checks else {
                    return;
                };
                match &checks.setup_needed {
                    Some(reason) => {
                        match app.update_from() {
                            Some(old) => ui.colored_label(
                                theme::WARN,
                                format!(
                                    "NVIDIA Broadcast {} is available. Your effects run on {old} \
                                     until you install it.",
                                    release(setup::BUILD)
                                ),
                            ),
                            None => ui.colored_label(theme::WARN, reason),
                        };
                        ui.label(
                            "Downloads NVIDIA Broadcast for your GPU (about 2.2 to 2.4 GB) from NVIDIA, \
                             asks you to accept NVIDIA's licence, and extracts the effects \
                             (about 8 GB of free space needed).",
                        );
                        let gpu_ok = checks.gpu.is_ok();
                        if ui
                            .add_enabled(gpu_ok, egui::Button::new("Download and install"))
                            .clicked()
                        {
                            app.preview = None;
                            app.message = None;
                            app.setup = Some(SetupTask::start(ctx.clone()));
                        }
                    }
                    None => {
                        ui.colored_label(theme::GOOD, "Installed and ready.");
                    }
                }
            }
        }
    });
}

fn gpu_choice(
    app: &mut App,
    ui: &mut egui::Ui,
    chosen: &Chosen,
    gpus: &[Gpu],
    installed: Option<&str>,
) {
    if gpus.len() < 2 && app.draft.service.gpu.is_none() {
        ui.label(format!("GPU: {} ({})", chosen.gpu.name, chosen.generation));
        return;
    }
    let label = |gpu: &Gpu| {
        let Some(generation) = gpu.generation else {
            return format!("{} · not supported · {}", gpu.name, gpu.short_bus());
        };
        let mut text = format!("{} · {generation} · {}", gpu.name, gpu.short_bus());
        if installed.is_some_and(|installed| installed != generation.to_string()) {
            text.push_str(&download_note(generation));
        }
        text
    };
    let automatic = gpus
        .iter()
        .find(|gpu| gpu.generation.is_some())
        .map_or_else(
            || "Automatic".to_owned(),
            |gpu| format!("Automatic ({})", gpu.name),
        );
    let selected = match &app.draft.service.gpu {
        None => automatic.clone(),
        Some(uuid) => gpus
            .iter()
            .find(|gpu| gpu.uuid.eq_ignore_ascii_case(uuid))
            .map_or_else(|| format!("{uuid} (not present)"), label),
    };
    ui.horizontal_wrapped(|ui| {
        ui.label("Run effects on");
        egui::ComboBox::from_id_salt("gpu")
            .selected_text(selected)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut app.draft.service.gpu, None, automatic);
                for gpu in gpus {
                    ui.add_enabled_ui(gpu.generation.is_some(), |ui| {
                        ui.selectable_value(
                            &mut app.draft.service.gpu,
                            Some(gpu.uuid.clone()),
                            label(gpu),
                        );
                    });
                }
            });
    });
    if let Some(fallback) = &chosen.fallback {
        ui.colored_label(theme::WARN, format!("{fallback}."));
    }
}

fn download_note(generation: Generation) -> String {
    setup::INSTALLERS
        .iter()
        .find(|installer| installer.generation == generation)
        .map_or_else(String::new, |installer| {
            let tenths = (installer.size + 50_000_000) / 100_000_000;
            format!(" · needs a {}.{} GB download", tenths / 10, tenths % 10)
        })
}

fn service(app: &mut App, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    card(ui, |ui| {
        let active = app.service.active();
        ui.horizontal_wrapped(|ui| {
            theme::dot(ui, if active { theme::GOOD } else { theme::MUTED });
            ui.label(if active { "Running" } else { "Not running" });
        });
        ui.add_enabled_ui(app.busy().is_none(), |ui| {
            ui.horizontal_wrapped(|ui| {
                if active {
                    if ui.button("Restart").clicked() {
                        app.restart_service(&ctx);
                    }
                    if ui.button("Stop").clicked() {
                        app.stop_service(&ctx);
                    }
                } else if ui.button("Start").clicked() {
                    app.start_service(&ctx);
                }
            });
            let mut enabled = app.service.enabled();
            if ui
                .checkbox(&mut enabled, "Start automatically when I log in")
                .changed()
            {
                let action = if enabled { "enable" } else { "disable" };
                if let Err(e) = service::systemctl(&[action, "broadcast-linux"]) {
                    app.fail(format!("{e:#}"));
                }
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("Stop effects when no app has used them for");
            whole_number(ui, &mut app.draft.service.idle_timeout_seconds, 3600);
            ui.label("seconds");
        });
        ui.checkbox(
            &mut app.draft.service.gpu_keep_awake,
            "Keep the GPU awake while a mic or speaker model is loaded",
        )
        .on_hover_text(
            "Fewer short gaps when an app reopens them or the GPU changes power state. \
             Off saves some GPU power.",
        );
    });
}

fn checks(app: &mut App, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    card(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(!app.checking(), egui::Button::new("Check again"))
                .clicked()
            {
                app.run_checks(&ctx);
            }
            if app.checking() {
                ui.spinner();
            }
        });
        let Some(checks) = &app.checks else {
            return;
        };
        for item in &checks.report.items {
            let color = match item.status {
                Check::Ok => theme::GOOD,
                Check::Warn => theme::WARN,
                Check::Fail => theme::BAD,
                Check::Skip => theme::MUTED,
            };
            ui.horizontal(|ui| {
                theme::dot(ui, color);
                ui.label(RichText::new(item.name).strong());
            });
            ui.indent(item.name, |ui| {
                ui.add(egui::Label::new(&item.detail).wrap_mode(egui::TextWrapMode::Wrap));
                commands_list(ui, &item.fix);
            });
        }
    });
}
