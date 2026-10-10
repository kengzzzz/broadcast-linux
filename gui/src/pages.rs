use broadcast_linux::VERSION;
use broadcast_linux::audio::Kind;
use broadcast_linux::camera::expand_home;
use broadcast_linux::config::{
    Adjustable, CameraConfig, InputColor, InputFormat, LightPreset, ParallelDecode,
};
use broadcast_linux::doctor::Status as Check;
use broadcast_linux::graph::AudioNode;
use broadcast_linux::status::State;
use broadcast_linux::webcam::{Format, Webcam};
use eframe::egui::{self, RichText};

use crate::app::{App, Page, device_summary};
use crate::devices;
use crate::preview::Preview;
use crate::service;
use crate::setup_task::SetupTask;
use crate::theme;

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    nav(app, ui);
    action_bar(app, ui);
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(6.0);
                banners(app, ui);
                // Edits to defaults shown in place of an unreadable file would go nowhere.
                let editable = app.file.is_ok();
                match app.page {
                    Page::Setup => setup(app, ui),
                    page => {
                        ui.add_enabled_ui(editable, |ui| match page {
                            Page::Mic => mic(app, ui),
                            Page::Speaker => speaker(app, ui),
                            _ => camera(app, ui),
                        });
                    }
                }
            });
    });
    dialogs(app, &ctx);
}

