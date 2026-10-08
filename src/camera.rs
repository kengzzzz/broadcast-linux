use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, ErrorKind, PipeWriter, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::{ChildStdout, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder, ImageReader};
use sha2::{Digest, Sha256};

use crate::config::CameraConfig;
use crate::frames::{Decoder, Layout, SharedFrames};
use crate::nvidia::{self, Installation};
use crate::paths::Paths;
use crate::status::{self, State};
use crate::v4l2::Loopback;
use crate::webcam::{self, Capture};
use crate::worker::{Launch, Worker};

/// Placeholder rate while no effect is running; enough to keep the device listed.
const PLACEHOLDER_INTERVAL: Duration = Duration::from_millis(200);

pub enum Control {
    Reload {
        config: CameraConfig,
        idle_timeout: Duration,
    },
    Quit,
}

pub struct Camera {
    commands: Sender<Control>,
    thread: Option<JoinHandle<()>>,
    status: Arc<Mutex<status::Device>>,
}

type Notify = Box<dyn Fn() + Send>;

impl Camera {
    /// Waits for the first placeholder frame, so the device is probed as a camera.
    /// `notify` runs on the camera thread whenever `status` changes.
    pub fn start(
        paths: Paths,
        config: CameraConfig,
        idle_timeout: Duration,
        notify: Notify,
    ) -> Result<Self> {
        let (commands, receiver) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(status::Device {
            state: State::Idle,
            ..status::Device::default()
        }));
        let shared = Arc::clone(&status);
        let thread = thread::spawn(move || {
            let loopback = match Loopback::open(&config.device, config.width, config.height) {
                Ok(loopback) => loopback,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let mut camera = CameraLoop::new(paths, config, idle_timeout, loopback, receiver);
            camera.status = shared;
            camera.notify = notify;
            // SAFETY: no session runs yet, so nothing else writes the loopback's buffers.
            if let Err(e) = unsafe { camera.loopback.write_frame(&camera.placeholder) } {
                eprintln!("camera: writing the first placeholder frame: {e}");
            }
            let _ = ready_tx.send(Ok(()));
            camera.run();
        });
        ready_rx.recv().context("camera thread ended early")??;
        Ok(Self {
            commands,
            thread: Some(thread),
            status,
        })
    }

    pub fn send(&self, command: Control) {
        let _ = self.commands.send(command);
    }

