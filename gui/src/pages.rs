use broadcast_linux::VERSION;
use broadcast_linux::camera::expand_home;
use broadcast_linux::graph::AudioNode;
use broadcast_linux::status::{Device, State};
use eframe::egui::{self, Color32, RichText, Sense, vec2};

use crate::app::{App, Endpoint, Page};
use crate::setup_task::SetupTask;
use crate::widgets::{self, Icon};
use crate::{audio, settings, theme, video};

const RAIL_WIDTH: f32 = 78.0;

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    rail(app, ui);
    header(app, ui);
    action_bar(app, ui);
    egui::CentralPanel::default()
        .frame(egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::symmetric(20, 12)))
        .show(ui, |ui| {
            banners(app, ui);
            // Edits to defaults shown in place of an unreadable file would go nowhere.
            let editable = app.file.is_ok();
            match app.page {
                Page::Settings => settings::page(app, ui),
                page => {
                    ui.add_enabled_ui(editable, |ui| match page {
                        Page::Audio => audio::page(app, ui),
                        _ => video::page(app, ui),
                    });
                }
            }
        });
    dialogs(app, &ctx);
}

pub fn state_color(device: &Device) -> Color32 {
    match device.state {
        _ if device.error.is_some() => theme::BAD,
        State::Running => theme::GOOD,
        State::Loading => theme::WARN,
        State::Unavailable => theme::BAD,
        _ => theme::MUTED,
    }
}

fn rail(app: &mut App, ui: &mut egui::Ui) {
    let fill = theme::surface(ui).rail;
    egui::Panel::left("rail")
        .resizable(false)
        .exact_size(RAIL_WIDTH)
        .show_separator_line(false)
        .frame(egui::Frame::new().fill(fill))
        .show(ui, |ui| {
            ui.add_space(10.0);
            for (page, icon, label) in [
                (Page::Audio, Icon::Audio, "Audio"),
                (Page::Video, Icon::Video, "Video"),
                (Page::Settings, Icon::Settings, "Settings"),
            ] {
                let status = match page {
                    Page::Audio => widgets::worst(
                        [Endpoint::Mic, Endpoint::Speaker]
                            .into_iter()
                            .filter_map(|e| app.device(e))
                            .map(|d| state_color(&d)),
                    ),
                    Page::Video => app.device(Endpoint::Camera).map(|d| state_color(&d)),
                    Page::Settings => app.setup_needed().map(|_| theme::BAD),
                };
                if rail_item(ui, icon, label, app.page == page, status).clicked() {
                    let ctx = ui.ctx().clone();
                    app.show(page, &ctx);
                }
            }
            ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!("v{VERSION}"))
                        .small()
                        .color(theme::MUTED),
                );
            });
        });
}

fn rail_item(
    ui: &mut egui::Ui,
    icon: Icon,
    label: &str,
    selected: bool,
    status: Option<Color32>,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(RAIL_WIDTH, 66.0), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, label)
    });
    let painter = ui.painter();
    if response.hovered() && !selected {
        painter.rect_filled(rect, 0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let color = if selected {
        theme::accent_text(ui)
    } else {
        ui.visuals().text_color()
    };
    if selected {
        let bar = egui::Rect::from_min_size(rect.min, vec2(3.0, rect.height()));
        painter.rect_filled(bar, 0, theme::ACCENT);
    }
    let icon_center = rect.center_top() + vec2(0.0, 24.0);
    widgets::paint_icon(ui, icon, icon_center, 24.0, color);
    painter.text(
        rect.center_top() + vec2(0.0, 50.0),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(12.5),
        color,
    );
    if let Some(status) = status.filter(|c| *c != theme::MUTED) {
        painter.circle_filled(icon_center + vec2(15.0, -10.0), 4.0, status);
    }
    response
}

fn header(app: &App, ui: &mut egui::Ui) {
    let fill = theme::surface(ui).header;
    egui::Panel::top("header")
        .resizable(false)
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(fill)
                .inner_margin(egui::Margin::symmetric(20, 14)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let title = match app.page {
                    Page::Audio => "Audio",
                    Page::Video => "Video",
                    Page::Settings => "Settings",
                };
                ui.label(RichText::new(title).size(24.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (color, text) = if app.service.active() {
                        (theme::GOOD, "Service running")
                    } else {
                        (theme::MUTED, "Service stopped")
                    };
                    ui.label(RichText::new(text).color(theme::MUTED));
                    theme::dot(ui, color);
                });
            });
        });
}