fn nav(app: &mut App, ui: &mut egui::Ui) {
    egui::Panel::left("nav")
        .resizable(false)
        .exact_size(170.0)
        .show(ui, |ui| {
            ui.add_space(8.0);
            ui.label(RichText::new("broadcast-linux").strong().size(16.0));
            ui.label(
                RichText::new(format!("v{VERSION}"))
                    .small()
                    .color(theme::MUTED),
            );
            ui.add_space(12.0);
            for (page, label) in [
                (Page::Mic, "Microphone"),
                (Page::Speaker, "Speaker"),
                (Page::Camera, "Camera"),
                (Page::Setup, "Setup"),
            ] {
                let color = match page {
                    Page::Setup if app.setup_needed().is_some() => Some(theme::BAD),
                    Page::Setup => None,
                    _ => app.device(page).map(|d| match d.state {
                        State::Running => theme::GOOD,
                        State::Loading => theme::WARN,
                        State::Unavailable => theme::BAD,
                        _ if d.error.is_some() => theme::BAD,
                        _ => theme::MUTED,
                    }),
                };
                ui.horizontal(|ui| {
                    theme::dot(ui, color.unwrap_or(egui::Color32::TRANSPARENT));
                    if ui
                        .add(egui::Button::selectable(app.page == page, label))
                        .clicked()
                    {
                        let ctx = ui.ctx().clone();
                        app.show(page, &ctx);
                    }
                });
            }
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
        && app.page != Page::Setup
    {
        warning(ui, theme::WARN, |ui| {
            ui.label(RichText::new("Setup is not finished").strong());
            ui.label(reason);
            if ui.button("Go to Setup").clicked() {
                app.show(Page::Setup, &ctx);
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

fn warning(ui: &mut egui::Ui, color: egui::Color32, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .stroke(egui::Stroke::new(1.0, color))
        .inner_margin(10)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    ui.add_space(8.0);
}

fn heading(app: &App, ui: &mut egui::Ui, title: &str, page: Page) {
    ui.heading(title);
    if let Some(device) = app.device(page) {
        let (color, text) = device_summary(&device);
        ui.horizontal_wrapped(|ui| {
            theme::dot(ui, color);
            ui.label(RichText::new(text).color(color));
        });
    }
    ui.add_space(6.0);
}

/// Widget ids count what was drawn before them, so the action bar appearing or vanishing
/// as a drag makes the config dirty or clean would give the slider a new id and end the drag.
fn fixed_id<R>(ui: &mut egui::Ui, key: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let builder = egui::UiBuilder::new().id(egui::Id::new(("block", key)));
    ui.scope_builder(builder, add).inner
}

fn section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(4.0);
    ui.label(RichText::new(title).strong());
    fixed_id(ui, title, |ui| {
        egui::Frame::group(ui.style())
            .inner_margin(10)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                add(ui);
            });
    });
    ui.add_space(4.0);
}

// By default these clamp and snap any shown value, rewriting hand-written ones like 0.333.
fn strength(ui: &mut egui::Ui, value: &mut f32) {
    // The value box keeps counting a drag past the range, so dragging back stalls.
    ui.add(
        egui::Slider::new(value, 0.0..=1.0)
            .clamping(egui::SliderClamping::Edits)
            .step_by(0.01)
            .show_value(false),
    );
    ui.label(format!("{:.0}%", *value * 100.0));
}

fn whole_number(ui: &mut egui::Ui, value: &mut u64, max: u64) {
    ui.add(
        egui::DragValue::new(value)
            .range(0..=max)
            .clamp_existing_to_range(false),
    );
}

fn adjustable(ui: &mut egui::Ui, label: &str, hint: &str, effect: &mut Adjustable) {
    ui.checkbox(&mut effect.enabled, label);
    ui.add_enabled_ui(effect.enabled, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.add_space(24.0);
            ui.label("Strength");
            strength(ui, &mut effect.strength);
        });
    });
    ui.label(RichText::new(hint).small().color(theme::MUTED));
    ui.add_space(4.0);
}

fn node_picker(
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

fn mic(app: &mut App, ui: &mut egui::Ui) {
    heading(app, ui, "Microphone", Page::Mic);
    let name = app.mic_name();
    let default_is_us = app.default_source.as_deref() == Some(Kind::Mic.node_name());
    let default_label = match &app.default_source {
        Some(name) => {
            let description = app
                .sources
                .iter()
                .find(|n| n.name == *name)
                .map_or(name.as_str(), |n| n.description.as_str());
            if default_is_us {
                format!("System default ({name} itself)")
            } else {
                format!("System default ({description})")
            }
        }
        None => "System default".to_owned(),
    };
    let draft = &mut app.draft.mic;
    section(ui, "Device", |ui| {
        ui.checkbox(
            &mut draft.enabled,
            format!("Create the virtual mic “{name}”"),
        );
        ui.add_enabled_ui(draft.enabled, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Your microphone");
                node_picker(ui, "mic-input", &mut draft.input, &app.sources, &default_label);
                if ui.small_button("Refresh").clicked() {
                    let devices = devices::query();
                    app.sources = devices.sources;
                    app.default_source = devices.default_source;
                }
            });
            if draft.input == "default" && default_is_us {
                ui.colored_label(
                    theme::BAD,
                    format!("{name} is the system default, so it cannot also read the default. Pick your real microphone."),
                );
            }
        });
    });
    let draft = &mut app.draft.mic;
    section(ui, "Effects", |ui| {
        ui.add_enabled_ui(draft.enabled, |ui| {
            ui.checkbox(&mut draft.studio_voice.enabled, "Studio Voice");
            ui.label(
                RichText::new(
                    "Makes your voice sound like a studio mic. Replaces noise and echo removal.",
                )
                .small()
                .color(theme::MUTED),
            );
            ui.add_space(4.0);
            ui.add_enabled_ui(!draft.studio_voice.enabled, |ui| {
                adjustable(
                    ui,
                    "Noise removal",
                    "Removes background noise such as typing, fans and traffic.",
                    &mut draft.noise_removal,
                );
                adjustable(
                    ui,
                    "Room echo removal",
                    "Removes the echo of a large or empty room.",
                    &mut draft.room_echo_removal,
                );
            });
        });
    });
    mic_default(app, ui);
    let draft = &mut app.draft.mic;
    egui::CollapsingHeader::new("Advanced")
        .id_salt("mic-advanced")
        .show(ui, |ui| {
            fixed_id(ui, "mic-advanced", |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Keep the model loaded after use for");
                    whole_number(ui, &mut draft.unload_after_minutes, 1440);
                    ui.label("minutes");
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("Name shown to apps");
                    ui.text_edit_singleline(&mut draft.name);
                });
            });
        });
}

