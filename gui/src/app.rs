use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use broadcast_linux::config::{self, Config};
use broadcast_linux::doctor::{self, Report};
use broadcast_linux::gpu::{self, Chosen, Gpu};
use broadcast_linux::graph::AudioNode;
use broadcast_linux::nvidia::{Feature, Installation};
use broadcast_linux::paths::Paths;
use broadcast_linux::status::{Device, State, Status};
use broadcast_linux::webcam::{self, Webcam};
use broadcast_linux::{VERSION, setup, v4l2};
use eframe::egui;

use crate::config_file::{ChangedOnDisk, ConfigFile};
use crate::devices;
use crate::files::Files;
use crate::gpu_bar::WholeGpu;
use crate::preview::Preview;
use crate::service::{self, Link};
use crate::setup_task::{Outcome, SetupTask};
use crate::{pages, theme};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Audio,
    Video,
    Files,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    Mic,
    Speaker,
    Camera,
}

pub struct Message {
    pub error: bool,
    pub text: String,
}

/// Gathered off the UI thread.
pub struct Checks {
    /// From the saved config, not the draft.
    pub gpu: Result<Chosen, String>,
    pub gpus: Vec<Gpu>,
    /// Generation of the installed NVIDIA files.
    pub installed_for: Option<String>,
    /// Why setup must run, if it must.
    pub setup_needed: Option<String>,
    pub report: Report,
    /// Effects the installed files can't run, with the reason.
    pub missing: Vec<(Feature, String)>,
    pub older_build: Option<String>,
}

pub struct App {
    pub page: Page,
    pub link: Link,
    pub paths: Paths,
    pub file: Result<ConfigFile, String>,
    pub draft: Config,
    config_seen: Option<SystemTime>,
    last_poll: Instant,
    pub service: Arc<ServiceState>,
    pub checks: Option<Checks>,
    checks_task: Option<Receiver<Checks>>,
    /// Requested while a check ran, which may have read an older config.
    recheck: bool,
    pub sources: Vec<AudioNode>,
    pub sinks: Vec<AudioNode>,
    pub default_source: Option<String>,
    pub webcams: Vec<Webcam>,
    /// The virtual camera's label, or what to fix. `None` until the Video page checks.
    pub loopback: Option<Result<String, v4l2::Hint>>,
    task: Option<(String, Receiver<Result<String, String>>)>,
    pub message: Option<Message>,
    pub changed_on_disk: bool,
    pub setup: Option<SetupTask>,
    pub preview: Option<Preview>,
    pub journal: Option<String>,
    picker: Option<Receiver<Option<String>>>,
    /// Saved, but the service has not taken it yet; `true` when that needs a restart.
    unapplied: Option<bool>,
    pub files: Files,
    pub whole_gpu: WholeGpu,
}

/// Refreshed every few seconds on a background thread.
#[derive(Default)]
pub struct ServiceState {
    active: AtomicBool,
    enabled: AtomicBool,
    since: Mutex<Option<Instant>>,
}

impl ServiceState {
    pub fn active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    fn running_for(&self) -> Duration {
        self.since
            .lock()
            .unwrap()
            .map_or(Duration::ZERO, |t| t.elapsed())
    }

    fn refresh(&self) -> bool {
        let active = service::is_active();
        let enabled = service::is_enabled();
        let changed = self.active.swap(active, Ordering::Relaxed) != active
            || self.enabled.swap(enabled, Ordering::Relaxed) != enabled;
        let mut since = self.since.lock().unwrap();
        match (active, *since) {
            (true, None) => *since = Some(Instant::now()),
            (false, Some(_)) => *since = None,
            _ => {}
        }
        changed
    }
}

impl App {
    pub fn new(ctx: &egui::Context) -> Self {
        theme::install(ctx);
        let paths = Paths::new().expect("HOME is set");
        let file = ConfigFile::load(&paths.config).map_err(|e| format!("{e:#}"));
        let draft = file
            .as_ref()
            .map_or_else(|_| Config::default(), |f| f.saved.clone());
        let service = Arc::new(ServiceState::default());
        service.refresh();
        thread::spawn({
            let (service, ctx) = (Arc::clone(&service), ctx.clone());
            move || {
                loop {
                    thread::sleep(Duration::from_secs(2));
                    if service.refresh() {
                        ctx.request_repaint();
                    }
                }
            }
        });
        let link = Link::start(ctx.clone());
        let mut app = Self {
            page: Page::Audio,
            link: link.clone(),
            config_seen: modified(&paths),
            paths,
            file,
            draft,
            last_poll: Instant::now(),
            service,
            checks: None,
            checks_task: None,
            recheck: false,
            sources: Vec::new(),
            sinks: Vec::new(),
            default_source: None,
            webcams: Vec::new(),
            loopback: None,
            task: None,
            message: None,
            changed_on_disk: false,
            setup: None,
            preview: None,
            journal: None,
            picker: None,
            unapplied: None,
            files: Files::default(),
            whole_gpu: WholeGpu::start(ctx.clone(), link.clone()),
        };
        app.run_checks(ctx);
        app.refresh_devices();
        app
    }

