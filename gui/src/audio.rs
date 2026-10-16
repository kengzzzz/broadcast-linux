use broadcast_linux::audio::Kind;
use broadcast_linux::config::Adjustable;
use eframe::egui::{self, RichText};

use crate::app::{App, Endpoint, device_summary};
use crate::pages::{fixed_id, node_picker, strength, whole_number};
use crate::widgets::{self, card, effect_row, section_title};
use crate::{devices, theme};

pub fn page(app: &mut App, ui: &mut egui::Ui) {
    if let Some((left, right)) = widgets::split(ui) {
        widgets::pane(ui, left, "audio-effects", |ui| effects(app, ui));
        widgets::pane(ui, right, "audio-use", |ui| usage(app, ui));
    } else {
        let area = ui.available_rect_before_wrap();
        widgets::pane(ui, area, "audio", |ui| {
            effects(app, ui);
            usage(app, ui);
        });
    }
}

fn effects(app: &mut App, ui: &mut egui::Ui) {
    mic(app, ui);
    ui.add_space(16.0);
    speaker(app, ui);
}

pub fn status_line(app: &App, ui: &mut egui::Ui, endpoint: Endpoint) {
    if let Some(device) = app.device(endpoint) {
        let (color, text) = device_summary(&device);
        ui.horizontal_wrapped(|ui| {
            theme::dot(ui, color);
            ui.label(RichText::new(text).color(color));
        });
    }
}

fn adjustable(
    ui: &mut egui::Ui,
    id: &str,
    title: &str,
    description: &str,
    effect: &mut Adjustable,
) {
    let Adjustable {
        enabled,
        strength: value,
    } = effect;
    effect_row(
        ui,
        id,
        title,
        description,
        enabled,
        Some(&mut |ui: &mut egui::Ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Strength");
                strength(ui, value);
            });
        }),
    );
}

fn mic(app: &mut App, ui: &mut egui::Ui) {
    section_title(ui, "Microphone");
    status_line(app, ui, Endpoint::Mic);
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
    effect_row(
        ui,
        "mic-device",
        "Virtual microphone",
        &format!("Creates “{name}” for your apps to use as their microphone."),
        &mut draft.enabled,
        None,
    );
    ui.add_enabled_ui(draft.enabled, |ui| {
        ui.add_space(8.0);
        ui.label("Your microphone");
        ui.horizontal(|ui| {
            node_picker(ui, "mic-input", &mut draft.input, &app.sources, &default_label);
            if ui.button("Refresh").clicked() {
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
    section_title(ui, "Microphone effects");
    ui.add_enabled_ui(draft.enabled, |ui| {
        effect_row(
            ui,
            "mic-studio-voice",
            "Studio Voice",
            "Makes your voice sound like a studio mic. Replaces noise and echo removal.",
            &mut draft.studio_voice.enabled,
            None,
        );
        let studio = draft.studio_voice.enabled;
        let replaced = "Off while Studio Voice is on.";
        ui.add_enabled_ui(!studio, |ui| {
            adjustable(
                ui,
                "mic-noise",
                "Noise removal",
                if studio {
                    replaced
                } else {
                    "Removes background noise such as typing, fans and traffic."
                },
                &mut draft.noise_removal,
            );
            adjustable(
                ui,
                "mic-echo",
                "Room echo removal",
                if studio {
                    replaced
                } else {
                    "Removes the echo of a large or empty room."
                },
                &mut draft.room_echo_removal,
            );
        });
    });
}

fn speaker(app: &mut App, ui: &mut egui::Ui) {
    section_title(ui, "Speaker");
    status_line(app, ui, Endpoint::Speaker);
    let name = app.speaker_name();
    let draft = &mut app.draft.speaker;
    effect_row(
        ui,
        "speaker-device",
        "Virtual speaker",
        &format!(
            "Creates “{name}”. Pick it as the output in your call app to clean up what others say."
        ),
        &mut draft.enabled,
        None,
    );
    ui.add_enabled_ui(draft.enabled, |ui| {
        ui.add_space(8.0);
        ui.label("Play through");
        ui.horizontal(|ui| {
            node_picker(
                ui,
                "speaker-output",
                &mut draft.output,
                &app.sinks,
                "System default",
            );
            if ui.button("Refresh").clicked() {
                app.sinks = devices::query().sinks;
            }
        });
    });
    section_title(ui, "Speaker effects");
    ui.add_enabled_ui(draft.enabled, |ui| {
        adjustable(
            ui,
            "speaker-noise",
            "Noise removal",
            "Removes background noise from the other side of the call.",
            &mut draft.noise_removal,
        );
        adjustable(
            ui,
            "speaker-echo",
            "Room echo removal",
            "Removes room echo from the other side of the call.",
            &mut draft.room_echo_removal,
        );
    });
}

fn usage(app: &mut App, ui: &mut egui::Ui) {
    section_title(ui, "Use in apps");
    mic_default(app, ui);
    let name = app.speaker_name();
    card(ui, |ui| {
        ui.label(RichText::new("Speaker").strong());
        ui.label(format!(
            "Select “{name}” as the output in your call app. It is mono and tuned for speech, so keep music and games on your real output."
        ));
    });
    egui::CollapsingHeader::new("Advanced")
        .id_salt("audio-advanced")
        .show(ui, |ui| {
            fixed_id(ui, "audio-advanced", |ui| {
                for (title, draft_minutes, draft_name) in [
                    (
                        "Microphone",
                        &mut app.draft.mic.unload_after_minutes,
                        &mut app.draft.mic.name,
                    ),
                    (
                        "Speaker",
                        &mut app.draft.speaker.unload_after_minutes,
                        &mut app.draft.speaker.name,
                    ),
                ] {
                    ui.label(RichText::new(title).strong());
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Keep the model loaded after use for");
                        whole_number(ui, draft_minutes, 1440);
                        ui.label("minutes");
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Name shown to apps");
                        ui.text_edit_singleline(draft_name);
                    });
                    ui.add_space(6.0);
                }
            });
        });
}

fn mic_default(app: &mut App, ui: &mut egui::Ui) {
    let default_is_us = app.default_source.as_deref() == Some(Kind::Mic.node_name());
    let saved_input = app.saved().map(|c| c.mic.input.clone());
    let name = app.mic_name();
    card(ui, |ui| {
        ui.label(RichText::new("Microphone").strong());
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
