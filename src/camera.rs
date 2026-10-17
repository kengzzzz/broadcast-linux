use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, ErrorKind, PipeWriter, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
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

use crate::config::{CameraConfig, InputColor};
use crate::frames::{Decoder, SharedFrames};
use crate::gpu;
use crate::nvidia::{self, Feature, Installation};
use crate::paths::Paths;
use crate::status::{self, State};
use crate::v4l2::{InUse, Loopback};
use crate::webcam::{self, Capture, Color, Format};
use crate::worker::{self, Launch, Worker};

/// Placeholder rate while no effect is running; enough to keep the device listed.
const PLACEHOLDER_INTERVAL: Duration = Duration::from_millis(200);
const SESSION_POLL: Duration = Duration::from_millis(20);

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
            let mut waiting = Waiting {
                config,
                idle_timeout,
                commands: &receiver,
                status: &shared,
                notify: &notify,
                ready: Some(ready_tx),
                logged: None,
                retry: Duration::from_secs(1),
            };
            let Some(loopback) = waiting.open() else {
                return;
            };
            let Waiting {
                config,
                idle_timeout,
                ready,
                ..
            } = waiting;
            let mut camera = CameraLoop::new(paths, config, idle_timeout, loopback, receiver);
            camera.status = shared;
            camera.notify = notify;
            camera.set_fps();
            // SAFETY: no session runs yet, so nothing else writes the loopback's buffers.
            if let Err(e) = unsafe { camera.loopback.write_frame(&camera.placeholder) } {
                eprintln!("camera: writing the first placeholder frame: {e}");
            }
            if let Some(ready) = ready {
                let _ = ready.send(Ok(()));
            }
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

/// The camera thread until the loopback opens at the configured size.
struct Waiting<'a> {
    config: CameraConfig,
    idle_timeout: Duration,
    commands: &'a Receiver<Control>,
    status: &'a Mutex<status::Device>,
    notify: &'a Notify,
    ready: Option<Sender<Result<()>>>,
    /// The held and the configured size last logged.
    logged: Option<((u32, u32), (u32, u32))>,
    /// An app may keep the device, and its size, without reading, so retries back off.
    retry: Duration,
}

impl Waiting<'_> {
    /// `None` on `Quit`, or on an error, which goes to `ready` or the status.
    fn open(&mut self) -> Option<Loopback> {
        let error = loop {
            let config = &self.config;
            match Loopback::open(&config.device, config.width, config.height) {
                Ok(loopback) => return Some(loopback),
                Err(e) => match e
                    .downcast_ref::<InUse>()
                    .map(|held| (held.width, held.height))
                {
                    Some(held) => match self.hold(held) {
                        Ok(true) => {}
                        Ok(false) => return None,
                        Err(e) => break e,
                    },
                    None => break e,
                },
            }
        };
        if let Some(ready) = self.ready.take() {
            let _ = ready.send(Err(error));
        } else {
            eprintln!("camera: disabled: {error:#}");
            let error = Some(format!("{error:#}"));
            set_status(self.status, self.notify, State::Unavailable, 0, error);
        }
        None
    }

    /// Sends placeholder frames at `held`, so apps holding that size don't stall. True once
    /// they close it or the config asks for `held`, false on `Quit`.
    fn hold(&mut self, (width, height): (u32, u32)) -> Result<bool> {
        let mut loopback = Loopback::open(&self.config.device, width, height)?;
        let frame = placeholder(width, height);
        // SAFETY: no session runs, so nothing else writes the loopback's buffers.
        unsafe { loopback.write_frame(&frame) }?;
        if let Some(ready) = self.ready.take() {
            let _ = ready.send(Ok(()));
        }
        let retry_at = Instant::now() + self.retry;
        self.retry = (self.retry * 2).min(Duration::from_secs(30));
        let mut in_use = true;
        loop {
            match self.commands.try_recv() {
                Ok(Control::Quit) | Err(TryRecvError::Disconnected) => return Ok(false),
                Ok(Control::Reload {
                    config,
                    idle_timeout,
                }) => {
                    self.config = config;
                    self.idle_timeout = idle_timeout;
                }
                Err(TryRecvError::Empty) => {}
            }
            let (w, h) = (self.config.width, self.config.height);
            if (w, h) == (width, height) || (!in_use && Instant::now() >= retry_at) {
                return Ok(true);
            }
            if self.logged != Some(((width, height), (w, h))) {
                self.logged = Some(((width, height), (w, h)));
                eprintln!(
                    "camera: {} is in use at {width}x{height}; it switches to {w}x{h} once the apps using it close it",
                    self.config.device
                );
            }
            set_status(
                self.status,
                self.notify,
                State::Unavailable,
                usize::from(in_use),
                Some(format!(
                    "an app is using it at {width} × {height}. It switches to {w} × {h} once the app closes the camera."
                )),
            );
            // SAFETY: as above.
            let _ = unsafe { loopback.write_frame(&frame) };
            let mut fd = libc::pollfd {
                fd: loopback.fd(),
                events: libc::POLLPRI,
                revents: 0,
            };
            let timeout = i32::try_from(PLACEHOLDER_INTERVAL.as_millis()).unwrap_or(i32::MAX);
            // SAFETY: one valid pollfd for the duration of the call.
            unsafe { libc::poll(&raw mut fd, 1, timeout) };
            if let Some(usage) = loopback.take_usage() {
                in_use = usage;
            }
        }
    }
}

