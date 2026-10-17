use broadcast_linux::config::{
    CameraConfig, FrameGenerationMode, InputColor, InputFormat, LightPreset, ParallelDecode,
    SuperResolutionMode,
};
use broadcast_linux::nvidia::Feature;
use broadcast_linux::webcam::{Format, Webcam};
use eframe::egui::{self, RichText, Sense, vec2};

use crate::app::{App, Endpoint};
use crate::audio::status_line;
use crate::pages::{commands_list, fixed_id, strength, warning};
use crate::preview::Preview;
use crate::theme;
use crate::widgets::{self, effect_row, section_title};

pub fn page(app: &mut App, ui: &mut egui::Ui) {
    if let Some((left, right)) = widgets::split(ui) {
        widgets::pane(ui, left, "video-effects", |ui| effects(app, ui));
        widgets::pane(ui, right, "video-preview", |ui| preview(app, ui));
    } else {
        let area = ui.available_rect_before_wrap();
        widgets::pane(ui, area, "video", |ui| {
            preview(app, ui);
            effects(app, ui);
        });
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Background {
    Real,
    Blur,
    Image,
    Removal,
}

fn background_of(draft: &CameraConfig) -> Background {
    if draft.background.is_some() {
        Background::Image
    } else if draft.background_blur.enabled {
        Background::Blur
    } else if draft.background_removal.enabled {
        Background::Removal
    } else {
        Background::Real
    }
}

fn set_background(draft: &mut CameraConfig, background: Background) {
    draft.background = (background == Background::Image).then(String::new);
    draft.background_blur.enabled = background == Background::Blur;
    draft.background_removal.enabled = background == Background::Removal;
}

fn effects(app: &mut App, ui: &mut egui::Ui) {
    section_title(ui, "Camera");
    status_line(app, ui, Endpoint::Camera);
    loopback_setup(app, ui);
    let name = match &app.loopback {
        Some(Ok(name)) if !name.is_empty() => name.clone(),
        _ => "Broadcast Camera".to_owned(),
    };
    let webcams = &app.webcams;
    let draft = &mut app.draft.camera;
    effect_row(
        ui,
        "camera-device",
        "Virtual camera",
        &format!("Creates “{name}” for your apps to use as their camera."),
        &mut draft.enabled,
        None,
    );
    ui.add_enabled_ui(draft.enabled, |ui| {
        ui.add_space(8.0);
        ui.label("Your webcam");
        let selected = webcams.iter().find(|w| w.path == draft.input).map_or_else(
            || format!("{} (not found)", draft.input),
            |w| format!("{} ({})", w.name, w.path),
        );
        egui::ComboBox::from_id_salt("camera-input")
            .selected_text(selected)
            .width(ui.available_width().min(460.0))
            .show_ui(ui, |ui| {
                for webcam in webcams {
                    ui.selectable_value(
                        &mut draft.input,
                        webcam.path.clone(),
                        format!("{} ({})", webcam.name, webcam.path),
                    );
                }
            });
    });
    camera_effects(app, ui);
}

fn camera_effects(app: &mut App, ui: &mut egui::Ui) {
    section_title(ui, "Camera effects");
    let picking = app.picking();
    let mut pick = false;
    let draft = &mut app.draft.camera;
    ui.add_enabled_ui(draft.enabled, |ui| {
        pick = background_row(ui, "camera-background", draft, picking);
        effect_row(
            ui,
            "camera-noise",
            "Video noise removal",
            "Removes grain in low light.",
            &mut draft.video_noise_removal.enabled,
            None,
        );
        let light = &mut draft.studio_light;
        effect_row(
            ui,
            "camera-light",
            "Studio Light",
            "Lights your face as if by a studio light.",
            &mut light.enabled,
            Some(&mut |ui: &mut egui::Ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Strength");
                    strength(ui, &mut light.strength);
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("Light");
                    egui::ComboBox::from_id_salt("light-preset")
                        .selected_text(preset_name(light.preset))
                        .show_ui(ui, |ui| {
                            for preset in LightPreset::ALL {
                                ui.selectable_value(&mut light.preset, preset, preset_name(preset));
                            }
                        });
                });
            }),
        );
        effect_row(
            ui,
            "camera-eye",
            "Eye Contact",
            "Points your eyes at the camera while you look at the screen.",
            &mut draft.eye_contact.enabled,
            None,
        );
        effect_row(
            ui,
            "camera-frame",
            "Auto Frame",
            "Zooms in and follows your face as you move.",
            &mut draft.auto_frame.enabled,
            None,
        );
    });
    if pick && !picking {
        let ctx = ui.ctx().clone();
        app.pick_background(&ctx);
    }
    rtx_video_effects(app, ui);
}