    pub fn saved(&self) -> Option<&Config> {
        self.file.as_ref().ok().map(|f| &f.saved)
    }

    /// The applied name, which is what apps see.
    pub fn mic_name(&self) -> String {
        self.saved()
            .map_or_else(|| Config::default().mic.name, |c| c.mic.name.clone())
    }

    pub fn speaker_name(&self) -> String {
        self.saved().map_or_else(
            || Config::default().speaker.name,
            |c| c.speaker.name.clone(),
        )
    }

    pub fn dirty(&self) -> bool {
        self.saved().is_some_and(|saved| *saved != self.draft)
    }

    pub fn busy(&self) -> Option<&str> {
        self.task.as_ref().map(|(label, _)| label.as_str())
    }

    pub fn status(&self) -> Option<Status> {
        self.link.status()
    }

    /// Running another version, or one too old to have a status socket.
    pub fn service_outdated(&self) -> bool {
        match self.status() {
            Some(status) => status.version != VERSION,
            None => self.service.active() && self.service.running_for() > Duration::from_secs(3),
        }
    }

    pub fn unapplied(&self) -> bool {
        self.unapplied.is_some()
    }

    pub fn restart_needed(&self) -> bool {
        let Some(saved) = self.saved() else {
            return false;
        };
        if self.unapplied == Some(true) {
            return true;
        }
        match self.status() {
            Some(status) => {
                status.version != VERSION
                    || status.restart_pending
                    || config::needs_restart(saved, &self.draft)
            }
            None => self.service.active(),
        }
    }

    pub fn device(&self, endpoint: Endpoint) -> Option<Device> {
        let status = self.status()?;
        Some(match endpoint {
            Endpoint::Mic => status.mic,
            Endpoint::Speaker => status.speaker,
            Endpoint::Camera => status.camera,
        })
    }

    pub fn show(&mut self, page: Page, ctx: &egui::Context) {
        if page == self.page {
            return;
        }
        if page != Page::Video {
            self.preview = None;
        }
        self.page = page;
        match page {
            Page::Audio => self.refresh_devices(),
            Page::Video => self.refresh_camera(),
            Page::Files => self.files.opened(),
            Page::Settings => self.run_checks(ctx),
        }
    }

    pub fn refresh_devices(&mut self) {
        let devices = devices::query();
        self.sources = devices.sources;
        self.sinks = devices.sinks;
        self.default_source = devices.default_source;
    }

    pub fn refresh_camera(&mut self) {
        self.webcams = webcam::list();
        self.loopback = Some(v4l2::check(&self.draft.camera.device));
    }