fn action_bar(app: &mut App, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let show = app.dirty() || app.unapplied() || app.busy().is_some() || app.message.is_some();
    if !show {
        return;
    }
    egui::Panel::bottom("actions").show(ui, |ui| {
        ui.add_space(6.0);
        if let Some(label) = app.busy() {
            ui.horizontal_wrapped(|ui| {
                ui.spinner();
                ui.label(label);
            });
        } else if let Some(message) = &app.message {
            let color = if message.error {
                theme::BAD
            } else {
                theme::GOOD
            };
            let mut close = false;
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(color, &message.text);
                close = ui.small_button("Dismiss").clicked();
            });
            if close {
                app.message = None;
            }
        }
        if (app.dirty() || app.unapplied()) && app.busy().is_none() {
            let restart = app.restart_needed();
            let problem = draft_problem(app);
            ui.horizontal_wrapped(|ui| {
                let label = if !app.service.active() {
                    "Save"
                } else if restart {
                    "Apply and restart"
                } else {
                    "Apply"
                };
                if ui
                    .add_enabled(problem.is_none(), egui::Button::new(label))
                    .clicked()
                {
                    app.apply(false, &ctx);
                }
                if app.dirty() && ui.button("Revert").clicked() {
                    app.revert();
                }
                if let Some(problem) = problem {
                    ui.colored_label(theme::WARN, problem);
                } else if !app.dirty() {
                    ui.label(
                        RichText::new("Your settings are saved but not active yet.")
                            .color(theme::MUTED),
                    );
                } else if restart && app.service.active() {
                    let in_use = app
                        .status()
                        .is_some_and(|s| s.mic.readers + s.speaker.readers + s.camera.readers > 0);
                    let note = if in_use {
                        "Restarting briefly interrupts apps using the mic, speaker or camera."
                    } else {
                        "This change needs a service restart."
                    };
                    ui.label(RichText::new(note).color(theme::MUTED));
                }
            });
        }
        ui.add_space(6.0);
    });
}

/// Only camera settings being changed or switched on are checked, so a stale setting
/// elsewhere cannot block an unrelated save.
fn draft_problem(app: &App) -> Option<&'static str> {
    let camera = &app.draft.camera;
    let saved = app.saved().map(|c| &c.camera).filter(|s| s.enabled);
    if !camera.enabled {
        return None;
    }
    if saved.is_none_or(|s| s.background != camera.background)
        && let Some(path) = camera.background.as_deref()
    {
        if path.trim().is_empty() {
            return Some("Choose a background image first.");
        }
        if !expand_home(path.trim()).is_file() {
            return Some("The background image does not exist.");
        }
    }
    let size = |c: &broadcast_linux::config::CameraConfig| (c.width, c.height, c.fps);
    if saved.is_none_or(|s| size(s) != size(camera))
        && (!camera.width.is_multiple_of(2)
            || camera.width == 0
            || camera.height == 0
            || camera.fps == 0)
    {
        return Some("The camera size and frame rate must be positive, with an even width.");
    }
    None
}

fn banners(app: &mut App, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    if let Err(error) = &app.file {
        let error = error.clone();
        warning(ui, theme::BAD, |ui| {
            ui.label(RichText::new("The config file has an error").strong());
            ui.label(error);
            ui.horizontal_wrapped(|ui| {
                if ui.button("Load it again").clicked() {
                    app.reload_file();
                }
                if ui.button("Reset to defaults").clicked() {
                    app.reset_file(&ctx);
                }
            });
        });
    }
    if let Some(reason) = app.setup_needed().map(str::to_owned)
        && app.page != Page::Settings
    {
        warning(ui, theme::WARN, |ui| {
            ui.label(RichText::new("Setup is not finished").strong());
            ui.label(reason);
            if ui.button("Open Settings").clicked() {
                app.show(Page::Settings, &ctx);
            }
        });
    }
    if app.busy().is_some() {
        return;
    }
    if !app.service.active() && app.setup_needed().is_none() {
        warning(ui, theme::WARN, |ui| {
            ui.label(RichText::new("The service is not running").strong());
            ui.label("Effects only work while it runs. Starting it also starts it at login.");
            if ui.button("Start the service").clicked() {
                app.start_service(&ctx);
            }
        });
    } else if app.service_outdated() {
        let running = app.status().map_or_else(
            || "an older version".to_owned(),
            |s| format!("version {}", s.version),
        );
        warning(ui, theme::WARN, |ui| {
            ui.label(RichText::new("The service needs a restart").strong());
            ui.label(format!(
                "It is still running {running}; this app is version {VERSION}."
            ));
            if ui.button("Restart the service").clicked() {
                app.restart_service(&ctx);
            }
        });
    } else if app.status().is_some_and(|s| s.restart_pending) && !app.dirty() {
        warning(ui, theme::WARN, |ui| {
            ui.label("Some saved settings take effect after a restart.");
            if ui.button("Restart the service").clicked() {
                app.restart_service(&ctx);
            }
        });
    }
}