fn mic_default(app: &mut App, ui: &mut egui::Ui) {
    let default_is_us = app.default_source.as_deref() == Some(Kind::Mic.node_name());
    let saved_input = app.saved().map(|c| c.mic.input.clone());
    let name = app.mic_name();
    section(ui, "Use in apps", |ui| {
        ui.label(format!(
            "Select “{name}” as the microphone in your app, or make it the system default."
        ));
        ui.horizontal_wrapped(|ui| {
            let can = saved_input.as_deref().is_some_and(|i| i != "default") && !default_is_us;
            let button = ui.add_enabled(can, egui::Button::new("Make it the system default"));
            if button.clicked() {
                // WirePlumber switches the default a moment later, so don't read it back.
                match devices::make_default_mic() {
                    Ok(()) => {
                        app.note(format!("{name} is now the default input."));
                        app.default_source = Some(Kind::Mic.node_name().to_owned());
                    }
                    Err(e) => app.fail(format!("{e:#}")),
                }
            }
            if default_is_us {
                ui.colored_label(theme::GOOD, format!("{name} is the default input."));
            } else if !can {
                ui.label(
                    RichText::new("Pick and apply your real microphone first.").color(theme::MUTED),
                );
            }
        });
    });
}

fn speaker(app: &mut App, ui: &mut egui::Ui) {
    heading(app, ui, "Speaker", Page::Speaker);
    let name = app.speaker_name();
    let draft = &mut app.draft.speaker;
    section(ui, "Device", |ui| {
        ui.checkbox(
            &mut draft.enabled,
            format!("Create the virtual speaker “{name}”"),
        );
        ui.label(
            RichText::new(format!(
                "Cleans up what others say in calls. Select “{name}” as the output in your call app. It is mono and tuned for speech, so keep music and games on your real output.",
            ))
            .small()
            .color(theme::MUTED),
        );
        ui.add_enabled_ui(draft.enabled, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Play through");
                node_picker(
                    ui,
                    "speaker-output",
                    &mut draft.output,
                    &app.sinks,
                    "System default",
                );
                if ui.small_button("Refresh").clicked() {
                    app.sinks = devices::query().sinks;
                }
            });
        });
    });
    let draft = &mut app.draft.speaker;
    section(ui, "Effects", |ui| {
        ui.add_enabled_ui(draft.enabled, |ui| {
            adjustable(
                ui,
                "Noise removal",
                "Removes background noise from the other side of the call.",
                &mut draft.noise_removal,
            );
            adjustable(
                ui,
                "Room echo removal",
                "Removes room echo from the other side of the call.",
                &mut draft.room_echo_removal,
            );
        });
    });
    egui::CollapsingHeader::new("Advanced")
        .id_salt("speaker-advanced")
        .show(ui, |ui| {
            fixed_id(ui, "speaker-advanced", |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Keep the model loaded after use for");
                    whole_number(ui, &mut draft.unload_after_minutes, 1440);
                    ui.label("minutes");
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("Name shown to apps");
                    ui.text_edit_singleline(&mut draft.name);
                });
            });
        });
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Background {
    Real,
    Blur,
    Image,
    Removal,
}

fn camera(app: &mut App, ui: &mut egui::Ui) {
    heading(app, ui, "Camera", Page::Camera);
    camera_setup(app, ui);
    camera_device(app, ui);
    let picking = app.picking();
    if camera_effects(ui, &mut app.draft.camera, picking) {
        let ctx = ui.ctx().clone();
        app.pick_background(&ctx);
    }
    camera_preview(app, ui);
    let webcam = app
        .webcams
        .iter()
        .find(|w| w.path == app.draft.camera.input);
    camera_advanced(ui, &mut app.draft.camera, webcam);
}

fn camera_setup(app: &mut App, ui: &mut egui::Ui) {
    if let Some(Err(hint)) = app.loopback.clone()
        && app.draft.camera.enabled
    {
        warning(ui, theme::WARN, |ui| {
            ui.label(RichText::new("The virtual camera is not set up").strong());
            ui.label(&hint.text);
            if !hint.commands.is_empty() {
                ui.label("Run these commands in a terminal; they need administrator rights:");
                commands_list(ui, &hint.commands);
                ui.label(
                    RichText::new("If you joined the video group, log out and back in afterwards.")
                        .small()
                        .color(theme::MUTED),
                );
            }
            ui.horizontal_wrapped(|ui| {
                if ui.button("Check again").clicked() {
                    app.refresh_camera();
                }
                if ui.button("I don’t use a camera").clicked() {
                    app.draft.camera.enabled = false;
                }
            });
        });
    }
}