    pub fn run_checks(&mut self, ctx: &egui::Context) {
        if self.checks_task.is_some() {
            self.recheck = true;
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let paths = Paths::new().expect("HOME is set");
        thread::spawn(move || {
            let gpus = gpu::list().map_err(|e| format!("{e:#}"));
            let wanted = Config::load(&paths.config)
                .ok()
                .and_then(|config| config.service.gpu);
            let gpu = gpus.as_ref().map_err(Clone::clone).and_then(|gpus| {
                gpu::choose(gpus, wanted.as_deref()).map_err(|e| format!("{e:#}"))
            });
            let setup_needed = match &gpu {
                Ok(gpu) => setup::needed(&paths, gpu.generation),
                Err(e) => Some(e.clone()),
            };
            let install = Installation::find(&paths).ok();
            let missing = install.as_ref().map_or_else(Vec::new, |install| {
                Feature::ALL
                    .into_iter()
                    .filter_map(|feature| {
                        let reason = if install.needs_setup(feature) {
                            format!(
                                "Needs NVIDIA Broadcast {}. Click “Download and install” in Settings.",
                                release(setup::BUILD)
                            )
                        } else {
                            install.missing(feature)?
                        };
                        Some((feature, reason))
                    })
                    .collect()
            });
            let checks = Checks {
                gpu,
                gpus: gpus.unwrap_or_default(),
                installed_for: install.as_ref().and_then(Installation::generation),
                setup_needed,
                report: doctor::check(&paths),
                missing,
                older_build: install.as_ref().and_then(Installation::older_build),
            };
            let _ = tx.send(checks);
            ctx.request_repaint();
        });
        self.checks_task = Some(rx);
    }

    pub fn checking(&self) -> bool {
        self.checks_task.is_some()
    }

    pub fn setup_needed(&self) -> Option<&str> {
        self.checks.as_ref()?.setup_needed.as_deref()
    }

    pub fn gpu_fallback(&self) -> bool {
        self.checks
            .as_ref()
            .is_some_and(|checks| checks.gpu.as_ref().is_ok_and(|gpu| gpu.fallback.is_some()))
    }

    /// The older release in use, when setup only has to update NVIDIA's files.
    pub fn update_from(&self) -> Option<&str> {
        let checks = self.checks.as_ref()?;
        checks.setup_needed.as_ref()?;
        checks.gpu.as_ref().ok()?;
        checks.older_build.as_deref().map(release)
    }

    /// Why `feature` can't run. `None` also until the checks finish.
    pub fn missing(&self, feature: Feature) -> Option<String> {
        let checks = self.checks.as_ref()?;
        let (_, reason) = checks.missing.iter().find(|(f, _)| *f == feature)?;
        Some(reason.clone())
    }

    pub fn spawn(
        &mut self,
        label: &str,
        ctx: &egui::Context,
        job: impl FnOnce() -> anyhow::Result<String> + Send + 'static,
    ) {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(job().map_err(|e| format!("{e:#}")));
            ctx.request_repaint();
        });
        self.message = None;
        self.task = Some((label.to_owned(), rx));
    }

    pub fn apply(&mut self, overwrite: bool, ctx: &egui::Context) {
        // The service would refuse the whole file.
        if let Err(e) = self.draft.camera.validate() {
            self.fail(format!("{e:#}"));
            return;
        }
        let restart = self.restart_needed();
        let camera = &self.draft.camera;
        let resized = self.saved().is_some_and(|saved| {
            (saved.camera.width, saved.camera.height) != (camera.width, camera.height)
        });
        let camera_in_use = self
            .device(Endpoint::Camera)
            .is_some_and(|camera| camera.readers > 0);
        let gpu_changed =
            self.saved().map(|saved| &saved.service.gpu) != Some(&self.draft.service.gpu);
        let Ok(file) = &mut self.file else {
            return;
        };
        if let Err(e) = file.write(&self.draft, overwrite) {
            if e.is::<ChangedOnDisk>() {
                self.changed_on_disk = true;
            } else {
                self.fail(format!("{e:#}"));
            }
            return;
        }
        self.config_seen = modified(&self.paths);
        self.changed_on_disk = false;
        if gpu_changed {
            self.run_checks(ctx);
        }
        if !self.service.active() {
            self.unapplied = None;
            self.note("Saved. The service is not running, so nothing changed yet.");
            return;
        }
        self.unapplied = Some(restart);
        let link = self.link.clone();
        if restart {
            // An open preview would keep the virtual camera at its old size.
            self.preview = None;
            self.spawn("Restarting the service…", ctx, move || {
                link.restart()?;
                Ok(if resized && camera_in_use {
                    "Applied; the service restarted. Apps using the camera keep the old size \
                     until they close it."
                } else {
                    "Applied; the service restarted."
                }
                .into())
            });
        } else {
            self.spawn("Applying…", ctx, move || {
                link.reload()?;
                Ok("Applied.".into())
            });
        }
    }

    pub fn revert(&mut self) {
        if let Some(saved) = self.saved() {
            self.draft = saved.clone();
        }
    }

    /// Re-reads the file, dropping unapplied changes.
    pub fn reload_file(&mut self) {
        self.file = ConfigFile::load(&self.paths.config).map_err(|e| format!("{e:#}"));
        self.config_seen = modified(&self.paths);
        self.changed_on_disk = false;
        self.revert();
    }

    /// Replaces the unreadable file with the defaults and applies them.
    pub fn reset_file(&mut self, ctx: &egui::Context) {
        match ConfigFile::defaults(&self.paths.config) {
            Ok(file) => {
                self.draft = file.saved.clone();
                self.file = Ok(file);
                // What the running service uses is unknown, so only a restart is sure to apply.
                self.unapplied = Some(true);
                self.apply(true, ctx);
            }
            Err(e) => self.fail(format!("{e:#}")),
        }
    }

    pub fn picking(&self) -> bool {
        self.picker.is_some()
    }

    /// On its own thread: the portal dialog blocks until it closes.
    pub fn pick_background(&mut self, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .set_title("Choose a background image")
                .add_filter("Images", &any_case(&IMAGE_EXTENSIONS))
                .pick_file()
                .map(|p| p.display().to_string());
            let _ = tx.send(picked);
            ctx.request_repaint();
        });
        self.picker = Some(rx);
    }

    pub fn restart_service(&mut self, ctx: &egui::Context) {
        self.preview = None;
        let link = self.link.clone();
        self.spawn("Restarting the service…", ctx, move || {
            link.restart()?;
            Ok("The service restarted.".into())
        });
    }

    pub fn start_service(&mut self, ctx: &egui::Context) {
        let service = Arc::clone(&self.service);
        self.spawn("Starting the service…", ctx, move || {
            service::start()?;
            service.refresh();
            Ok("The service is running.".into())
        });
    }

    pub fn stop_service(&mut self, ctx: &egui::Context) {
        let service = Arc::clone(&self.service);
        self.spawn("Stopping the service…", ctx, move || {
            service::stop()?;
            service.refresh();
            Ok("The service stopped.".into())
        });
    }

    pub fn note(&mut self, text: impl Into<String>) {
        self.message = Some(Message {
            error: false,
            text: text.into(),
        });
    }

    pub fn fail(&mut self, text: impl Into<String>) {
        self.message = Some(Message {
            error: true,
            text: text.into(),
        });
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some((_, rx)) = &self.task
            && let Ok(result) = rx.try_recv()
        {
            self.task = None;
            match result {
                Ok(text) => {
                    self.unapplied = None;
                    self.note(text);
                }
                Err(text) => self.fail(text),
            }
            self.run_checks(ctx);
        }
        if let Some(rx) = &self.checks_task
            && let Ok(checks) = rx.try_recv()
        {
            self.checks_task = None;
            if self.checks.is_none() && checks.setup_needed.is_some() {
                self.page = Page::Settings;
            }
            self.checks = Some(checks);
            if std::mem::take(&mut self.recheck) {
                self.run_checks(ctx);
            }
        }
        if let Some(rx) = &self.picker
            && let Ok(picked) = rx.try_recv()
        {
            self.picker = None;
            if let Some(path) = picked {
                let camera = &mut self.draft.camera;
                camera.background = Some(path);
                camera.background_blur.enabled = false;
                camera.background_removal.enabled = false;
            }
        }
        if let Some(outcome) = self.setup.as_ref().and_then(SetupTask::take_outcome) {
            self.setup = None;
            match outcome {
                Outcome::Done => self.note("Setup complete."),
                Outcome::Cancelled => {
                    self.note("Setup cancelled. Run it again to resume the download.");
                }
                Outcome::Failed(e) => self.fail(e),
            }
            self.run_checks(ctx);
        }
        if self.last_poll.elapsed() >= Duration::from_secs(2) {
            self.last_poll = Instant::now();
            let seen = modified(&self.paths);
            if seen != self.config_seen && !self.dirty() && self.task.is_none() {
                self.reload_file();
            }
        }
        ctx.request_repaint_after(Duration::from_secs(2));
    }
}

