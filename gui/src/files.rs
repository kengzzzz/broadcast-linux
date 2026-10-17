use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use broadcast_linux::camera::expand_home;
use broadcast_linux::config::{
    CameraConfig, FrameGenerationMode, LightPreset, MicConfig, SuperResolutionMode,
};
use broadcast_linux::nvidia::Feature;
use broadcast_linux::paths::Paths;
use broadcast_linux::process::{self, Effects, UPSCALE_HEIGHTS};
use broadcast_linux::progress::{Cancelled, Progress};
use eframe::egui::{self, RichText};

use crate::app::App;
use crate::audio::{adjustable, profile_options};
use crate::pages::{strength, warning};
use crate::theme;
use crate::video::{background_row, preset_name};
use crate::widgets::{self, effect_row, section_title};

/// Unlike the other pages, nothing here is saved to the config.
#[derive(Default)]
pub struct Files {
    items: Arc<Mutex<Vec<Item>>>,
    effects: Effects,
    output_dir: Option<PathBuf>,
    ffmpeg_missing: Option<&'static str>,
    job: Option<Job>,
    picker: Option<(Pick, Receiver<Vec<PathBuf>>)>,
}

struct Item {
    path: PathBuf,
    state: ItemState,
}

#[derive(Clone)]
enum ItemState {
    Waiting,
    Running { step: String, done: u64, total: u64 },
    Done(PathBuf),
    Failed(String),
    Cancelled,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pick {
    Files,
    Folder,
    Output,
    Background,
}

struct Job {
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Job {
    /// Cancels and waits, so closing the window leaves no half-made file.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Report {
    items: Arc<Mutex<Vec<Item>>>,
    index: usize,
    cancel: Arc<AtomicBool>,
    ctx: egui::Context,
}

impl Report {
    fn set(&self, state: ItemState) {
        if let Some(item) = self.items.lock().unwrap().get_mut(self.index) {
            item.state = state;
        }
        self.ctx.request_repaint();
    }
}

impl Progress for Report {
    fn step(&self, message: &str) {
        self.set(ItemState::Running {
            step: message.to_owned(),
            done: 0,
            total: 0,
        });
    }

    fn advance(&self, done: u64, total: u64) {
        if let Some(item) = self.items.lock().unwrap().get_mut(self.index)
            && let ItemState::Running {
                done: was_done,
                total: was_total,
                ..
            } = &mut item.state
        {
            (*was_done, *was_total) = (done, total);
        }
        self.ctx.request_repaint();
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

impl Files {
    pub fn opened(&mut self) {
        self.ffmpeg_missing = process::ffmpeg_missing();
    }

    pub fn running(&self) -> bool {
        self.job
            .as_ref()
            .is_some_and(|job| job.thread.as_ref().is_some_and(|t| !t.is_finished()))
    }

    fn cancelling(&self) -> bool {
        self.job
            .as_ref()
            .is_some_and(|job| job.cancel.load(Ordering::Relaxed))
    }

    /// Folders add the media files directly in them. Rows are only ever appended, since a
    /// running job refers to them by index.
    fn add(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        let paths: Vec<PathBuf> = paths
            .into_iter()
            .flat_map(|path| {
                if path.is_dir() {
                    media_in(&path)
                } else {
                    vec![path]
                }
            })
            .collect();
        let mut items = self.items.lock().unwrap();
        for path in paths {
            if !items.iter().any(|item| item.path == path) {
                items.push(Item {
                    path,
                    state: ItemState::Waiting,
                });
            }
        }
    }

    fn start(&mut self, paths: &Paths, ctx: &egui::Context) {
        for item in self.items.lock().unwrap().iter_mut() {
            if matches!(item.state, ItemState::Failed(_) | ItemState::Cancelled) {
                item.state = ItemState::Waiting;
            }
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let items = Arc::clone(&self.items);
        let (effects, output_dir) = (self.effects.clone(), self.output_dir.clone());
        let paths = Paths {
            data: paths.data.clone(),
            config: paths.config.clone(),
        };
        let ctx = ctx.clone();
        let thread = thread::spawn({
            let cancel = Arc::clone(&cancel);
            move || {
                while !cancel.load(Ordering::Relaxed) {
                    let next = items
                        .lock()
                        .unwrap()
                        .iter()
                        .enumerate()
                        .find(|(_, item)| matches!(item.state, ItemState::Waiting))
                        .map(|(index, item)| (index, item.path.clone()));
                    let Some((index, path)) = next else {
                        break;
                    };
                    let report = Report {
                        items: Arc::clone(&items),
                        index,
                        cancel: Arc::clone(&cancel),
                        ctx: ctx.clone(),
                    };
                    report.step("Starting");
                    let result =
                        process::process(&paths, &path, &effects, output_dir.as_deref(), &report);
                    report.set(match result {
                        Ok(output) => ItemState::Done(output),
                        Err(e) if e.is::<Cancelled>() => ItemState::Cancelled,
                        Err(e) => ItemState::Failed(format!("{e:#}")),
                    });
                }
            }
        });
        self.job = Some(Job {
            cancel,
            thread: Some(thread),
        });
    }

    fn pick(&mut self, pick: Pick, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let dialog = rfd::FileDialog::new();
            let picked = match pick {
                Pick::Files => dialog
                    .set_title("Add video or audio files")
                    .add_filter("Video and audio", &process::EXTENSIONS)
                    .pick_files()
                    .unwrap_or_default(),
                Pick::Folder => dialog
                    .set_title("Add the video and audio files in a folder")
                    .pick_folder()
                    .into_iter()
                    .collect(),
                Pick::Output => dialog
                    .set_title("Save processed files in")
                    .pick_folder()
                    .into_iter()
                    .collect(),
                Pick::Background => dialog
                    .set_title("Choose a background image")
                    .add_filter(
                        "Images",
                        &["jpg", "jpeg", "png", "webp", "bmp", "gif", "tif", "tiff"],
                    )
                    .pick_file()
                    .into_iter()
                    .collect(),
            };
            let _ = tx.send(picked);
            ctx.request_repaint();
        });
        self.picker = Some((pick, rx));
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some((pick, rx)) = &self.picker
            && let Ok(picked) = rx.try_recv()
        {
            let pick = *pick;
            self.picker = None;
            match pick {
                Pick::Files | Pick::Folder => self.add(picked),
                Pick::Output => {
                    if let Some(dir) = picked.into_iter().next() {
                        self.output_dir = Some(dir);
                    }
                }
                Pick::Background => {
                    if let Some(image) = picked.into_iter().next() {
                        self.effects.video.background = Some(image.display().to_string());
                    }
                }
            }
        }
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_path_buf())
                .collect()
        });
        if !dropped.is_empty() {
            self.add(dropped);
        }
        if self.job.is_some() && !self.running() {
            self.job = None;
        }
    }
}