fn camera_device(app: &mut App, ui: &mut egui::Ui) {
    let label = match &app.loopback {
        Some(Ok(name)) if !name.is_empty() => format!("Create the virtual camera “{name}”"),
        _ => "Create the virtual camera".to_owned(),
    };
    let webcams = &app.webcams;
    let draft = &mut app.draft.camera;
    section(ui, "Device", |ui| {
        ui.checkbox(&mut draft.enabled, label);
        ui.add_enabled_ui(draft.enabled, |ui| {
            egui::Grid::new("camera-device")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Your webcam");
                    let selected = webcams.iter().find(|w| w.path == draft.input).map_or_else(
                        || format!("{} (not found)", draft.input),
                        |w| format!("{} ({})", w.name, w.path),
                    );
                    egui::ComboBox::from_id_salt("camera-input")
                        .selected_text(selected)
                        .width(ui.available_width().clamp(160.0, 320.0))
                        .show_ui(ui, |ui| {
                            for webcam in webcams {
                                ui.selectable_value(
                                    &mut draft.input,
                                    webcam.path.clone(),
                                    format!("{} ({})", webcam.name, webcam.path),
                                );
                            }
                        });
                    ui.end_row();

                    ui.label("Resolution");
                    let sizes = webcams
                        .iter()
                        .find(|w| w.path == draft.input)
                        .map(|w| w.sizes.clone())
                        .unwrap_or_default();
                    let mut size = (draft.width, draft.height);
                    egui::ComboBox::from_id_salt("camera-size")
                        .selected_text(format!("{}×{}", size.0, size.1))
                        .show_ui(ui, |ui| {
                            for (w, h) in sizes {
                                ui.selectable_value(&mut size, (w, h), format!("{w}×{h}"));
                            }
                        });
                    (draft.width, draft.height) = size;
                    ui.end_row();

                    ui.label("Frame rate");
                    egui::ComboBox::from_id_salt("camera-fps")
                        .selected_text(format!("{} fps", draft.fps))
                        .show_ui(ui, |ui| {
                            for fps in [15, 24, 30, 60] {
                                ui.selectable_value(&mut draft.fps, fps, format!("{fps} fps"));
                            }
                        });
                    ui.end_row();
                });
        });
    });
}

fn camera_effects(ui: &mut egui::Ui, draft: &mut CameraConfig, picking: bool) -> bool {
    let mut pick = false;
    section(ui, "Effects", |ui| {
        ui.add_enabled_ui(draft.enabled, |ui| {
            ui.checkbox(
                &mut draft.video_noise_removal.enabled,
                "Video noise removal",
            );
            ui.label(
                RichText::new("Removes grain in low light.")
                    .small()
                    .color(theme::MUTED),
            );
            ui.add_space(6.0);

            pick = background(ui, draft, picking);
            ui.add_space(6.0);

            ui.checkbox(&mut draft.studio_light.enabled, "Studio Light");
            ui.add_enabled_ui(draft.studio_light.enabled, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(24.0);
                    ui.label("Strength");
                    strength(ui, &mut draft.studio_light.strength);
                });
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(24.0);
                    ui.label("Light");
                    egui::ComboBox::from_id_salt("light-preset")
                        .selected_text(preset_name(draft.studio_light.preset))
                        .show_ui(ui, |ui| {
                            for preset in LightPreset::ALL {
                                ui.selectable_value(
                                    &mut draft.studio_light.preset,
                                    preset,
                                    preset_name(preset),
                                );
                            }
                        });
                });
            });
            ui.label(
                RichText::new("Lights your face as if by a studio light.")
                    .small()
                    .color(theme::MUTED),
            );
            ui.add_space(6.0);
            ui.checkbox(&mut draft.eye_contact.enabled, "Eye Contact");
            ui.label(
                RichText::new("Points your eyes at the camera while you look at the screen.")
                    .small()
                    .color(theme::MUTED),
            );
            ui.add_space(4.0);
            ui.checkbox(&mut draft.auto_frame.enabled, "Auto Frame");
            ui.label(
                RichText::new("Zooms in and follows your face as you move.")
                    .small()
                    .color(theme::MUTED),
            );
        });
    });
    pick
}