fn set_status(
    status: &Mutex<status::Device>,
    notify: &Notify,
    state: State,
    readers: usize,
    error: Option<String>,
) {
    let device = status::Device {
        state,
        readers,
        error,
    };
    let mut status = status.lock().unwrap();
    if *status != device {
        *status = device;
        drop(status);
        notify();
    }
}

/// Black, in YUYV.
fn placeholder(width: u32, height: u32) -> Vec<u8> {
    [16u8, 128].repeat(width as usize * height as usize)
}

/// Fields drop in order: closing `tokens` and stopping the worker unblock the feed
/// before it is joined.
struct Session {
    tokens: File,
    _worker: Option<Worker>,
    _feed: Feed,
    frames: Arc<SharedFrames>,
    free: Sender<Freed>,
    /// With mapped loopback buffers, the slot on screen (`Loopback::shown`) stays out of
    /// circulation until a newer frame is queued, so the feed and worker never write it.
    hold_shown: bool,
    spread: Spread,
    live: bool,
}

struct Freed {
    slot: usize,
    /// When the worker finished a frame; `None` for a session's first slots.
    done: Option<Instant>,
}

/// When the worker is slower than the webcam, times each decode to end just before the
/// worker is free, so the frame doesn't wait a whole run in line.
#[derive(Default)]
pub struct Pace {
    /// Send times of frames still with the worker, oldest first.
    sent: VecDeque<Instant>,
    last_done: Option<Instant>,
    runs: Recent,
    decodes: Recent,
}

impl Pace {
    /// For jitter in decode and run times.
    const MARGIN: Duration = Duration::from_millis(3);

    pub fn sent(&mut self, at: Instant, decode: Duration) {
        self.sent.push_back(at);
        self.decodes.push(decode);
    }

    pub fn done(&mut self, at: Instant) {
        let Some(sent) = self.sent.pop_front() else {
            return;
        };
        // The first run also loads the models, so it isn't counted.
        if let Some(last) = self.last_done.replace(at) {
            self.runs.push(at.saturating_duration_since(sent.max(last)));
        }
    }

    /// When to start decoding the next frame; `None` for right away.
    pub fn next_decode(&self) -> Option<Instant> {
        let &sent = self.sent.front()?;
        let started = self.last_done.map_or(sent, |last| sent.max(last));
        let queued = u32::try_from(self.sent.len()).ok()?;
        let lead = self.decodes.median()? + Self::MARGIN;
        (started + self.runs.median()? * queued).checked_sub(lead)
    }
}

/// Recent durations. Their median ignores a one-off stall.
#[derive(Default)]
struct Recent(VecDeque<Duration>);

impl Recent {
    const LEN: usize = 9;

    fn push(&mut self, value: Duration) {
        if self.0.len() == Self::LEN {
            self.0.pop_front();
        }
        self.0.push_back(value);
    }

    fn median(&self) -> Option<Duration> {
        let mut sorted: Vec<Duration> = self.0.iter().copied().collect();
        sorted.sort_unstable();
        sorted.get(sorted.len() / 2).copied()
    }
}