fn media_in(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path.extension().is_some_and(|ext| {
                    let ext = ext.to_string_lossy().to_ascii_lowercase();
                    process::EXTENSIONS.contains(&ext.as_str())
                })
        })
        .collect();
    files.sort();
    files
}

pub fn page(app: &mut App, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    app.files.poll(&ctx);
    if let Some((left, right)) = widgets::split(ui) {
        widgets::pane(ui, left, "files-inputs", |ui| inputs(app, ui));
        widgets::pane(ui, right, "files-effects", |ui| effects(app, ui));
    } else {
        let area = ui.available_rect_before_wrap();
        widgets::pane(ui, area, "files", |ui| {
            inputs(app, ui);
            effects(app, ui);
        });
    }
}

fn inputs(app: &mut App, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let files = &mut app.files;
    section_title(ui, "Input files");
    if files.ffmpeg_missing.is_some() {
        warning(ui, theme::WARN, |ui| {
            ui.label(RichText::new("ffmpeg is not installed").strong());
            ui.label(
                "The File Processor reads and writes files with it. Install ffmpeg with your \
                 package manager (ffmpeg-free on Fedora), then check again.",
            );
            if ui.button("Check again").clicked() {
                files.opened();
            }
        });
    }
    let running = files.running();
    let picking = files.picker.is_some();
    ui.horizontal_wrapped(|ui| {
        ui.add_enabled_ui(!picking, |ui| {
            if ui.button("Add files…").clicked() {
                files.pick(Pick::Files, &ctx);
            }
            if ui.button("Add a folder…").clicked() {
                files.pick(Pick::Folder, &ctx);
            }
        });
        let finished = files
            .items
            .lock()
            .unwrap()
            .iter()
            .any(|item| !matches!(item.state, ItemState::Waiting | ItemState::Running { .. }));
        if finished && !running && ui.button("Clear finished").clicked() {
            files
                .items
                .lock()
                .unwrap()
                .retain(|item| matches!(item.state, ItemState::Waiting));
        }
    });
    // winit only delivers dropped files on X11.
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        ui.label(RichText::new("You can also drop files or folders here.").color(theme::MUTED));
    }
    ui.add_space(6.0);
    file_list(files, ui, running);

    section_title(ui, "Output folder");
    ui.add_enabled_ui(!running, |ui| {
        let beside = files.output_dir.is_none();
        ui.horizontal_wrapped(|ui| {
            if ui.radio(beside, "Beside each file").clicked() {
                files.output_dir = None;
            }
            if ui.add_enabled(!picking, egui::RadioButton::new(!beside, "In a folder…")).clicked() {
                files.pick(Pick::Output, &ctx);
            }
        });
        if let Some(dir) = &files.output_dir {
            ui.label(dir.display().to_string());
        }
        ui.label(
            RichText::new(
                "New files get “-broadcast” added to their name. Videos with video effects become MP4.",
            )
            .small()
            .color(theme::MUTED),
        );
    });
    ui.add_space(10.0);
    process_bar(app, ui);
}