fn modified(paths: &Paths) -> Option<SystemTime> {
    fs::metadata(&paths.config).and_then(|m| m.modified()).ok()
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        pages::draw(self, ui);
    }
}

pub fn device_summary(device: &Device) -> (egui::Color32, String) {
    let apps = |n: usize| match n {
        1 => "1 app".to_owned(),
        n => format!("{n} apps"),
    };
    let (color, text) = match device.state {
        State::Disabled => (theme::MUTED, "Off".to_owned()),
        State::Unavailable => (theme::BAD, "Unavailable".to_owned()),
        State::Idle => (theme::MUTED, "Ready, not in use".to_owned()),
        State::Loading => (theme::WARN, "Loading the effect…".to_owned()),
        State::Running => (
            theme::GOOD,
            format!("Running for {}", apps(device.readers.max(1))),
        ),
        State::Paused => (theme::MUTED, "Paused; the model stays loaded".to_owned()),
    };
    match &device.error {
        Some(error) => (theme::BAD, format!("{text}: {error}")),
        None => (color, text),
    }
}

/// "2.3.0" of the build "2.3.0.71988672".
pub fn release(build: &str) -> &str {
    build
        .match_indices('.')
        .nth(2)
        .map_or(build, |(at, _)| &build[..at])
}

pub const IMAGE_EXTENSIONS: [&str; 8] = ["jpg", "jpeg", "png", "webp", "bmp", "gif", "tif", "tiff"];

/// Portal file filters are case-sensitive.
pub fn any_case(extensions: &[&str]) -> Vec<String> {
    extensions
        .iter()
        .flat_map(|ext| [ext.to_ascii_lowercase(), ext.to_ascii_uppercase()])
        .collect()
}