/// Greyed out, with the reason, when the GPU's files lack the effect and it is off.
fn rtx_video_effects(app: &mut App, ui: &mut egui::Ui) {
    let camera = &app.draft.camera;
    let (width, height, fps) = (camera.width, camera.height, camera.fps);
    let webcam = app.webcams.iter().find(|w| w.path == camera.input);
    let source = webcam.map(|w| w.upscale_source(camera.input_format, width, height));
    let native_sizes = webcam.map(|w| w.sizes.clone()).unwrap_or_default();
    let upscale_missing = app.missing(Feature::SuperResolution).or_else(|| {
        matches!(source, Some(None)).then(|| {
            format!("Your webcam has no size smaller than {width} × {height} with the same shape.")
        })
    });
    let generation_missing = app.missing(Feature::FrameGeneration);
    let draft = &mut app.draft.camera;
    ui.add_enabled_ui(draft.enabled, |ui| {
        let upscale = &mut draft.super_resolution;
        let upscaling = upscale.enabled;
        ui.add_enabled_ui(upscale_missing.is_none() || upscale.enabled, |ui| {
            effect_row(
                ui,
                "camera-super-res",
                "Super Resolution",
                upscale_missing.as_deref().unwrap_or(
                    "Captures your webcam at a lower resolution and upscales it with AI.",
                ),
                &mut upscale.enabled,
                Some(&mut |ui: &mut egui::Ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Mode");
                        ui.radio_value(&mut upscale.mode, SuperResolutionMode::Quality, "Quality");
                        ui.radio_value(
                            &mut upscale.mode,
                            SuperResolutionMode::Performance,
                            "Performance",
                        );
                    });
                    if let Some(Some((w, h))) = source {
                        ui.label(
                            RichText::new(format!("Captures {w} × {h} for {width} × {height}."))
                                .small()
                                .color(theme::MUTED),
                        );
                    }
                }),
            );
        });
        // An upscaled size the webcam can't capture would stop the camera.
        if upscaling
            && !draft.super_resolution.enabled
            && let Some(size) = native_size(&native_sizes, (draft.width, draft.height))
        {
            (draft.width, draft.height) = size;
        }
        let generation = &mut draft.frame_generation;
        ui.add_enabled_ui(generation_missing.is_none() || generation.enabled, |ui| {
            effect_row(
                ui,
                "camera-frame-gen",
                "Frame Generation",
                generation_missing.as_deref().unwrap_or(
                    "Runs your webcam at a lower frame rate and makes the frames in between with AI.",
                ),
                &mut generation.enabled,
                Some(&mut |ui: &mut egui::Ui| {
                    generation_options(ui, &mut generation.multiplier, &mut generation.mode, fps);
                }),
            );
        });
        // The service refuses a frame rate the multiplier doesn't divide.
        if generation.enabled
            && !fps.is_multiple_of(generation.multiplier)
            && let Some(multiplier) = (2..=4).rev().find(|&m| fps.is_multiple_of(m))
        {
            generation.multiplier = multiplier;
        }
    });
}

/// `None` when `size` is in `sizes`, else the largest of its shape, else the largest.
fn native_size(sizes: &[(u32, u32)], (width, height): (u32, u32)) -> Option<(u32, u32)> {
    if sizes.contains(&(width, height)) {
        return None;
    }
    sizes
        .iter()
        .find(|&&(w, h)| u64::from(w) * u64::from(height) == u64::from(h) * u64::from(width))
        .or(sizes.first())
        .copied()
}