fn file_list(files: &mut Files, ui: &mut egui::Ui, running: bool) {
    let mut remove = None;
    let items = files.items.lock().unwrap();
    if items.is_empty() {
        ui.label(
            RichText::new("No files yet. Add videos or audio to run the effects on them.")
                .color(theme::MUTED),
        );
    }
    for (index, item) in items.iter().enumerate() {
        widgets::card(ui, |ui| {
            ui.horizontal(|ui| {
                let name = item.path.file_name().map_or_else(
                    || item.path.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                ui.label(RichText::new(name).strong())
                    .on_hover_text(item.path.display().to_string());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !running && ui.small_button("Remove").clicked() {
                        remove = Some(index);
                    }
                });
            });
            match &item.state {
                ItemState::Waiting => {
                    ui.label(RichText::new("Waiting").color(theme::MUTED));
                }
                ItemState::Running { step, done, total } => {
                    ui.label(step);
                    #[allow(clippy::cast_precision_loss)]
                    let fraction = if *total > 0 {
                        *done as f32 / *total as f32
                    } else {
                        0.0
                    };
                    ui.add(egui::ProgressBar::new(fraction).show_percentage());
                }
                ItemState::Done(output) => {
                    ui.horizontal_wrapped(|ui| {
                        let name = output
                            .file_name()
                            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                        ui.colored_label(theme::GOOD, format!("Saved as {name}"));
                        if let Some(dir) = output.parent()
                            && ui.small_button("Show folder").clicked()
                        {
                            let _ = Command::new("xdg-open").arg(dir).spawn();
                        }
                    });
                }
                ItemState::Failed(error) => {
                    ui.colored_label(theme::BAD, error);
                }
                ItemState::Cancelled => {
                    ui.label(RichText::new("Cancelled").color(theme::MUTED));
                }
            }
        });
    }
    drop(items);
    if let Some(index) = remove {
        files.items.lock().unwrap().remove(index);
    }
}

fn process_bar(app: &mut App, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let files = &mut app.files;
    if files.running() {
        ui.horizontal_wrapped(|ui| {
            ui.spinner();
            if files.cancelling() {
                ui.label("Cancelling…");
            } else {
                ui.label("Processing…");
                if ui.button("Cancel").clicked()
                    && let Some(job) = &files.job
                {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            }
        });
        return;
    }
    let waiting = files
        .items
        .lock()
        .unwrap()
        .iter()
        .filter(|item| !matches!(item.state, ItemState::Done(_)))
        .count();
    let problem = if files.ffmpeg_missing.is_some() {
        Some("Install ffmpeg first.".to_owned())
    } else if waiting == 0 {
        Some("Add files to process.".to_owned())
    } else if !files.effects.audio_on() && !files.effects.video_on() {
        Some("Turn on at least one effect.".to_owned())
    } else if let Some(problem) = background_problem(&files.effects) {
        Some(problem.to_owned())
    } else {
        files.effects.validate().err().map(|e| format!("{e:#}"))
    };
    let mut start = false;
    ui.horizontal_wrapped(|ui| {
        let label = match waiting {
            0 => "Process files".to_owned(),
            1 => "Process 1 file".to_owned(),
            n => format!("Process {n} files"),
        };
        start = ui
            .add_enabled(problem.is_none(), egui::Button::new(label))
            .clicked();
        if let Some(problem) = &problem {
            ui.label(RichText::new(problem).color(theme::MUTED));
        }
    });
    ui.label(
        RichText::new(
            "Uses the same GPU as the live effects, so they may slow down while files process.",
        )
        .small()
        .color(theme::MUTED),
    );
    if start {
        app.files.start(&app.paths, &ctx);
    }
}

fn background_problem(effects: &Effects) -> Option<&'static str> {
    let image = effects.video.background.as_deref()?.trim();
    if image.is_empty() {
        Some("Choose a background image first.")
    } else if !expand_home(image).is_file() {
        Some("The background image does not exist.")
    } else {
        None
    }
}

fn effects(app: &mut App, ui: &mut egui::Ui) {
    let profiles_missing = app.missing(Feature::MicProfiles);
    let upscale_missing = app.missing(Feature::SuperResolution);
    let generation_missing = app.missing(Feature::FrameGeneration);
    let ctx = ui.ctx().clone();
    let files = &mut app.files;
    let picking = files.picker.is_some();
    let mut pick = false;
    ui.add_enabled_ui(!files.running(), |ui| {
        let effects = &mut files.effects;
        audio_effects(ui, &mut effects.audio, profiles_missing.as_deref());
        pick = video_effects(ui, &mut effects.video, picking);
        rtx_effects(
            ui,
            effects,
            upscale_missing.as_deref(),
            generation_missing.as_deref(),
        );
    });
    if pick && !picking {
        files.pick(Pick::Background, &ctx);
    }
}