/// Spreads each slot's frames evenly until the next slot is due. Frame Generation puts
/// several frames in a slot.
struct Spread {
    frames: usize,
    /// The webcam's frame interval, until the worker's own is measured.
    nominal: Duration,
    intervals: Recent,
    last: Option<Instant>,
    arrivals: usize,
    held: Option<Held>,
}

struct Held {
    slot: usize,
    arrived: Instant,
    step: Duration,
    next: usize,
}

impl Spread {
    fn new(frames: usize, nominal: Duration) -> Self {
        Self {
            frames: frames.max(1),
            nominal,
            intervals: Recent::default(),
            last: None,
            arrivals: 0,
            held: None,
        }
    }

    /// Returns the slot this one replaces, with its arrival.
    fn arrived(&mut self, slot: usize, at: Instant) -> Option<(usize, Instant)> {
        // The second slot was queued while the models loaded, so its gap isn't counted.
        if let Some(last) = self.last.replace(at)
            && self.arrivals >= 2
        {
            self.intervals.push(at.saturating_duration_since(last));
        }
        self.arrivals += 1;
        let interval = self.intervals.median().unwrap_or(self.nominal);
        let step = interval / u32::try_from(self.frames).unwrap_or(1);
        let replaced = self.held.replace(Held {
            slot,
            arrived: at,
            step,
            next: 0,
        });
        replaced.map(|held| (held.slot, held.arrived))
    }

    /// The newest frame due by `now`, as (slot, index). Overdue frames are skipped.
    fn due(&mut self, now: Instant) -> Option<(usize, usize)> {
        let held = self.held.as_mut()?;
        let mut due = None;
        while held.next < self.frames && held.at(held.next) <= now {
            due = Some((held.slot, held.next));
            held.next += 1;
        }
        due
    }

    /// The held slot and its arrival, once its last frame is shown.
    fn release(&mut self) -> Option<(usize, Instant)> {
        let held = self.held.take_if(|held| held.next >= self.frames)?;
        Some((held.slot, held.arrived))
    }

    fn next_at(&self) -> Option<Instant> {
        let held = self.held.as_ref()?;
        (held.next < self.frames).then(|| held.at(held.next))
    }
}

