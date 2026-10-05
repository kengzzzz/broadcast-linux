use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::CameraConfig;
use crate::nvidia::{self, Installation};
use crate::paths::Paths;
use crate::v4l2::{self, Loopback};
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
}

impl Camera {
    /// Waits for the first placeholder frame, so the device is probed as a camera.
    pub fn start(paths: Paths, config: CameraConfig, idle_timeout: Duration) -> Result<Self> {
        let (commands, receiver) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            let loopback = match Loopback::open(&config.device, config.width, config.height) {
                Ok(loopback) => loopback,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let mut camera = CameraLoop::new(paths, config, idle_timeout, loopback, receiver);
            if let Err(e) = camera.loopback.write_frame(&camera.placeholder) {
                eprintln!("camera: writing the first placeholder frame: {e}");
            }
            let _ = ready_tx.send(Ok(()));
            camera.run();
        });
        ready_rx.recv().context("camera thread ended early")??;
        Ok(Self {
            commands,
            thread: Some(thread),
        })
    }

    pub fn send(&self, command: Control) {
        let _ = self.commands.send(command);
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

struct Session {
    _worker: Option<Worker>,
    capture: Child,
    stdout: ChildStdout,
    frame: Vec<u8>,
    filled: usize,
    live: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.capture.kill();
        let _ = self.capture.wait();
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
    yuyv: Vec<u8>,
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
        let yuyv_frame = width as usize * height as usize * 2;
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
            placeholder: [16u8, 128].repeat(yuyv_frame / 2),
            yuyv: vec![0; yuyv_frame],
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
                let _ = self.loopback.write_frame(&self.placeholder);
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
        }
        self.stop();
    }

    fn poll(&self, wait: Duration) {
        let mut fds = vec![libc::pollfd {
            fd: self.loopback.fd(),
            events: libc::POLLPRI,
            revents: 0,
        }];
        if let Some(session) = &self.session {
            fds.push(libc::pollfd {
                fd: session.stdout.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        let timeout = i32::try_from(wait.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: `fds` is a valid array of pollfd for the duration of the call.
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
        let changed = config != self.config;
        self.config = config;
        if changed && self.session.is_some() {
            self.stop();
            self.start();
        }
    }

    fn start(&mut self) {
        if self.session.is_some() {
            return;
        }
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
                eprintln!("camera: could not start: {e:#}");
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
            eprintln!("camera: giving up until the next use");
            return;
        }
        self.retry_at = Some(Instant::now() + Duration::from_secs(1 << self.failures));
    }

    fn start_session(&self) -> Result<Session> {
        let mut capture = Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-loglevel", "error"])
            .args(["-f", "v4l2", "-input_format", "mjpeg"])
            .args(["-framerate", &self.config.fps.to_string()])
            .args(["-video_size", &self.size(), "-i", &self.config.input])
            .args(["-pix_fmt", "bgr24", "-f", "rawvideo", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .context("starting ffmpeg for the webcam")?;
        let frames = capture.stdout.take().context("ffmpeg stdout")?;
        let (worker, stdout) = if self.config.has_effects() {
            let (worker, stdout) = self.spawn_worker(frames)?;
            (Some(worker), stdout)
        } else {
            (None, frames)
        };
        set_nonblocking(stdout.as_raw_fd())?;
        Ok(Session {
            _worker: worker,
            capture,
            stdout,
            frame: vec![0; self.width as usize * self.height as usize * 3],
            filled: 0,
            live: false,
        })
    }

    fn spawn_worker(&self, frames: ChildStdout) -> Result<(Worker, ChildStdout)> {
        let install = Installation::find(&self.paths)?;
        let models = install.model_dir("nvbcast_vfx_gs_v0_9")?;
        let mut args = vec![nvidia::windows_path(&models), "--size".into(), self.size()];
        if self.config.video_noise_removal.enabled {
            let models = install.model_dir("nvbcast_vfx_lld_v0_9")?;
            args.extend(["--denoise".into(), nvidia::windows_path(&models)]);
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
            let models = install.model_dir("nvbcast_vfx_rl_v0_9")?;
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
            stdin: Some(Stdio::from(frames)),
        })?;
        thread::spawn(move || {
            for line in BufReader::new(pipes.stderr).lines().map_while(Result::ok) {
                eprintln!("camera worker: {line}");
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
        loop {
            match session.stdout.read(&mut session.frame[session.filled..]) {
                Ok(0) => {
                    eprintln!("camera: worker exited");
                    self.stop();
                    if self.in_use {
                        self.schedule_retry();
                    }
                    return;
                }
                Ok(n) => {
                    session.filled += n;
                    if session.filled == session.frame.len() {
                        session.filled = 0;
                        if !session.live {
                            session.live = true;
                            self.failures = 0;
                            eprintln!("camera: effect running");
                        }
                        v4l2::bgr_to_yuyv(&session.frame, &mut self.yuyv);
                        let _ = self.loopback.write_frame(&self.yuyv);
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
    let cache = paths
        .data
        .join("camera")
        .join(format!("background_{width}x{height}.bgr"));
    let modified = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
    let stale = match (modified(&source), modified(&cache)) {
        (Some(src), Some(cached)) => src > cached,
        (Some(_), None) => true,
        (None, _) => anyhow::bail!("background image {} not found", source.display()),
    };
    if stale {
        fs::create_dir_all(cache.parent().unwrap_or(&paths.data))?;
        let status = Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&source)
            .args(["-vf", &format!("scale={width}:{height}:force_original_aspect_ratio=increase,crop={width}:{height}")])
            .args(["-frames:v", "1", "-pix_fmt", "bgr24", "-f", "rawvideo"])
            .arg(&cache)
            .status()
            .context("running ffmpeg to scale the background")?;
        if !status.success() {
            anyhow::bail!("ffmpeg could not read {}", source.display());
        }
    }
    Ok(cache)
}

fn expand_home(path: &str) -> PathBuf {
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