fn audio_effects(ui: &mut egui::Ui, audio: &mut MicConfig, profiles_missing: Option<&str>) {
    section_title(ui, "Audio effects");
    let studio = audio.studio_voice.enabled;
    let replaced = "Off while Studio Voice is on.";
    ui.add_enabled_ui(!studio, |ui| {
        adjustable(
            ui,
            "files-noise",
            "Noise removal",
            if studio {
                replaced
            } else {
                "Removes background noise such as typing, fans and traffic."
            },
            &mut audio.noise_removal,
        );
        adjustable(
            ui,
            "files-echo",
            "Room echo removal",
            if studio {
                replaced
            } else {
                "Removes the echo of a large or empty room."
            },
            &mut audio.room_echo_removal,
        );
    });
    let voice = &mut audio.studio_voice;
    effect_row(
        ui,
        "files-studio-voice",
        "Studio Voice",
        "Makes voices sound like a studio mic. Replaces noise and echo removal.",
        &mut voice.enabled,
        Some(&mut |ui: &mut egui::Ui| {
            profile_options(ui, &mut voice.profile, profiles_missing);
        }),
    );
}

/// True when the user asked to pick a background image.
fn video_effects(ui: &mut egui::Ui, video: &mut CameraConfig, picking: bool) -> bool {
    section_title(ui, "Video effects");
    let pick = background_row(ui, "files-background", video, picking);
    effect_row(
        ui,
        "files-video-noise",
        "Video noise removal",
        "Removes grain from video shot in low light.",
        &mut video.video_noise_removal.enabled,
        None,
    );
    let light = &mut video.studio_light;
    effect_row(
        ui,
        "files-light",
        "Studio Light",
        "Lights faces as if by a studio light.",
        &mut light.enabled,
        Some(&mut |ui: &mut egui::Ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Strength");
                strength(ui, &mut light.strength);
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("Light");
                egui::ComboBox::from_id_salt("files-light-preset")
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
        "files-eye",
        "Eye Contact",
        "Points eyes at the camera.",
        &mut video.eye_contact.enabled,
        None,
    );
    effect_row(
        ui,
        "files-frame",
        "Auto Frame",
        "Zooms in and follows the face.",
        &mut video.auto_frame.enabled,
        None,
    );
    pick
}

/// Greyed out, with the reason, when the GPU's files lack the effect and it is off.
fn rtx_effects(
    ui: &mut egui::Ui,
    effects: &mut Effects,
    upscale_missing: Option<&str>,
    generation_missing: Option<&str>,
) {
    let upscale = &mut effects.video.super_resolution;
    let height = &mut effects.upscale_height;
    ui.add_enabled_ui(upscale_missing.is_none() || upscale.enabled, |ui| {
        effect_row(
            ui,
            "files-super-res",
            "Super Resolution",
            upscale_missing.unwrap_or("Upscales videos with AI."),
            &mut upscale.enabled,
            Some(&mut |ui: &mut egui::Ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Height");
                    for h in UPSCALE_HEIGHTS {
                        ui.radio_value(height, h, format!("{h}p"));
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("Mode");
                    ui.radio_value(&mut upscale.mode, SuperResolutionMode::Quality, "Quality");
                    ui.radio_value(
                        &mut upscale.mode,
                        SuperResolutionMode::Performance,
                        "Performance",
                    );
                });
                ui.label(
                    RichText::new("Only for videos shorter than this.")
                        .small()
                        .color(theme::MUTED),
                );
            }),
        );
    });
    let generation = &mut effects.video.frame_generation;
    ui.add_enabled_ui(generation_missing.is_none() || generation.enabled, |ui| {
        effect_row(
            ui,
            "files-frame-gen",
            "Frame Generation",
            generation_missing
                .unwrap_or("Raises the frame rate by making frames in between with AI."),
            &mut generation.enabled,
            Some(&mut |ui: &mut egui::Ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Multiplier");
                    for m in 2..=4 {
                        ui.radio_value(&mut generation.multiplier, m, format!("{m}×"));
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("Mode");
                    for (value, name) in [
                        (FrameGenerationMode::Performance, "Performance"),
                        (FrameGenerationMode::Balanced, "Balanced"),
                        (FrameGenerationMode::Quality, "Quality"),
                    ] {
                        ui.radio_value(&mut generation.mode, value, name);
                    }
                });
            }),
        );
    });
}