impl Held {
    fn at(&self, frame: usize) -> Instant {
        self.arrived + self.step * u32::try_from(frame).unwrap_or(u32::MAX)
    }
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
        free: Receiver<Freed>,
        mut tokens: PipeWriter,
        to_worker: bool,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let mut pace = Pace::default();
            let mut slots = VecDeque::new();
            while !flag.load(Ordering::Relaxed) {
                let now = Instant::now();
                let wake = pace.next_decode().filter(|&at| at > now);
                if slots.is_empty() || wake.is_some() {
                    let wait = wake.map_or(Duration::from_millis(100), |at| {
                        (at - now).min(Duration::from_millis(100))
                    });
                    match free.recv_timeout(wait) {
                        Ok(freed) => {
                            if let Some(done) = freed.done {
                                pace.done(done);
                            }
                            slots.push_back(freed.slot);
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                    continue;
                }
                let k = slots[0];
                let mut took = Duration::ZERO;
                let sent = capture.newest(Duration::from_millis(100), |frame| -> Result<bool> {
                    let start = Instant::now();
                    // SAFETY: slot k came back as free, so nothing else uses it until
                    // its number is sent on below.
                    let filled = if to_worker {
                        decoder.fill_for_worker(frame, unsafe { frames.input(k) })?
                    } else {
                        decoder.fill_for_loopback(frame, unsafe { frames.output(k) })?
                    };
                    took = start.elapsed();
                    if filled {
                        tokens.write_all(&[u8::try_from(k)?])?;
                    }
                    Ok(filled)
                });
                match sent {
                    Ok(Some(Ok(true))) => {
                        slots.pop_front();
                        pace.sent(Instant::now(), took);
                    }
                    Ok(None | Some(Ok(false))) => {}
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
            placeholder: placeholder(width, height),
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

            let now = Instant::now();
            let wait = match &self.session {
                Some(session) => session.spread.next_at().map_or(SESSION_POLL, |at| {
                    at.saturating_duration_since(now).min(SESSION_POLL)
                }),
                None => next_placeholder.saturating_duration_since(now),
            };
            self.poll(wait);

            if let Some(in_use) = self.loopback.take_usage() {
                self.usage_changed(in_use);
            }
            self.pump_frames();
            self.show_due();
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
        set_status(
            &self.status,
            &self.notify,
            state,
            usize::from(self.in_use),
            self.error.clone(),
        );
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
        // Rounded up, so a paced frame is never polled for in a busy loop.
        let timeout = i32::try_from(wait.as_micros().div_ceil(1000)).unwrap_or(i32::MAX);
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
        let fps_changed = config.fps != self.config.fps;
        self.config = config;
        if fps_changed {
            self.set_fps();
        }
        // New settings get fresh retries, even after the last ones gave up.
        self.failures = 0;
        self.retry_at = None;
        if self.session.is_some() || self.in_use {
            self.stop();
            self.start();
        }
    }

    fn set_fps(&self) {
        let (device, fps) = (&self.config.device, self.config.fps);
        if let Err(e) = self.loopback.set_fps(fps) {
            eprintln!("camera: could not set {device} to {fps} fps: {e}");
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
        let (width, height) = if self.config.super_resolution.enabled {
            webcam::smaller_size(input, self.config.input_format, self.width, self.height)?
        } else {
            (self.width, self.height)
        };
        let fps = self.config.capture_fps();
        let format = webcam::choose(input, self.config.input_format, width, height)?;
        let mut capture = Capture::open(input, format, width, height, fps)?;
        let color = Color::resolve(format, self.config.input_color, capture.reported_color());
        eprintln!(
            "camera: capturing {input} at {width}x{height} {fps} fps as {}, {color}",
            format.label()
        );
        if format == Format::Mjpeg && self.config.input_color != InputColor::Auto {
            eprintln!("camera: input_color only applies to YUYV and NV12, so MJPEG ignores it");
        }
        // MJPEG chroma subsampling is only known from a frame.
        let deadline = Instant::now() + Duration::from_secs(5);
        let decoder = loop {
            if let Some(decoder) = capture.newest(Duration::from_millis(200), |frame| {
                Decoder::new(
                    format,
                    color,
                    frame,
                    width,
                    height,
                    fps,
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
        let multiplier = self.config.frame_multiplier() as usize;
        // Made frames have to be paced, so they are copied into the loopback.
        let buffers = if multiplier > 1 {
            None
        } else {
            self.loopback.buffers()
        };
        let frames = Arc::new(SharedFrames::new(
            in_bytes,
            self.placeholder.len() * multiplier,
            buffers,
        )?);
        let (tokens_in, tokens_out) = io::pipe()?;
        let (worker, tokens) = if effects {
            let (worker, stdout) =
                self.spawn_worker(Stdio::from(tokens_in), &decoder, &frames, (width, height))?;
            (Some(worker), File::from(OwnedFd::from(stdout)))
        } else {
            (None, File::from(OwnedFd::from(tokens_in)))
        };
        set_nonblocking(tokens.as_raw_fd())?;
        let hold_shown = frames.loopback().is_some();
        let (free, free_slots) = mpsc::channel();
        for k in (0..frames.slots()).filter(|&k| !hold_shown || k != self.loopback.shown()) {
            let _ = free.send(Freed {
                slot: k,
                done: None,
            });
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
            spread: Spread::new(multiplier, Duration::from_secs(1) / fps.max(1)),
            live: false,
        })
    }

    fn spawn_worker(
        &self,
        tokens: Stdio,
        decoder: &Decoder,
        frames: &SharedFrames,
        size: (u32, u32),
    ) -> Result<(Worker, ChildStdout)> {
        let install = Installation::find(&self.paths)?;
        let background = self
            .config
            .background
            .as_deref()
            .map(|image| prepare_background(&self.paths, image, size.0, size.1))
            .transpose()?;
        let mut args = effect_args(
            &self.config,
            &install,
            size,
            (self.width, self.height),
            background.as_deref(),
        )?;
        args.extend([
            "--input".into(),
            decoder.layout().worker_name().into(),
            "--color".into(),
            decoder.color().worker_name().into(),
            "--shm".into(),
            frames.path().context("no shared frames for the worker")?,
        ]);
        if let Some(buffers) = frames.loopback() {
            let offsets: Vec<String> = buffers.offsets().iter().map(u32::to_string).collect();
            args.extend([
                "--out-device".into(),
                buffers.path().into(),
                "--out-offsets".into(),
                offsets.join(","),
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
            for line in worker::log_lines(pipes.stderr) {
                eprintln!("camera worker: {line}");
                line.clone_into(&mut last.lock().unwrap());
            }
        });
        Ok((worker, pipes.stdout))
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
                    let now = Instant::now();
                    let done = Some(now);
                    let free = |slot| {
                        let _ = session.free.send(Freed { slot, done });
                    };
                    for &k in older {
                        free(usize::from(k));
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
                        free(freed);
                    } else if let Some((slot, done)) = session.spread.arrived(newest, now) {
                        let _ = session.free.send(Freed {
                            slot,
                            done: Some(done),
                        });
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

    /// Also frees slots whose frames have all been shown.
    fn show_due(&mut self) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        if let Some((slot, frame)) = session.spread.due(Instant::now()) {
            let bytes = self.placeholder.len();
            // SAFETY: the token gives this thread the slot until it is sent back. In copy mode
            // neither the feed nor the worker gets the loopback's buffers.
            let _ = unsafe {
                let output = session.frames.output(slot);
                self.loopback.write_frame(&output[frame * bytes..][..bytes])
            };
        }
        if let Some((slot, done)) = session.spread.release() {
            let _ = session.free.send(Freed {
                slot,
                done: Some(done),
            });
        }
    }
}

/// `out_size` is Super Resolution's output. `background` is the replacement image as BGR24
/// at `size`.
pub fn effect_args(
    config: &CameraConfig,
    install: &Installation,
    (width, height): (u32, u32),
    out_size: (u32, u32),
    background: Option<&Path>,
) -> Result<Vec<String>> {
    let models = install.model_dir(nvidia::BACKGROUND_MODELS)?;
    let mut args = vec![
        nvidia::windows_path(&models),
        "--size".into(),
        format!("{width}x{height}"),
    ];
    if config.video_noise_removal.enabled {
        let models = install.model_dir(nvidia::VIDEO_DENOISE_MODELS)?;
        args.extend(["--denoise".into(), nvidia::windows_path(&models)]);
    }
    if config.eye_contact.enabled {
        let models = install.model_dir(nvidia::EYE_CONTACT_MODELS)?;
        args.extend(["--eye-contact".into(), nvidia::windows_path(&models)]);
    }
    if config.auto_frame.enabled {
        let models = install.model_dir(nvidia::FACE_DETECTION_MODELS)?;
        args.extend(["--auto-frame".into(), nvidia::windows_path(&models)]);
    }
    if let Some(background) = background {
        args.extend(["--background".into(), background.display().to_string()]);
    }
    if config.background_blur.enabled {
        args.extend(["--blur".into(), config.background_blur.strength.to_string()]);
    }
    if config.background_removal.enabled {
        args.push("--remove-background".into());
    }
    let light = config.studio_light;
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
    args.extend(rtx_video_args(config, install, out_size)?);
    Ok(args)
}

fn rtx_video_args(
    config: &CameraConfig,
    install: &Installation,
    (width, height): (u32, u32),
) -> Result<Vec<String>> {
    let mut args = Vec::new();
    if config.super_resolution.enabled {
        if let Some(reason) = install.missing(Feature::SuperResolution) {
            bail!(reason);
        }
        let ngx = gpu::ngx_dir().context(gpu::NO_NGX)?;
        args.extend([
            "--super-res".into(),
            config.super_resolution.mode.quality_level().to_string(),
            "--out-size".into(),
            format!("{width}x{height}"),
            "--ngx-dir".into(),
            nvidia::windows_path(&ngx),
        ]);
    }
    let generation = config.frame_generation;
    if generation.enabled {
        if let Some(reason) = install.missing(Feature::FrameGeneration) {
            bail!(reason);
        }
        args.extend([
            "--frame-gen".into(),
            generation.multiplier.to_string(),
            "--frame-gen-mode".into(),
            generation.mode.worker_value().to_string(),
        ]);
    }
    Ok(args)
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
    if config.super_resolution.enabled {
        names.push("Super Resolution");
    }
    let generation = format!("Frame Generation {}x", config.frame_generation.multiplier);
    if config.frame_generation.enabled {
        names.push(&generation);
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
    fs::write(&cache, background_bgr(&source, width, height)?)?;
    Ok(cache)
}

/// Cropped to fill `width`x`height`, as BGR24.
pub fn background_bgr(source: &Path, width: u32, height: u32) -> Result<Vec<u8>> {
    let read = || -> image::ImageResult<DynamicImage> {
        let mut decoder = ImageReader::open(source)?
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
    Ok(rgb.pixels().flat_map(|p| [p[2], p[1], p[0]]).collect())
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
    fn decodes_just_before_a_slow_worker_is_free() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut pace = Pace::default();
        assert_eq!(pace.next_decode(), None, "nothing in flight");
        pace.sent(t0, ms(4));
        assert_eq!(pace.next_decode(), None, "no run measured yet");
        pace.done(t0 + ms(500));
        pace.sent(t0 + ms(500), ms(4));
        assert_eq!(pace.next_decode(), None, "the loading run is not counted");
        pace.done(t0 + ms(540));
        assert_eq!(pace.next_decode(), None, "the worker is idle");

        // Sent while the worker was idle, so the run starts when it was sent.
        pace.sent(t0 + ms(600), ms(4));
        assert_eq!(pace.next_decode(), Some(t0 + ms(633)));
        // A frame queued behind it pushes the decode back by a run.
        pace.sent(t0 + ms(610), ms(4));
        assert_eq!(pace.next_decode(), Some(t0 + ms(673)));
        pace.done(t0 + ms(640));
        // The queued frame started when the one before it finished.
        assert_eq!(pace.next_decode(), Some(t0 + ms(673)));
    }

    #[test]
    fn a_single_stall_does_not_move_the_estimate() {
        let ms = Duration::from_millis;
        let mut recent = Recent::default();
        for run in [40, 41, 39, 400, 40] {
            recent.push(ms(run));
        }
        assert_eq!(recent.median(), Some(ms(40)));
        for _ in 0..Recent::LEN {
            recent.push(ms(10));
        }
        assert_eq!(recent.median(), Some(ms(10)), "only recent runs count");
    }

    #[test]
    fn spreads_made_frames_until_the_next_slot() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut spread = Spread::new(2, ms(40));
        assert_eq!(spread.next_at(), None);
        assert_eq!(spread.arrived(0, t0), None);
        assert_eq!(spread.due(t0), Some((0, 0)));
        assert_eq!(spread.release(), None, "a frame is left");
        assert_eq!(
            spread.next_at(),
            Some(t0 + ms(20)),
            "half the webcam interval"
        );
        assert_eq!(spread.due(t0 + ms(19)), None);
        assert_eq!(spread.due(t0 + ms(20)), Some((0, 1)));
        assert_eq!(spread.release(), Some((0, t0)), "returned with its arrival");
        assert_eq!(spread.next_at(), None);

        // The second gap isn't measured, so the steps still follow the webcam.
        spread.arrived(1, t0 + ms(25));
        assert_eq!(spread.next_at(), Some(t0 + ms(25)));
        spread.due(t0 + ms(25));
        assert_eq!(spread.next_at(), Some(t0 + ms(45)));
        // A slot that comes early replaces one not fully shown.
        assert_eq!(spread.arrived(0, t0 + ms(85)), Some((1, t0 + ms(25))));
        spread.due(t0 + ms(85));
        assert_eq!(
            spread.next_at(),
            Some(t0 + ms(115)),
            "the measured 60 ms gap"
        );
    }

    #[test]
    fn late_frames_skip_to_the_newest() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut spread = Spread::new(4, ms(40));
        spread.arrived(0, t0);
        assert_eq!(spread.due(t0 + ms(25)), Some((0, 2)));
        assert_eq!(spread.release(), None);
        assert_eq!(spread.due(t0 + ms(30)), Some((0, 3)));
        assert_eq!(spread.release(), Some((0, t0)));

        let mut single = Spread::new(1, ms(40));
        single.arrived(1, t0);
        assert_eq!(single.due(t0), Some((1, 0)));
        assert_eq!(
            single.release(),
            Some((1, t0)),
            "one frame is released at once"
        );
    }

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