/// Returns true when the user asked to pick a background image.
fn background(ui: &mut egui::Ui, draft: &mut CameraConfig, picking: bool) -> bool {
    ui.label("Background");
    let current = if draft.background.is_some() {
        Background::Image
    } else if draft.background_blur.enabled {
        Background::Blur
    } else if draft.background_removal.enabled {
        Background::Removal
    } else {
        Background::Real
    };
    let mut chosen = current;
    ui.horizontal_wrapped(|ui| {
        ui.radio_value(&mut chosen, Background::Real, "Keep");
        ui.radio_value(&mut chosen, Background::Blur, "Blur");
        ui.radio_value(&mut chosen, Background::Image, "Replace with an image");
        ui.radio_value(&mut chosen, Background::Removal, "Remove (black)");
    });
    let mut pick = false;
    if chosen != current {
        draft.background = (chosen == Background::Image).then(String::new);
        draft.background_blur.enabled = chosen == Background::Blur;
        draft.background_removal.enabled = chosen == Background::Removal;
        pick = chosen == Background::Image;
    }
    match chosen {
        Background::Blur => {
            ui.horizontal_wrapped(|ui| {
                ui.add_space(24.0);
                ui.label("Strength");
                strength(ui, &mut draft.background_blur.strength);
            });
        }
        Background::Image => {
            ui.horizontal_wrapped(|ui| {
                ui.add_space(24.0);
                let path = draft.background.get_or_insert_with(String::new);
                ui.add(
                    egui::TextEdit::singleline(path)
                        .desired_width(ui.available_width().clamp(160.0, 320.0)),
                );
                pick |= ui
                    .add_enabled(!picking, egui::Button::new("Browse…"))
                    .clicked();
            });
        }
        Background::Real | Background::Removal => {}
    }
    pick && !picking
}

fn camera_preview(app: &mut App, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let loopback_ok = app.loopback.as_ref().is_some_and(Result::is_ok);
    section(ui, "Preview", |ui| {
        let usable =
            loopback_ok && app.saved().is_some_and(|c| c.camera.enabled) && app.service.active();
        let mut on = app.preview.is_some();
        // Starting one during a restart would hold the virtual camera at its old size; an
        // open one survives reloads, and `apply` closes it before a restart.
        ui.add_enabled_ui(usable && (on || app.busy().is_none()), |ui| {
            let label = if on { "Hide preview" } else { "Show preview" };
            if ui.button(label).clicked() {
                on = !on;
            }
        });
        ui.label(
            RichText::new(
                "Shows the applied settings. While it is open, the effects run and use the GPU.",
            )
            .small()
            .color(theme::MUTED),
        );
        if !usable {
            on = false;
        }
        match (on, app.preview.is_some()) {
            (true, false) => {
                let device = app
                    .saved()
                    .map_or_else(String::new, |c| c.camera.device.clone());
                app.preview = Some(Preview::start(device, ctx.clone()));
            }
            (false, true) => app.preview = None,
            _ => {}
        }
        if let Some(preview) = &mut app.preview {
            if let Some(texture) = preview.texture(&ctx) {
                let size = texture.size_vec2();
                let width = ui.available_width().min(size.x * 2.0);
                ui.image((texture.id(), egui::vec2(width, width * size.y / size.x)));
            } else {
                ui.horizontal_wrapped(|ui| {
                    ui.spinner();
                    ui.label("Starting the camera…");
                });
            }
            if let Some(error) = preview.error() {
                ui.colored_label(theme::BAD, error);
            }
        }
    });
}