pub fn warning(ui: &mut egui::Ui, color: egui::Color32, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .stroke(egui::Stroke::new(1.0, color))
        .inner_margin(10)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    ui.add_space(8.0);
}

/// Widget ids count what was drawn before them, so the action bar appearing or vanishing
/// as a drag makes the config dirty or clean would give the slider a new id and end the drag.
pub fn fixed_id<R>(ui: &mut egui::Ui, key: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let builder = egui::UiBuilder::new().id(egui::Id::new(("block", key)));
    ui.scope_builder(builder, add).inner
}

// By default these clamp and snap any shown value, rewriting hand-written ones like 0.333.
pub fn strength(ui: &mut egui::Ui, value: &mut f32) {
    // The value box keeps counting a drag past the range, so dragging back stalls.
    ui.add(
        egui::Slider::new(value, 0.0..=1.0)
            .clamping(egui::SliderClamping::Edits)
            .step_by(0.01)
            .show_value(false),
    );
    ui.label(format!("{:.0}%", *value * 100.0));
}

pub fn whole_number(ui: &mut egui::Ui, value: &mut u64, max: u64) {
    ui.add(
        egui::DragValue::new(value)
            .range(0..=max)
            .clamp_existing_to_range(false),
    );
}

pub fn node_picker(
    ui: &mut egui::Ui,
    id: &str,
    value: &mut String,
    nodes: &[AudioNode],
    default_label: &str,
) {
    let selected = if value == "default" {
        default_label.to_owned()
    } else {
        nodes
            .iter()
            .find(|n| n.name == *value)
            .map_or_else(|| format!("{value} (not found)"), |n| n.description.clone())
    };
    egui::ComboBox::from_id_salt(id)
        .selected_text(selected)
        .width(ui.available_width().clamp(160.0, 320.0))
        .show_ui(ui, |ui| {
            ui.selectable_value(value, "default".to_owned(), default_label);
            for node in nodes {
                ui.selectable_value(value, node.name.clone(), &node.description);
            }
        });
}

pub fn commands_list(ui: &mut egui::Ui, commands: &[String]) {
    for command in commands {
        ui.horizontal_wrapped(|ui| {
            ui.add(
                egui::Label::new(RichText::new(command).monospace())
                    .wrap_mode(egui::TextWrapMode::Wrap),
            );
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(command.clone());
            }
        });
    }
}

fn dialogs(app: &mut App, ctx: &egui::Context) {
    if let Some(text) = app.setup.as_ref().and_then(SetupTask::eula) {
        let mut answer = None;
        egui::Modal::new(egui::Id::new("eula")).show(ctx, |ui| {
            ui.set_width(620.0);
            ui.heading("NVIDIA Broadcast licence");
            ui.label(
                "The effects are NVIDIA software. Read and accept NVIDIA's licence to continue.",
            );
            egui::ScrollArea::vertical()
                .max_height(380.0)
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(&text).small())
                            .wrap_mode(egui::TextWrapMode::Wrap),
                    );
                });
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                if ui.button("Accept").clicked() {
                    answer = Some(true);
                }
                if ui.button("Decline").clicked() {
                    answer = Some(false);
                }
            });
        });
        if let (Some(answer), Some(task)) = (answer, &app.setup) {
            task.answer_eula(answer);
        }
    }

    if app.changed_on_disk {
        let mut choice = None;
        egui::Modal::new(egui::Id::new("changed-on-disk")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.heading("The config file changed");
            ui.label(format!(
                "{} was changed outside this app since it was loaded.",
                app.paths.config.display()
            ));
            ui.horizontal_wrapped(|ui| {
                if ui.button("Keep my changes").clicked() {
                    choice = Some(true);
                }
                if ui.button("Load the file").clicked() {
                    choice = Some(false);
                }
            });
        });
        match choice {
            Some(true) => app.apply(true, ctx),
            Some(false) => app.reload_file(),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drawing_keeps_hand_written_values() {
        let ctx = egui::Context::default();
        let (mut third, mut over, mut long) = (0.333_f32, 1.5_f32, 7200_u64);
        for _ in 0..3 {
            ctx.run_ui(egui::RawInput::default(), |ui| {
                strength(ui, &mut third);
                strength(ui, &mut over);
                whole_number(ui, &mut long, 3600);
            })
            .drop_without_applying_deltas();
        }
        assert!((third - 0.333).abs() < f32::EPSILON);
        assert!((over - 1.5).abs() < f32::EPSILON);
        assert_eq!(long, 7200);
    }
}