    pub fn status(&self) -> status::Device {
        self.status.lock().unwrap().clone()
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.send(Control::Quit);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Fields drop in order: closing `tokens` and stopping the worker unblock the feed
/// before it is joined.
struct Session {
    tokens: File,
    _worker: Option<Worker>,
    _feed: Feed,
    frames: Arc<SharedFrames>,
    free: Sender<usize>,
    /// With mapped loopback buffers, the slot on screen (`Loopback::shown`) stays out of
    /// circulation until a newer frame is queued, so the feed and worker never write it.
    hold_shown: bool,
    live: bool,
}

struct Feed {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Feed {
    fn start(
        mut capture: Capture,
        mut decoder: Decoder,
        frames: Arc<SharedFrames>,
        free: Receiver<usize>,
        mut tokens: PipeWriter,
        to_worker: bool,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let mut slot = None;
            while !flag.load(Ordering::Relaxed) {
                let k = match slot.take() {
                    Some(k) => k,
                    None => match free.recv_timeout(Duration::from_millis(100)) {
                        Ok(k) => k,
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => break,
                    },
                };
                let sent = capture.newest(Duration::from_millis(100), |frame| -> Result<bool> {
                    // SAFETY: slot k came back as free, so nothing else uses it until
                    // its number is sent on below.
                    let filled = if to_worker {
                        decoder.fill_for_worker(frame, unsafe { frames.input(k) })?
                    } else {
                        decoder.fill_for_loopback(frame, unsafe { frames.output(k) })?
                    };
                    if filled {
                        tokens.write_all(&[u8::try_from(k)?])?;
                    }
                    Ok(filled)
                });
                match sent {
                    Ok(Some(Ok(true))) => {}
                    Ok(None | Some(Ok(false))) => slot = Some(k),
                    Ok(Some(Err(e))) | Err(e) => {
                        let closed = e
                            .downcast_ref::<io::Error>()
                            .is_some_and(|e| e.kind() == ErrorKind::BrokenPipe);
                        if !closed {
                            eprintln!("camera: {e:#}");
                        }
                        break;
                    }
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct CameraLoop {
    paths: Paths,
    config: CameraConfig,
    idle_timeout: Duration,
    loopback: Loopback,
    /// The loopback format, fixed when the service starts.
    width: u32,
    height: u32,
    commands: Receiver<Control>,
    session: Option<Session>,
    in_use: bool,
    idle_since: Option<Instant>,
    retry_at: Option<Instant>,
    failures: u32,
    placeholder: Vec<u8>,
    error: Option<String>,
    /// Usually the reason when the worker exits.
    worker_line: Arc<Mutex<String>>,
    status: Arc<Mutex<status::Device>>,
    notify: Notify,
}

impl CameraLoop {
    fn new(
        paths: Paths,
        config: CameraConfig,
        idle_timeout: Duration,
        loopback: Loopback,
        commands: Receiver<Control>,
    ) -> Self {
        let (width, height) = (config.width, config.height);
        let frame = width as usize * height as usize * 2;
        Self {
            paths,
            config,
            idle_timeout,
            loopback,
            width,
            height,
            commands,
            session: None,
            in_use: false,
            idle_since: None,
            retry_at: None,
            failures: 0,
            placeholder: [16u8, 128].repeat(frame / 2),
            error: None,
            worker_line: Arc::default(),
            status: Arc::default(),
            notify: Box::new(|| {}),
        }
    }

    fn run(mut self) {
        eprintln!("camera: {} available", self.config.device);
        let mut next_placeholder = Instant::now();
        loop {
            match self.commands.try_recv() {
                Ok(Control::Quit) | Err(TryRecvError::Disconnected) => break,
                Ok(Control::Reload {
                    config,
                    idle_timeout,
                }) => self.reload(config, idle_timeout),
                Err(TryRecvError::Empty) => {}
            }

            let live = self.session.as_ref().is_some_and(|s| s.live);
            if !live && Instant::now() >= next_placeholder {
                // While loading, re-show the held buffer, which start() filled with the
                // placeholder.
                let _ = if self.session.as_ref().is_some_and(|s| s.hold_shown) {
                    self.loopback.queue(self.loopback.shown())
                } else {
                    // SAFETY: a session that uses mapped buffers takes the branch above, so
                    // nothing else is writing them here.
                    unsafe { self.loopback.write_frame(&self.placeholder) }
                };
                next_placeholder = Instant::now() + PLACEHOLDER_INTERVAL;
            }

            let wait = if self.session.is_some() {
                Duration::from_millis(20)
            } else {
                next_placeholder.saturating_duration_since(Instant::now())
            };
            self.poll(wait);

            if let Some(in_use) = self.loopback.take_usage() {
                self.usage_changed(in_use);
            }
            self.pump_frames();
            self.check_timers();
            self.publish();
        }
        self.stop();
    }

    fn publish(&self) {
        let state = match &self.session {
            None => State::Idle,
            Some(s) if !s.live => State::Loading,
            Some(_) => State::Running,
        };
        let device = status::Device {
            state,
            readers: usize::from(self.in_use),
            error: self.error.clone(),
        };
        let mut status = self.status.lock().unwrap();
        if *status != device {
            *status = device;
            drop(status);
            (self.notify)();
        }
    }

    fn fail(&mut self, message: String) {
        eprintln!("camera: {message}");
        self.error = Some(message);
    }

    fn poll(&self, wait: Duration) {
        let mut fds = [
            libc::pollfd {
                fd: self.loopback.fd(),
                events: libc::POLLPRI,
                revents: 0,
            },
            libc::pollfd {
                fd: self.session.as_ref().map_or(-1, |s| s.tokens.as_raw_fd()),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let timeout = i32::try_from(wait.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: `fds` is a valid array of pollfd for the duration of the call; poll
        // ignores entries with a negative fd.
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
    }

    fn usage_changed(&mut self, in_use: bool) {
        self.in_use = in_use;
        eprintln!(
            "camera: {}",
            if in_use {
                "an app is reading"
            } else {
                "no app reading"
            }
        );
        if in_use {
            self.idle_since = None;
            self.failures = 0;
            self.start();
        } else if self.session.is_some() {
            self.idle_since = Some(Instant::now());
        }
    }

    fn check_timers(&mut self) {
        if self
            .idle_since
            .is_some_and(|t| t.elapsed() >= self.idle_timeout)
        {
            self.idle_since = None;
            self.stop();
        }
        if self.retry_at.is_some_and(|t| Instant::now() >= t) {
            self.retry_at = None;
            if self.in_use {
                self.start();
            }
        }
    }

    fn reload(&mut self, config: CameraConfig, idle_timeout: Duration) {
        self.idle_timeout = idle_timeout;
        if config.device != self.config.device {
            eprintln!("camera: the loopback device only changes after the service restarts");
        }
        if (config.width, config.height) != (self.width, self.height) {
            eprintln!("camera: the resolution only changes after the service restarts");
        }
        if config == self.config {
            return;
        }
        self.config = config;
        // New settings get fresh retries, even after the last ones gave up.
        self.failures = 0;
        self.retry_at = None;
        if self.session.is_some() || self.in_use {
            self.stop();
            self.start();
        }
    }

    fn start(&mut self) {
        if self.session.is_some() {
            return;
        }
        // The session holds the buffer shown now, so it must not be a stale effect frame.
        // SAFETY: no session runs, so nothing else writes the loopback's buffers.
        let _ = unsafe { self.loopback.write_frame(&self.placeholder) };
        match self.start_session() {
            Ok(session) => {
                eprintln!(
                    "camera: started {} on {}",
                    effects_name(&self.config),
                    self.config.input
                );
                self.session = Some(session);
            }
            Err(e) => {
                self.fail(format!("could not start: {e:#}"));
                self.schedule_retry();
            }
        }
    }

    fn stop(&mut self) {
        if self.session.take().is_some() {
            eprintln!("camera: stopped");
        }
    }

    fn schedule_retry(&mut self) {
        self.failures += 1;
        if self.failures > 3 {
            let reason = self.error.take().unwrap_or_default();
            self.fail(format!("{reason}; giving up until the next use"));
            return;
        }
        self.retry_at = Some(Instant::now() + Duration::from_secs(1 << self.failures));
    }

    fn start_session(&self) -> Result<Session> {
        let input = &self.config.input;
        let format = webcam::choose(input, self.config.input_format, self.width, self.height)?;
        eprintln!("camera: capturing {input} as {}", format.label());
        let mut capture = Capture::open(input, format, self.width, self.height, self.config.fps)?;
        // MJPEG chroma subsampling is only known from a frame.
        let deadline = Instant::now() + Duration::from_secs(5);
        let decoder = loop {
            if let Some(decoder) = capture.newest(Duration::from_millis(200), |frame| {
                Decoder::new(
                    format,
                    frame,
                    self.width,
                    self.height,
                    self.config.fps,
                    self.config.parallel_decode,
                )
            })? {
                break decoder?;
            }
            if Instant::now() >= deadline {
                bail!("{input} sent no frames");
            }
        };
        let effects = self.config.has_effects();
        let in_bytes = if effects { decoder.worker_bytes() } else { 0 };
        let frames = Arc::new(SharedFrames::new(
            in_bytes,
            self.placeholder.len(),
            self.loopback.buffers(),
        )?);
        let (tokens_in, tokens_out) = io::pipe()?;
        let (worker, tokens) = if effects {
            let (worker, stdout) =
                self.spawn_worker(Stdio::from(tokens_in), decoder.layout(), &frames)?;
            (Some(worker), File::from(OwnedFd::from(stdout)))
        } else {
            (None, File::from(OwnedFd::from(tokens_in)))
        };
        set_nonblocking(tokens.as_raw_fd())?;
        let hold_shown = frames.loopback().is_some();
        let (free, free_slots) = mpsc::channel();
        for k in (0..frames.slots()).filter(|&k| !hold_shown || k != self.loopback.shown()) {
            let _ = free.send(k);
        }
        let feed = Feed::start(
            capture,
            decoder,
            Arc::clone(&frames),
            free_slots,
            tokens_out,
            effects,
        );
        Ok(Session {
            tokens,
            _worker: worker,
            _feed: feed,
            frames,
            free,
            hold_shown,
            live: false,
        })
    }

    fn spawn_worker(
        &self,
        tokens: Stdio,
        layout: Layout,
        frames: &SharedFrames,
    ) -> Result<(Worker, ChildStdout)> {
        let install = Installation::find(&self.paths)?;
        let models = install.model_dir(nvidia::BACKGROUND_MODELS)?;
        let mut args = vec![
            nvidia::windows_path(&models),
            "--size".into(),
            self.size(),
            "--input".into(),
            layout.worker_name().into(),
            "--shm".into(),
            frames.path().context("no shared frames for the worker")?,
        ];
        if let Some(buffers) = frames.loopback() {
            let offsets: Vec<String> = buffers.offsets().iter().map(u32::to_string).collect();
            args.extend([
                "--out-device".into(),
                buffers.path().into(),
                "--out-offsets".into(),
                offsets.join(","),
            ]);
        }
        if self.config.video_noise_removal.enabled {
            let models = install.model_dir(nvidia::VIDEO_DENOISE_MODELS)?;
            args.extend(["--denoise".into(), nvidia::windows_path(&models)]);
        }
        if self.config.eye_contact.enabled {
            let models = install.model_dir(nvidia::EYE_CONTACT_MODELS)?;
            args.extend(["--eye-contact".into(), nvidia::windows_path(&models)]);
        }
        if self.config.auto_frame.enabled {
            let models = install.model_dir(nvidia::FACE_DETECTION_MODELS)?;
            args.extend(["--auto-frame".into(), nvidia::windows_path(&models)]);
        }
        if let Some(background) = &self.config.background {
            let cache = prepare_background(&self.paths, background, self.width, self.height)?;
            args.extend(["--background".into(), cache.display().to_string()]);
        }
        if self.config.background_blur.enabled {
            args.extend([
                "--blur".into(),
                self.config.background_blur.strength.to_string(),
            ]);
        }
        if self.config.background_removal.enabled {
            args.push("--remove-background".into());
        }
        let light = self.config.studio_light;
        if light.enabled {
            let models = install.model_dir(nvidia::STUDIO_LIGHT_MODELS)?;
            args.extend([
                "--relight".into(),
                nvidia::windows_path(&models),
                // Read with winelib fopen, which takes Unix paths.
                "--hdr".into(),
                install
                    .studio_light(light.preset.file())?
                    .display()
                    .to_string(),
                "--strength".into(),
                light.strength.to_string(),
            ]);
        }

        let (worker, pipes) = Worker::spawn(Launch {
            program: "camera_stream",
            prefix: self.paths.prefix(),
            cwd: install.runtime,
            args,
            stdin: Some(tokens),
        })?;
        let last = Arc::clone(&self.worker_line);
        last.lock().unwrap().clear();
        thread::spawn(move || {
            for line in BufReader::new(pipes.stderr).lines().map_while(Result::ok) {
                eprintln!("camera worker: {line}");
                line.clone_into(&mut last.lock().unwrap());
            }
        });
        Ok((worker, pipes.stdout))
    }

    fn size(&self) -> String {
        format!("{}x{}", self.width, self.height)
    }

    fn pump_frames(&mut self) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let mut tokens = [0u8; 16];
        loop {
            match session.tokens.read(&mut tokens) {
                Ok(0) => {
                    let line = self.worker_line.lock().unwrap().clone();
                    self.fail(if line.is_empty() {
                        "the worker exited".into()
                    } else {
                        format!("the worker exited: {line}")
                    });
                    self.stop();
                    if self.in_use {
                        self.schedule_retry();
                    }
                    return;
                }
                Ok(n) => {
                    let slots = &tokens[..n];
                    let held = session.hold_shown.then(|| self.loopback.shown());
                    if slots.iter().any(|&k| {
                        usize::from(k) >= session.frames.slots() || Some(usize::from(k)) == held
                    }) {
                        eprintln!("camera: the worker sent a bad frame slot");
                        self.stop();
                        return;
                    }
                    // Show only the newest finished frame and hand older ones straight back.
                    let (&newest, older) = slots.split_last().unwrap_or((&0, &[]));
                    let newest = usize::from(newest);
                    for &k in older {
                        let _ = session.free.send(usize::from(k));
                    }
                    if !session.live {
                        session.live = true;
                        self.failures = 0;
                        self.error = None;
                        eprintln!("camera: effect running");
                    }
                    if let Some(shown) = held {
                        let freed = if self.loopback.queue(newest).is_ok() {
                            shown
                        } else {
                            newest
                        };
                        let _ = session.free.send(freed);
                    } else {
                        // SAFETY: the token gives this thread slot `newest` until it is sent
                        // back, and without mapped buffers write_frame() only copies.
                        let _ = unsafe { self.loopback.write_frame(session.frames.output(newest)) };
                        let _ = session.free.send(newest);
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    eprintln!("camera: reading the worker failed: {e}");
                    self.stop();
                    return;
                }
            }
        }
    }
}

fn effects_name(config: &CameraConfig) -> String {
    let mut names = Vec::new();
    if config.video_noise_removal.enabled {
        names.push("noise removal");
    }
    if config.eye_contact.enabled {
        names.push("Eye Contact");
    }
    if config.auto_frame.enabled {
        names.push("Auto Frame");
    }
    if config.background.is_some() {
        names.push("background replacement");
    }
    if config.background_blur.enabled {
        names.push("background blur");
    }
    if config.background_removal.enabled {
        names.push("background removal");
    }
    if config.studio_light.enabled {
        names.push("Studio Light");
    }
    if names.is_empty() {
        "passthrough (no effects enabled)".into()
    } else {
        names.join(" + ")
    }
}

fn prepare_background(paths: &Paths, image: &str, width: u32, height: u32) -> Result<PathBuf> {
    let source = expand_home(image);
    let meta = fs::metadata(&source)
        .with_context(|| format!("background image {} not found", source.display()))?;
    // Keyed by the source's identity, so picking another image never reuses this one.
    let key = format!(
        "{}\0{:?}\0{}",
        fs::canonicalize(&source)
            .unwrap_or_else(|_| source.clone())
            .display(),
        meta.modified().ok(),
        meta.len()
    );
    let hash = hex::encode(&Sha256::digest(key.as_bytes())[..8]);
    let dir = paths.data.join("camera");
    let cache = dir.join(format!("background_{width}x{height}_{hash}.bgr"));
    if cache.exists() {
        return Ok(cache);
    }
    fs::create_dir_all(&dir)?;
    for old in fs::read_dir(&dir)?.flatten() {
        let name = old.file_name();
        if name.to_string_lossy().starts_with("background_") {
            let _ = fs::remove_file(old.path());
        }
    }
    let read = || -> image::ImageResult<DynamicImage> {
        let mut decoder = ImageReader::open(&source)?
            .with_guessed_format()?
            .into_decoder()?;
        let orientation = decoder.orientation()?;
        let mut image = DynamicImage::from_decoder(decoder)?;
        image.apply_orientation(orientation);
        Ok(image)
    };
    let image = read().with_context(|| format!("reading {}", source.display()))?;
    let rgb = image
        .resize_to_fill(width, height, FilterType::CatmullRom)
        .into_rgb8();
    let bgr: Vec<u8> = rgb.pixels().flat_map(|p| [p[2], p[1], p[0]]).collect();
    fs::write(&cache, bgr)?;
    Ok(cache)
}

pub fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

fn set_nonblocking(fd: i32) -> Result<()> {
    // SAFETY: fcntl on a file descriptor owned by the caller.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        anyhow::bail!(
            "making the worker output non-blocking: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;

    #[test]
    fn background_cache_follows_the_chosen_image() {
        let dir = std::env::temp_dir().join(format!("broadcast-linux-bg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let paths = Paths {
            data: dir.join("data"),
            config: dir.join("config.toml"),
        };
        let image = |name: &str, rgb: [u8; 3], age: u64| {
            let path = dir.join(name);
            image::RgbImage::from_pixel(4, 4, image::Rgb(rgb))
                .save(&path)
                .unwrap();
            let when = SystemTime::now() - Duration::from_secs(age);
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(when)
                .unwrap();
            path.display().to_string()
        };
        let red = image("red.png", [255, 0, 0], 10);
        let blue = image("blue.png", [0, 0, 255], 1000);

        let first = prepare_background(&paths, &red, 2, 2).unwrap();
        assert_eq!(&fs::read(&first).unwrap()[..3], [0, 0, 255], "BGR of red");
        let second = prepare_background(&paths, &blue, 2, 2).unwrap();
        assert_eq!(&fs::read(&second).unwrap()[..3], [255, 0, 0], "BGR of blue");
        assert!(!first.exists(), "the old cache is removed");
        assert_eq!(prepare_background(&paths, &blue, 2, 2).unwrap(), second);
        fs::remove_dir_all(&dir).unwrap();
    }
}