fn camera_advanced(ui: &mut egui::Ui, draft: &mut CameraConfig, webcam: Option<&Webcam>) {
    egui::CollapsingHeader::new("Advanced")
        .id_salt("camera-advanced")
        .show(ui, |ui| {
            fixed_id(ui, "camera-advanced", |ui| {
                egui::Grid::new("camera-advanced-grid")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Webcam format");
                        let current = draft.input_format;
                        let (width, height) = (draft.width, draft.height);
                        let offered = |format| {
                            webcam.is_none_or(|w| w.format_for(format, width, height).is_some())
                        };
                        let label = |format| {
                            if format == InputFormat::Auto || offered(format) {
                                format_name(format).to_owned()
                            } else {
                                format!("{} (not offered)", format_name(format))
                            }
                        };
                        egui::ComboBox::from_id_salt("input-format")
                            .selected_text(label(current))
                            .show_ui(ui, |ui| {
                                for format in [
                                    InputFormat::Auto,
                                    InputFormat::Mjpeg,
                                    InputFormat::Yuyv,
                                    InputFormat::Nv12,
                                ]
                                .into_iter()
                                .filter(|&format| format == current || offered(format))
                                {
                                    ui.selectable_value(
                                        &mut draft.input_format,
                                        format,
                                        label(format),
                                    );
                                }
                            });
                        ui.end_row();
                        ui.label("Webcam colors");
                        let raw = webcam
                            .and_then(|w| w.format_for(draft.input_format, width, height))
                            .map_or(draft.input_format != InputFormat::Mjpeg, |format| {
                                format != Format::Mjpeg
                            });
                        ui.add_enabled_ui(raw, |ui| {
                            egui::ComboBox::from_id_salt("input-color")
                                .selected_text(color_name(draft.input_color))
                                .show_ui(ui, |ui| {
                                    for color in [
                                        InputColor::Auto,
                                        InputColor::Bt601,
                                        InputColor::Bt601Full,
                                        InputColor::Bt709,
                                        InputColor::Bt709Full,
                                    ] {
                                        ui.selectable_value(
                                            &mut draft.input_color,
                                            color,
                                            color_name(color),
                                        );
                                    }
                                })
                                .response
                                .on_hover_text(
                                    "Try full range if dark and bright areas lose detail.",
                                )
                                .on_disabled_hover_text(
                                    "Ignored for MJPEG, which is always BT.601 full range.",
                                );
                        });
                        ui.end_row();
                        ui.label("Parallel decoding");
                        egui::ComboBox::from_id_salt("parallel-decode")
                            .selected_text(decode_name(draft.parallel_decode))
                            .show_ui(ui, |ui| {
                                for mode in [
                                    ParallelDecode::Auto,
                                    ParallelDecode::On,
                                    ParallelDecode::Off,
                                ] {
                                    ui.selectable_value(
                                        &mut draft.parallel_decode,
                                        mode,
                                        decode_name(mode),
                                    );
                                }
                            });
                        ui.end_row();
                        ui.label("Virtual camera device");
                        ui.text_edit_singleline(&mut draft.device);
                        ui.end_row();
                    });
            });
        });
}

fn preset_name(preset: LightPreset) -> &'static str {
    match preset {
        LightPreset::Cooler => "Cooler",
        LightPreset::Cool => "Cool",
        LightPreset::Neutral => "Neutral",
        LightPreset::Warm => "Warm",
        LightPreset::Warmer => "Warmer",
    }
}

fn format_name(format: InputFormat) -> &'static str {
    match format {
        InputFormat::Auto => "Automatic",
        InputFormat::Mjpeg => "MJPEG",
        InputFormat::Yuyv => "YUYV",
        InputFormat::Nv12 => "NV12",
    }
}

fn color_name(color: InputColor) -> &'static str {
    match color {
        InputColor::Auto => "Automatic (from the driver)",
        InputColor::Bt601 => "BT.601, limited range",
        InputColor::Bt601Full => "BT.601, full range",
        InputColor::Bt709 => "BT.709, limited range",
        InputColor::Bt709Full => "BT.709, full range",
    }
}

fn decode_name(mode: ParallelDecode) -> &'static str {
    match mode {
        ParallelDecode::Auto => "Automatic (above 1080p30)",
        ParallelDecode::On => "On",
        ParallelDecode::Off => "Off",
    }
}

fn commands_list(ui: &mut egui::Ui, commands: &[String]) {
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

fn setup(app: &mut App, ui: &mut egui::Ui) {
    ui.heading("Setup");
    ui.add_space(6.0);
    setup_files(app, ui);
    setup_service(app, ui);
    setup_checks(app, ui);
}

fn setup_files(app: &mut App, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    section(ui, "NVIDIA Broadcast files", |ui| {
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
                if let Ok(gpu) = &checks.gpu {
                    ui.label(format!("GPU: {gpu}"));
                }
                match &checks.setup_needed {
                    Some(reason) => {
                        ui.colored_label(theme::WARN, reason);
                        ui.label(
                            "Downloads NVIDIA Broadcast for your GPU (about 2.2 GB) from NVIDIA, \
                             asks you to accept NVIDIA's licence, and extracts the effects \
                             (about 5 GB of free space needed).",
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

fn setup_service(app: &mut App, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    section(ui, "Service", |ui| {
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
        egui::CollapsingHeader::new("Service log")
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

fn setup_checks(app: &mut App, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    section(ui, "System check", |ui| {
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