fn generation_options(
    ui: &mut egui::Ui,
    multiplier: &mut u32,
    mode: &mut FrameGenerationMode,
    fps: u32,
) {
    ui.horizontal_wrapped(|ui| {
        ui.label("Multiplier");
        for m in 2..=4 {
            ui.add_enabled_ui(fps.is_multiple_of(m), |ui| {
                ui.radio_value(multiplier, m, format!("{m}×"));
            })
            .response
            .on_disabled_hover_text(format!("Needs a frame rate that divides by {m}."));
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.label("Mode");
        for (value, name) in [
            (FrameGenerationMode::Performance, "Performance"),
            (FrameGenerationMode::Balanced, "Balanced"),
            (FrameGenerationMode::Quality, "Quality"),
        ] {
            ui.radio_value(mode, value, name);
        }
    });
    let m = (*multiplier).max(1);
    let note = if fps.is_multiple_of(m) {
        format!(
            "Your webcam runs at {} fps for {fps} fps. Adds about {} ms of delay.",
            fps / m,
            (m - 1) * 1000 / fps.max(1)
        )
    } else {
        "Pick a frame rate that divides by 2, 3 or 4.".to_owned()
    };
    ui.label(RichText::new(note).small().color(theme::MUTED));
}

/// True when the user asked to pick an image.
pub fn background_row(
    ui: &mut egui::Ui,
    id: &str,
    draft: &mut CameraConfig,
    picking: bool,
) -> bool {
    let current = background_of(draft);
    let mut on = current != Background::Real;
    let mut pick = false;
    effect_row(
        ui,
        id,
        "Virtual background",
        "Blurs, replaces or removes your background.",
        &mut on,
        Some(&mut |ui: &mut egui::Ui| pick = background_options(ui, draft, picking)),
    );
    if on != (current != Background::Real) {
        let background = if on {
            Background::Blur
        } else {
            Background::Real
        };
        set_background(draft, background);
    }
    pick
}

/// True when the user asked to pick an image.
fn background_options(ui: &mut egui::Ui, draft: &mut CameraConfig, picking: bool) -> bool {
    let current = background_of(draft);
    let mut chosen = current;
    ui.horizontal_wrapped(|ui| {
        ui.radio_value(&mut chosen, Background::Blur, "Blur");
        ui.radio_value(&mut chosen, Background::Image, "Replace with an image");
        ui.radio_value(&mut chosen, Background::Removal, "Remove (black)");
    });
    let mut pick = false;
    if chosen != current {
        set_background(draft, chosen);
        pick = chosen == Background::Image;
    }
    match chosen {
        Background::Blur => {
            ui.horizontal_wrapped(|ui| {
                ui.label("Strength");
                strength(ui, &mut draft.background_blur.strength);
            });
        }
        Background::Image => {
            ui.horizontal_wrapped(|ui| {
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
    pick
}

fn loopback_setup(app: &mut App, ui: &mut egui::Ui) {
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

/// Why the preview can't open, if it can't.
fn preview_blocked(app: &App) -> Option<&'static str> {
    if !app.loopback.as_ref().is_some_and(Result::is_ok) {
        Some("Set up the virtual camera to see the preview.")
    } else if !app.saved().is_some_and(|c| c.camera.enabled) {
        Some("Turn on the virtual camera and apply to see the preview.")
    } else if !app.service.active() {
        Some("Start the service to see the preview.")
    } else {
        None
    }
}

fn preview(app: &mut App, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    ui.add_space(8.0);
    let blocked = preview_blocked(app);
    let mut on = app.preview.is_some();
    if blocked.is_some() {
        on = false;
    }
    let aspect = (f32::from(u16::try_from(app.draft.camera.height).unwrap_or(9))
        / f32::from(u16::try_from(app.draft.camera.width).unwrap_or(16)).max(1.0))
    .clamp(0.4, 1.0);
    // Leaves room for the effects under it when the window is narrow.
    let height = (ui.available_width() * aspect).min(ui.clip_rect().height() * 0.5);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    ui.painter()
        .rect_filled(rect, 6, theme::surface(ui).preview);
    let mut message = None;
    if let Some(preview) = app.preview.as_mut().filter(|_| on) {
        if let Some(texture) = preview.texture(&ctx) {
            let size = texture.size_vec2();
            let scale = (rect.width() / size.x).min(rect.height() / size.y);
            let fitted = egui::Rect::from_center_size(rect.center(), size * scale);
            egui::Image::new((texture.id(), size))
                .corner_radius(6)
                .paint_at(ui, fitted);
        } else {
            message = Some("Starting the camera…".to_owned());
        }
    } else {
        message = Some(
            blocked
                .unwrap_or("Turn on the preview to see your effects.")
                .to_owned(),
        );
    }
    if let Some(message) = message {
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            message,
            egui::FontId::proportional(16.0),
            theme::MUTED,
        );
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        // Opening during a restart would hold the camera at its old size. An open preview
        // survives reloads, and `apply` closes it before a restart.
        ui.add_enabled_ui(blocked.is_none() && (on || app.busy().is_none()), |ui| {
            widgets::switch(ui, &mut on, "Preview");
            ui.label("Preview");
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            size_menu(app, ui);
        });
    });
    ui.label(
        RichText::new(
            "Shows the applied settings. While it is open, the effects run and use the GPU.",
        )
        .small()
        .color(theme::MUTED),
    );
    if let Some(error) = app.preview.as_ref().and_then(Preview::error) {
        ui.colored_label(theme::BAD, error);
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
    ui.add_space(8.0);
    let webcam = app
        .webcams
        .iter()
        .find(|w| w.path == app.draft.camera.input);
    camera_advanced(ui, &mut app.draft.camera, webcam);
}

fn size_menu(app: &mut App, ui: &mut egui::Ui) {
    let camera = &app.draft.camera;
    let sizes = app
        .webcams
        .iter()
        .find(|w| w.path == camera.input)
        .map(|w| {
            if camera.super_resolution.enabled {
                w.upscaled_sizes(camera.input_format)
            } else {
                w.sizes.clone()
            }
        })
        .unwrap_or_default();
    let draft = &mut app.draft.camera;
    ui.add_enabled_ui(draft.enabled, |ui| {
        let label = format!("{} × {} @ {} fps", draft.width, draft.height, draft.fps);
        ui.menu_button(label, |ui| {
            ui.label(RichText::new("Resolution").strong());
            let mut size = (draft.width, draft.height);
            for (w, h) in sizes {
                ui.selectable_value(&mut size, (w, h), format!("{w} × {h}"));
            }
            (draft.width, draft.height) = size;
            ui.separator();
            ui.label(RichText::new("Frame rate").strong());
            for fps in [15, 24, 30, 60] {
                ui.selectable_value(&mut draft.fps, fps, format!("{fps} fps"));
            }
        });
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
                        let size = (draft.width, draft.height);
                        let upscale = draft.super_resolution.enabled;
                        let capture =
                            move |w: &Webcam, format| capture_format(w, format, size, upscale);
                        let offered = |format| webcam.is_none_or(|w| capture(w, format).is_some());
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
                            .and_then(|w| capture(w, draft.input_format))
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

/// The format the service would capture in, at its capture size, which is smaller with
/// Super Resolution.
fn capture_format(
    webcam: &Webcam,
    format: InputFormat,
    (width, height): (u32, u32),
    upscale: bool,
) -> Option<Format> {
    let (width, height) = if upscale {
        webcam.upscale_source(format, width, height)?
    } else {
        (width, height)
    };
    webcam.format_for(format, width, height)
}

pub fn preset_name(preset: LightPreset) -> &'static str {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_shape_when_super_resolution_turns_off() {
        let sizes = [(1920, 1080), (1280, 960), (1280, 720), (640, 480)];
        assert_eq!(native_size(&sizes, (3840, 2160)), Some((1920, 1080)));
        assert_eq!(native_size(&sizes, (2560, 1920)), Some((1280, 960)));
        assert_eq!(native_size(&sizes, (2000, 1000)), Some((1920, 1080)));
        assert_eq!(native_size(&sizes, (1280, 720)), None);
        assert_eq!(native_size(&[], (3840, 2160)), None);
    }
}
