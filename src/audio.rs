use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Producer, Split};

use crate::config::{MicConfig, SpeakerConfig, Stage};
use crate::graph;
use crate::nvidia::{self, Installation};
use crate::paths::Paths;
use crate::service::{Event, EventSender};
use crate::status::{self, State};
use crate::worker::{Launch, Worker};

const RATE: u32 = 48_000;
/// Broadcast's audio models all run on fixed 40 ms frames at 48 kHz.
const FRAME: usize = 1920;
/// Ask PipeWire for 20 ms periods; the default can be hundreds of milliseconds.
const LATENCY: &str = "960/48000";
/// Frames in the worker beyond this are dropped so latency can't build up. It also
/// keeps the worker from dropping frames itself, which would misalign output indices.
const MAX_IN_FLIGHT: u64 = 2;
/// Capacity of the frame rings.
const QUEUE: usize = 32;
/// Output off schedule by more than this holds or skips up to n / SLIP_RATE samples
/// per period until it is back on.
const SLIP: i64 = 24;
const SLIP_RATE: usize = 64;
/// Further behind than this, or ahead by more than the reserve, jumps instead.
const JUMP_BEHIND: i64 = RATE as i64 / 10;
/// Covers the worker's round trip for a frame, up to about 9 ms.
const RESERVE: usize = RATE as usize / 100;
const MAX_RESERVE: usize = 3 * RATE as usize / 50;
/// Late output during model warmup doesn't grow the reserve.
const SETTLE: usize = RATE as usize;
/// A shorter period must last this long before it is used.
const PERIOD_DECAY: usize = 10 * RATE as usize;
/// Time without late output before a grown reserve shrinks by a period.
const RESERVE_DECAY: usize = 60 * RATE as usize;

/// Field order matters: removing the listener after destroying the stream writes to
/// freed memory.
struct Stream {
    _listener: pw::stream::StreamListener<()>,
    stream: pw::stream::StreamRc,
}
type FrameProd = ringbuf::HeapProd<Frame>;
type FrameCons = ringbuf::HeapCons<Frame>;

#[derive(Clone, Copy)]
struct Frame {
    /// Input sample index of `samples[0]`.
    index: u64,
    len: usize,
    samples: [f32; FRAME],
}

impl Frame {
    const EMPTY: Self = Self {
        index: 0,
        len: 0,
        samples: [0.0; FRAME],
    };
}

#[derive(Default)]
struct Stats {
    dropped_frames: AtomicU64,
    late_samples: AtomicU64,
    underrun_samples: AtomicU64,
    resyncs: AtomicU64,
    /// Set once processed audio plays; earlier gaps are just model loading.
    flowing: AtomicBool,
}

/// The latest capture period. A seqlock: the two callbacks may run on different threads.
#[derive(Default)]
struct CaptureClock {
    seq: AtomicU64,
    /// Input indices of the period's first sample and one past its last.
    start: AtomicU64,
    end: AtomicU64,
    now: AtomicI64,
}

impl CaptureClock {
    fn publish(&self, start: u64, end: u64, now: i64) {
        self.seq.fetch_add(1, Ordering::SeqCst);
        self.start.store(start, Ordering::SeqCst);
        self.end.store(end, Ordering::SeqCst);
        self.now.store(now, Ordering::SeqCst);
        self.seq.fetch_add(1, Ordering::SeqCst);
    }

    fn read(&self) -> Option<(u64, u64, i64)> {
        for _ in 0..4 {
            let seq = self.seq.load(Ordering::SeqCst);
            let value = (
                self.start.load(Ordering::SeqCst),
                self.end.load(Ordering::SeqCst),
                self.now.load(Ordering::SeqCst),
            );
            if seq.is_multiple_of(2) && self.seq.load(Ordering::SeqCst) == seq {
                return Some(value);
            }
        }
        None
    }
}

/// Shared with the realtime callbacks, which only `try_lock`.
struct Rt {
    capture: Mutex<Capture>,
    playback: Mutex<Playback>,
    clock: CaptureClock,
    /// Cleared when capture restarts.
    clock_valid: AtomicBool,
    playback_now: AtomicI64,
    /// Last graph time both callbacks saw. While recent, the streams share a driver.
    locked_at: AtomicI64,
    /// Samples captured while the main thread held `capture`.
    lost: AtomicU64,
    /// An app is linked. Gaps only count then: PipeWire may drop a period as the last
    /// one leaves.
    linked: AtomicBool,
    stats: Stats,
}

impl Rt {
    fn new() -> Self {
        Self {
            capture: Mutex::new(Capture {
                ring: None,
                whole: false,
                active: false,
                feeder: None,
                next: 0,
                pending: Frame::EMPTY,
            }),
            playback: Mutex::new(Playback {
                ring: None,
                frame: 0,
                reserve: 0,
                base_reserve: 0,
                period: 0,
                shorter_for: 0,
                clean_for: 0,
                flowing_for: 0,
                next: None,
                used: 0,
                last: 0.0,
                off: 0,
            }),
            clock: CaptureClock::default(),
            clock_valid: AtomicBool::new(false),
            playback_now: AtomicI64::new(0),
            locked_at: AtomicI64::new(0),
            lost: AtomicU64::new(0),
            linked: AtomicBool::new(false),
            stats: Stats::default(),
        }
    }

    /// Swaps the realtime side's rings. The old ones are freed here, not in a callback.
    fn connect(
        &self,
        input: Option<FrameProd>,
        whole: bool,
        output: Option<FrameCons>,
        frame: usize,
        reserve: usize,
    ) {
        let old_input = {
            let mut capture = self.capture.lock().unwrap();
            capture.whole = whole;
            capture.active = input.is_some();
            capture.feeder = None;
            capture.pending.len = 0;
            self.clock_valid.store(false, Ordering::Relaxed);
            std::mem::replace(&mut capture.ring, input)
        };
        let old_output = {
            let mut playback = self.playback.lock().unwrap();
            playback.frame = frame;
            playback.reserve = reserve;
            playback.base_reserve = reserve;
            playback.shorter_for = 0;
            playback.clean_for = 0;
            playback.period = 0;
            playback.flowing_for = 0;
            playback.next = None;
            playback.used = 0;
            playback.off = 0;
            std::mem::replace(&mut playback.ring, output)
        };
        drop((old_input, old_output));
    }

    fn set_active(&self, active: bool) {
        let mut capture = self.capture.lock().unwrap();
        capture.active = active;
        capture.pending.len = 0;
        // Makes output queued before the pause late, so it is skipped.
        capture.next += u64::from(RATE);
        self.clock_valid.store(false, Ordering::Relaxed);
        drop(capture);
        let mut playback = self.playback.lock().unwrap();
        playback.next = None;
        playback.flowing_for = 0;
        playback.off = 0;
        drop(playback);
    }
}

struct Capture {
    ring: Option<FrameProd>,
    /// Cut whole model frames for the worker, or pass each period straight through.
    whole: bool,
    active: bool,
    feeder: Option<thread::Thread>,
    /// Input index of the next sample.
    next: u64,
    pending: Frame,
}

impl Capture {
    fn take(&mut self, samples: &[[u8; 4]], stats: &Stats) {
        for sample in samples {
            if self.pending.len == 0 {
                self.pending.index = self.next;
            }
            self.pending.samples[self.pending.len] = f32::from_le_bytes(*sample);
            self.pending.len += 1;
            self.next += 1;
            if self.pending.len == FRAME {
                self.flush(stats);
            }
        }
        if !self.whole && self.pending.len > 0 {
            self.flush(stats);
        }
    }

    fn flush(&mut self, stats: &Stats) {
        if self.active
            && let Some(ring) = self.ring.as_mut()
        {
            if ring.try_push(self.pending).is_err() {
                stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
            }
            if let Some(feeder) = &self.feeder {
                feeder.unpark();
            }
        }
        self.pending.len = 0;
    }
}

/// Plays each input sample a fixed delay after capture: a model frame plus a reserve
/// for the worker. Late output is skipped and missing output is silence, so a slow
/// frame costs a gap, not lasting delay.
struct Playback {
    ring: Option<FrameCons>,
    /// FRAME with effects, 0 for passthrough.
    frame: usize,
    /// Time the worker gets per frame. Grows by a period on late output.
    reserve: usize,
    base_reserve: usize,
    /// The longest recent period.
    period: usize,
    /// Samples played in a row at a shorter period.
    shorter_for: usize,
    /// Samples played since the last late output.
    clean_for: usize,
    /// Samples played since processed audio started.
    flowing_for: usize,
    /// Input index of the next output sample.
    next: Option<i64>,
    /// Samples of the oldest queued frame already played or skipped.
    used: usize,
    last: f32,
    /// Periods in a row off by enough to jump.
    off: u32,
}

impl Playback {
    fn fill(&mut self, out: &mut [[u8; 4]], now: i64, rt: &Rt) {
        let n = out.len();
        if n == 0 {
            return;
        }
        let valid = rt.clock_valid.load(Ordering::Acquire) && now != 0;
        let clock = rt.clock.read().filter(|_| valid);
        let (Some(ring), Some((start, end, captured))) = (self.ring.as_mut(), clock) else {
            out.fill([0; 4]);
            return;
        };
        if n >= self.period {
            self.period = n;
            self.shorter_for = 0;
        } else {
            self.shorter_for += n;
            if self.shorter_for >= PERIOD_DECAY {
                self.period = n;
                self.shorter_for = 0;
            }
        }
        let period_ns = self.period as i64 * 1_000_000_000 / i64::from(RATE);
        if captured == now {
            rt.locked_at.store(now, Ordering::Relaxed);
        }
        // Capture's input index this period. Sample counts, since graph time jumps when
        // the period or driver changes. Graph time is the fallback across drivers.
        #[expect(clippy::cast_possible_truncation, reason = "sample indices fit in i64")]
        let reference = if captured == now {
            start as i64
        } else if now - rt.locked_at.load(Ordering::Relaxed) <= 4 * period_ns {
            end as i64
        } else {
            start as i64 + ((now - captured) as f64 * f64::from(RATE) / 1e9).round() as i64
        };
        // Frames only become playable at period boundaries, so a partial period would
        // add delay but no worker time.
        let reserve = self.period * self.reserve.div_ceil(self.period).max(1);
        let target = reference - (self.frame + reserve) as i64;
        // Gaps only count while capture delivers and an app is linked.
        let live = now - captured <= 3 * period_ns && rt.linked.load(Ordering::Relaxed);
        let flowing = rt.stats.flowing.load(Ordering::Relaxed);
        let counting = flowing && live;
        let mut q = self.next.unwrap_or(target);
        let ahead = q - target;
        // Skipped on purpose, so not late.
        let mut skipped = 0;
        let mut i = 0;
        // Two periods in a row, since one can be off at a driver switch.
        if ahead > reserve as i64 || ahead < -JUMP_BEHIND {
            self.off += 1;
            if self.off > 1 {
                if counting {
                    rt.stats.resyncs.fetch_add(1, Ordering::Relaxed);
                }
                skipped = usize::try_from(-ahead).unwrap_or(0);
                q = target;
                self.off = 0;
            }
        } else {
            self.off = 0;
            let slip = (ahead.unsigned_abs() as usize).min((n / SLIP_RATE).max(1));
            if ahead > SLIP {
                out[..slip].fill(self.last.to_le_bytes());
                i = slip;
            } else if ahead < -SLIP {
                q += slip as i64;
                skipped = slip;
            }
        }
        let correcting = ahead.abs() > SLIP;
        let mut late = 0;
        let mut played = false;
        while i < n {
            let Some(frame) = ring.first() else {
                break;
            };
            let len = frame.len;
            let head = frame.index as i64 + self.used as i64;
            let left = len - self.used;
            if head + left as i64 <= q {
                late += left;
                self.used = len;
            } else if head < q {
                let k = (q - head) as usize;
                late += k;
                self.used += k;
                continue;
            } else if head > q {
                let k = ((head - q) as usize).min(n - i);
                out[i..i + k].fill([0; 4]);
                if counting {
                    rt.stats
                        .underrun_samples
                        .fetch_add(k as u64, Ordering::Relaxed);
                }
                i += k;
                q += k as i64;
                continue;
            } else {
                let k = left.min(n - i);
                for (slot, sample) in out[i..i + k]
                    .iter_mut()
                    .zip(&frame.samples[self.used..self.used + k])
                {
                    *slot = sample.to_le_bytes();
                }
                i += k;
                q += k as i64;
                self.used += k;
                played = true;
            }
            if self.used == len {
                ring.skip(1);
                self.used = 0;
            }
        }
        if i < n {
            out[i..].fill([0; 4]);
            if counting {
                rt.stats
                    .underrun_samples
                    .fetch_add((n - i) as u64, Ordering::Relaxed);
            }
            q += (n - i) as i64;
        }
        let late = late.saturating_sub(skipped);
        if counting {
            rt.stats
                .late_samples
                .fetch_add(late as u64, Ordering::Relaxed);
            if late > 0 {
                self.clean_for = 0;
                if !correcting && self.flowing_for >= SETTLE {
                    self.reserve = (reserve + 1).min(MAX_RESERVE);
                }
            } else {
                self.clean_for += n;
                if self.clean_for >= RESERVE_DECAY && self.reserve > self.base_reserve {
                    self.reserve = reserve.saturating_sub(self.period).max(self.base_reserve);
                    self.clean_for = 0;
                }
            }
            self.flowing_for += n;
        } else if played && !flowing {
            rt.stats.flowing.store(true, Ordering::Relaxed);
        }
        self.last = f32::from_le_bytes(out[n - 1]);
        self.next = Some(q);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Mic,
    Speaker,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mic => "mic",
            Self::Speaker => "speaker",
        }
    }

    pub fn node_name(self) -> &'static str {
        match self {
            Self::Mic => "broadcast_linux_mic",
            Self::Speaker => "broadcast_linux_speaker",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// The real device: "default" or a node name.
    pub target: String,
    pub stages: Vec<Stage>,
    pub unload_after: Duration,
    pub name: String,
}

impl Settings {
    pub fn mic(config: &MicConfig) -> Self {
        Self {
            target: config.input.clone(),
            stages: config.stages(),
            unload_after: Duration::from_secs(config.unload_after_minutes * 60),
            name: config.name.clone(),
        }
    }

    pub fn speaker(config: &SpeakerConfig) -> Self {
        Self {
            target: config.output.clone(),
            stages: config.stages(),
            unload_after: Duration::from_secs(config.unload_after_minutes * 60),
            name: config.name.clone(),
        }
    }
}

/// Mic: real mic -> input, output -> node. Speaker: node -> input, output -> real device.
pub struct AudioDevice {
    kind: Kind,
    core: pw::core::CoreRc,
    settings: Settings,
    events: EventSender,
    node: Stream,
    rt: Arc<Rt>,
    /// Puts the node and the session stream on one clock, and keeps them unlinked.
    group: String,
    readers: HashSet<u32>,
    session: Option<Session>,
    next_session: u64,
    failures: u32,
    /// Bumped whenever a pending idle timeout should be ignored.
    idle_token: u64,
    last_error: Option<String>,
}

struct Session {
    id: u64,
    target: String,
    /// The real device's stream; dropped while paused, which releases it.
    stream: Option<Stream>,
    worker: Option<Worker>,
    shared: Arc<Shared>,
    ready: bool,
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    active: AtomicBool,
    sent: AtomicU64,
    received: AtomicU64,
    /// Usually the reason when a worker fails to start.
    last_line: Mutex<String>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // Not joining the I/O threads: this runs in the event callback, and the stdout
        // thread sends an event on exit, so joining would deadlock. They exit on `stop`
        // or EOF, and their late events carry a stale session id.
        self.shared.stop.store(true, Ordering::Relaxed);
        drop(self.worker.take());
    }
}

impl AudioDevice {
    pub fn new(
        kind: Kind,
        core: &pw::core::CoreRc,
        settings: Settings,
        events: EventSender,
    ) -> Result<Self> {
        let rt = Arc::new(Rt::new());
        let group = format!("broadcast-linux-{}-{}", kind.label(), std::process::id());
        let media_class = match kind {
            Kind::Mic => "Audio/Source",
            Kind::Speaker => "Audio/Sink",
        };
        let props = properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CLASS => media_class,
            *pw::keys::NODE_NAME => kind.node_name(),
            *pw::keys::NODE_DESCRIPTION => settings.name.as_str(),
            *pw::keys::AUDIO_CHANNELS => "1",
            *pw::keys::NODE_LATENCY => LATENCY,
            *pw::keys::NODE_GROUP => group.as_str(),
            *pw::keys::NODE_LINK_GROUP => group.as_str(),
        };
        let name = format!("broadcast-linux-{}", kind.label());
        let flags = pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS;
        let node = match kind {
            Kind::Mic => play_stream(core, &name, props, flags, rt.clone())?,
            Kind::Speaker => record_stream(core, &name, props, flags, rt.clone())?,
        };
        Ok(Self {
            kind,
            core: core.clone(),
            settings,
            events,
            node,
            rt,
            group,
            readers: HashSet::new(),
            session: None,
            next_session: 0,
            failures: 0,
            idle_token: 0,
            last_error: None,
        })
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn node_id(&self) -> u32 {
        self.node.stream.node_id()
    }

    pub fn idle_token(&self) -> u64 {
        self.idle_token
    }

    pub fn bump_idle_token(&mut self) -> u64 {
        self.idle_token += 1;
        self.idle_token
    }

    pub fn has_session(&self) -> bool {
        self.session.is_some()
    }

    pub fn readers(&self) -> usize {
        self.readers.len()
    }

    pub fn status(&self) -> status::Device {
        let state = match &self.session {
            None => State::Idle,
            Some(s) if s.stream.is_none() => State::Paused,
            Some(s) if !s.ready => State::Loading,
            Some(_) => State::Running,
        };
        status::Device {
            state,
            readers: self.readers.len(),
            error: self.last_error.clone(),
        }
    }

    fn fail(&mut self, message: String) {
        eprintln!("{}: {message}", self.kind.label());
        self.last_error = Some(message);
    }

    pub fn link_added(&mut self, link: u32, output_node: u32, input_node: u32) -> bool {
        let node = match self.kind {
            Kind::Mic => output_node,
            Kind::Speaker => input_node,
        };
        let added = node == self.node_id() && self.readers.insert(link);
        self.rt
            .linked
            .store(!self.readers.is_empty(), Ordering::Relaxed);
        added
    }

    pub fn link_removed(&mut self, link: u32) -> bool {
        let removed = self.readers.remove(&link);
        self.rt
            .linked
            .store(!self.readers.is_empty(), Ordering::Relaxed);
        removed
    }

    pub fn set_settings(&mut self, settings: Settings) -> bool {
        let changed = settings != self.settings;
        if settings.name != self.settings.name {
            eprintln!(
                "{}: the device name only changes after the service restarts",
                self.kind.label()
            );
        }
        self.settings = settings;
        changed
    }

    pub fn unload_after(&self) -> Duration {
        self.settings.unload_after
    }

    pub fn start(&mut self, paths: &Paths) {
        if self.session.is_some() {
            self.resume();
            return;
        }
        self.next_session += 1;
        let id = self.next_session;
        match self.start_session(id, paths) {
            Ok(session) => {
                eprintln!(
                    "{}: started {} on {} (session {id})",
                    self.kind.label(),
                    chain_name(&self.settings.stages),
                    session.target
                );
                self.session = Some(session);
            }
            Err(e) => {
                self.rt.connect(None, false, None, 0, 0);
                self.fail(format!("could not start: {e:#}"));
            }
        }
    }

    /// Runs after the idle timeout. Returns true if only paused (unload comes later).
    pub fn idle(&mut self) -> bool {
        let has_model = self.session.as_ref().is_some_and(|s| s.worker.is_some());
        if has_model && !self.settings.unload_after.is_zero() {
            self.pause();
            self.session.is_some()
        } else {
            self.stop();
            false
        }
    }

    pub fn stop(&mut self) {
        if let Some(session) = self.session.take() {
            self.rt.connect(None, false, None, 0, 0);
            let was_active = session.shared.active.load(Ordering::Relaxed);
            drop(session);
            if was_active {
                self.log_stats("stopped");
            } else {
                eprintln!("{}: stopped", self.kind.label());
            }
        }
    }

    fn pause(&mut self) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        if session.stream.take().is_none() {
            return;
        }
        self.rt.set_active(false);
        session.shared.active.store(false, Ordering::Relaxed);
        self.log_stats("paused; the model stays loaded");
    }

    fn resume(&mut self) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        if session.stream.is_none() {
            self.rt.set_active(true);
            match self.session_stream(&session.target) {
                Ok(stream) => {
                    session.stream = Some(stream);
                    session.shared.active.store(true, Ordering::Relaxed);
                    eprintln!("{}: resumed", self.kind.label());
                }
                Err(e) => {
                    self.fail(format!("could not resume: {e:#}"));
                    self.session = Some(session);
                    self.stop();
                    return;
                }
            }
        }
        self.session = Some(session);
    }

    pub fn worker_ready(&mut self, session: u64) {
        if let Some(s) = self.session.as_mut().filter(|s| s.id == session) {
            s.ready = true;
            self.failures = 0;
            self.last_error = None;
            eprintln!("{}: effect running", self.kind.label());
        }
    }

    pub fn worker_exited(&mut self, session: u64) -> Option<Duration> {
        let current = self.session.as_ref().filter(|s| s.id == session)?;
        let was_ready = current.ready;
        let reason = current.shared.last_line.lock().unwrap().clone();
        self.stop();
        self.failures += 1;
        let what = if was_ready {
            "the worker crashed"
        } else {
            "the effect failed to load"
        };
        let what = if reason.is_empty() {
            what.to_owned()
        } else {
            format!("{what}: {reason}")
        };
        if !was_ready && self.failures > 2 {
            self.fail(format!("{what}; giving up until the next use"));
            self.failures = 0;
            return None;
        }
        let delay = Duration::from_secs(1 << self.failures.min(5));
        self.fail(format!("{what}; restarting in {}s", delay.as_secs()));
        Some(delay)
    }

    fn start_session(&mut self, id: u64, paths: &Paths) -> Result<Session> {
        let target = graph::resolve(self.kind, &self.settings.target)?;
        let shared = Arc::new(Shared::default());
        shared.active.store(true, Ordering::Relaxed);

        if self.settings.stages.is_empty() {
            let (prod, cons) = HeapRb::<Frame>::new(QUEUE).split();
            self.rt.connect(Some(prod), false, Some(cons), 0, 0);
            let stream = self.session_stream(&target)?;
            return Ok(Session {
                id,
                target,
                stream: Some(stream),
                worker: None,
                shared,
                ready: true,
            });
        }

        let install = Installation::find(paths)?;
        let mut args = vec!["0".to_owned()];
        for stage in &self.settings.stages {
            let (folder, file) = stage.effect.model();
            args.extend([
                stage.effect.selector().to_owned(),
                nvidia::windows_path(&install.model(folder, file)?),
                stage.strength.to_string(),
            ]);
        }
        let (in_prod, in_cons) = HeapRb::<Frame>::new(QUEUE).split();
        let (out_prod, out_cons) = HeapRb::<Frame>::new(QUEUE).split();
        self.rt
            .connect(Some(in_prod), true, Some(out_cons), FRAME, RESERVE);
        let stream = self.session_stream(&target)?;

        let (worker, pipes) = Worker::spawn(Launch {
            program: "afx_stream",
            prefix: paths.prefix(),
            cwd: install.runtime.clone(),
            args,
            stdin: None,
        })?;
        let stdin = pipes.stdin.context("worker stdin")?;

        let (frame_tx, frame_rx) = mpsc::channel::<usize>();
        let (order_tx, order_rx) = mpsc::channel::<u64>();
        let kind = self.kind;
        let events = self.events.clone();
        {
            let shared = shared.clone();
            thread::spawn(move || {
                watch_stderr(pipes.stderr, &frame_tx, &events, &shared, kind, id);
            });
        }
        {
            let (shared, rt) = (shared.clone(), self.rt.clone());
            let feeder = thread::spawn(move || {
                feed_worker(stdin, in_cons, &frame_rx, &order_tx, &shared, &rt.stats);
            });
            self.rt.capture.lock().unwrap().feeder = Some(feeder.thread().clone());
        }
        let events = self.events.clone();
        {
            let (shared, rt) = (shared.clone(), self.rt.clone());
            thread::spawn(move || {
                drain_worker(
                    pipes.stdout,
                    out_prod,
                    &order_rx,
                    &shared,
                    &rt.stats,
                    &events,
                    kind,
                    id,
                );
            });
        }

        Ok(Session {
            id,
            target,
            stream: Some(stream),
            worker: Some(worker),
            shared,
            ready: false,
        })
    }

    fn session_stream(&self, target: &str) -> Result<Stream> {
        let flags = pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS;
        // Never fall back to another device (possibly our own) if the real one goes away.
        match self.kind {
            Kind::Mic => record_stream(
                &self.core,
                "broadcast-linux-capture",
                properties! {
                    *pw::keys::MEDIA_TYPE => "Audio",
                    *pw::keys::MEDIA_CATEGORY => "Capture",
                    *pw::keys::MEDIA_ROLE => "Communication",
                    *pw::keys::NODE_NAME => "broadcast_linux_capture",
                    *pw::keys::AUDIO_CHANNELS => "1",
                    *pw::keys::NODE_LATENCY => LATENCY,
                    *pw::keys::NODE_GROUP => self.group.as_str(),
                    *pw::keys::NODE_LINK_GROUP => self.group.as_str(),
                    "target.object" => target,
                    "node.dont-reconnect" => "true",
                    "node.dont-fallback" => "true",
                },
                flags,
                self.rt.clone(),
            ),
            Kind::Speaker => play_stream(
                &self.core,
                "broadcast-linux-playback",
                properties! {
                    *pw::keys::MEDIA_TYPE => "Audio",
                    *pw::keys::MEDIA_CATEGORY => "Playback",
                    *pw::keys::MEDIA_ROLE => "Communication",
                    *pw::keys::NODE_NAME => "broadcast_linux_playback",
                    *pw::keys::AUDIO_CHANNELS => "1",
                    *pw::keys::NODE_LATENCY => LATENCY,
                    *pw::keys::NODE_GROUP => self.group.as_str(),
                    *pw::keys::NODE_LINK_GROUP => self.group.as_str(),
                    "target.object" => target,
                    "node.dont-reconnect" => "true",
                    "node.dont-fallback" => "true",
                },
                flags,
                self.rt.clone(),
            ),
        }
    }

    fn log_stats(&self, what: &str) {
        let stats = &self.rt.stats;
        let ms = |samples: &AtomicU64| samples.swap(0, Ordering::Relaxed) * 1000 / u64::from(RATE);
        eprintln!(
            "{}: {what} (dropped {} frames, late {} ms, underrun {} ms, resynced {} times)",
            self.kind.label(),
            stats.dropped_frames.swap(0, Ordering::Relaxed),
            ms(&stats.late_samples),
            ms(&stats.underrun_samples),
            stats.resyncs.swap(0, Ordering::Relaxed),
        );
        stats.flowing.store(false, Ordering::Relaxed);
    }
}

fn record_stream(
    core: &pw::core::CoreRc,
    name: &str,
    props: pw::properties::PropertiesBox,
    flags: pw::stream::StreamFlags,
    rt: Arc<Rt>,
) -> Result<Stream> {
    let stream = pw::stream::StreamRc::new(core.clone(), name, props)?;
    let listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, ()| capture_process(stream, &rt))
        .register()?;
    stream.connect(
        spa::utils::Direction::Input,
        None,
        flags,
        &mut [format_pod()?.as_pod()],
    )?;
    Ok(Stream {
        _listener: listener,
        stream,
    })
}

fn play_stream(
    core: &pw::core::CoreRc,
    name: &str,
    props: pw::properties::PropertiesBox,
    flags: pw::stream::StreamFlags,
    rt: Arc<Rt>,
) -> Result<Stream> {
    let stream = pw::stream::StreamRc::new(core.clone(), name, props)?;
    let listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, ()| playback_process(stream, &rt))
        .register()?;
    stream.connect(
        spa::utils::Direction::Output,
        None,
        flags,
        &mut [format_pod()?.as_pod()],
    )?;
    Ok(Stream {
        _listener: listener,
        stream,
    })
}

/// Graph time of the current cycle, the same for every node one driver runs.
fn cycle_time(stream: &pw::stream::Stream) -> i64 {
    // SAFETY: an all-zero pw_time is valid, and PipeWire writes at most `size` bytes.
    let mut time: pw::sys::pw_time = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<pw::sys::pw_time>();
    // SAFETY: the stream pointer is live for the duration of the callback.
    let res = unsafe { pw::sys::pw_stream_get_time_n(stream.as_raw_ptr(), &raw mut time, size) };
    if res < 0 { 0 } else { time.now }
}

/// Runs on PipeWire's realtime thread: no allocation, blocking or panics.
fn capture_process(stream: &pw::stream::Stream, rt: &Rt) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let now = cycle_time(stream);
    if now != 0 && rt.playback_now.load(Ordering::Relaxed) == now {
        rt.locked_at.store(now, Ordering::Relaxed);
    }
    let Some(data) = buffer.datas_mut().first_mut() else {
        return;
    };
    let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
    let Some(bytes) = data.data() else {
        return;
    };
    let end = (offset + size).min(bytes.len());
    let samples = bytes
        .get(offset..end)
        .unwrap_or_default()
        .as_chunks::<4>()
        .0;
    let Ok(mut capture) = rt.capture.try_lock() else {
        rt.lost.fetch_add(samples.len() as u64, Ordering::Relaxed);
        return;
    };
    let lost = rt.lost.swap(0, Ordering::Relaxed);
    if lost > 0 {
        // Don't splice a frame together from both sides of the gap.
        capture.next += lost;
        capture.pending.len = 0;
    }
    let start = capture.next;
    capture.take(samples, &rt.stats);
    if now != 0 {
        rt.clock.publish(start, capture.next, now);
        rt.clock_valid.store(true, Ordering::Release);
    }
}

/// Runs on PipeWire's realtime thread: no allocation, blocking or panics.
fn playback_process(stream: &pw::stream::Stream, rt: &Rt) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    // Fill only what this period needs; the buffer itself is much larger.
    let requested = usize::try_from(buffer.requested()).unwrap_or(0);
    let now = cycle_time(stream);
    rt.playback_now.store(now, Ordering::Relaxed);
    let Some(data) = buffer.datas_mut().first_mut() else {
        return;
    };
    let n = if let Some(bytes) = data.data() {
        let slots = bytes.as_chunks_mut::<4>().0;
        let n = if requested == 0 {
            slots.len().min(960)
        } else {
            requested.min(slots.len())
        };
        let out = &mut slots[..n];
        match rt.playback.try_lock() {
            Ok(mut playback) => playback.fill(out, now, rt),
            Err(_) => out.fill([0; 4]),
        }
        n
    } else {
        0
    };
    let chunk = data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = 4;
    *chunk.size_mut() = u32::try_from(n * 4).unwrap_or(0);
}

fn chain_name(stages: &[Stage]) -> String {
    let names: Vec<_> = stages.iter().map(|s| s.effect.selector()).collect();
    if names.is_empty() {
        "passthrough (no effects enabled)".into()
    } else {
        names.join("+")
    }
}

fn watch_stderr(
    stderr: impl Read,
    frame: &mpsc::Sender<usize>,
    events: &EventSender,
    shared: &Shared,
    kind: Kind,
    session: u64,
) {
    let mut failed = false;
    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        eprintln!("{} worker: {line}", kind.label());
        if !failed {
            line.clone_into(&mut shared.last_line.lock().unwrap());
        }
        // "<effect> ready; <n> samples per frame at 48 kHz mono f32"
        if let Some(n) = line
            .split_once(" ready; ")
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .and_then(|n| n.parse::<usize>().ok())
        {
            // The feeder then closes the worker's input, failing the start with this.
            if n != FRAME {
                failed = true;
                *shared.last_line.lock().unwrap() = format!("frame size {n}, expected {FRAME}");
            } else {
                let _ = events.send(Event::WorkerReady(kind, session));
            }
            let _ = frame.send(n);
        }
        // "Processed <n> frames, failures <n>, dropped <n>, max run ..."
        if let Some(dropped) = line
            .split_once(", dropped ")
            .and_then(|(_, rest)| rest.split(',').next())
            .and_then(|n| n.parse::<u64>().ok())
            && dropped > 0
        {
            eprintln!(
                "{} worker: dropped {dropped} frames itself, so its output no longer lines up with the input",
                kind.label()
            );
        }
    }
}

fn feed_worker(
    mut stdin: impl Write,
    mut ring: FrameCons,
    frame: &mpsc::Receiver<usize>,
    order: &mpsc::Sender<u64>,
    shared: &Shared,
    stats: &Stats,
) {
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        match frame.recv_timeout(Duration::from_millis(20)) {
            Ok(n) if n == FRAME => break,
            Ok(_) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                ring.clear();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
    ring.clear();
    let mut bytes = vec![0u8; FRAME * 4];
    while !shared.stop.load(Ordering::Relaxed) {
        let Some(frame) = ring.try_pop() else {
            let paused = !shared.active.load(Ordering::Relaxed);
            thread::park_timeout(Duration::from_millis(if paused { 50 } else { 20 }));
            continue;
        };
        let in_flight =
            shared.sent.load(Ordering::Relaxed) - shared.received.load(Ordering::Relaxed);
        if in_flight >= MAX_IN_FLIGHT {
            stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        for (b, s) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(&frame.samples) {
            *b = s.to_le_bytes();
        }
        // The drain thread pairs each output frame with these indices in order.
        if order.send(frame.index).is_err()
            || stdin
                .write_all(&bytes)
                .and_then(|()| stdin.flush())
                .is_err()
        {
            return;
        }
        shared.sent.fetch_add(1, Ordering::Relaxed);
    }
}

#[expect(clippy::too_many_arguments, reason = "one per pipe and channel")]
fn drain_worker(
    mut stdout: impl Read,
    mut ring: FrameProd,
    order: &mpsc::Receiver<u64>,
    shared: &Shared,
    stats: &Stats,
    events: &EventSender,
    kind: Kind,
    session: u64,
) {
    let mut buf = [0u8; 16_384];
    // Bytes at the front of `buf` left over from a read that split a sample.
    let mut carry = 0;
    let mut frame = Frame::EMPTY;
    'read: loop {
        let n = match stdout.read(&mut buf[carry..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => carry + n,
        };
        let (samples, rest) = buf[..n].as_chunks::<4>();
        for sample in samples {
            frame.samples[frame.len] = f32::from_le_bytes(*sample);
            frame.len += 1;
            if frame.len == FRAME {
                let Ok(index) = order.recv() else {
                    break 'read;
                };
                frame.index = index;
                if ring.try_push(frame).is_err() {
                    stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
                }
                frame.len = 0;
                shared.received.fetch_add(1, Ordering::Relaxed);
            }
        }
        carry = rest.len();
        buf.copy_within(n - carry..n, 0);
    }
    let _ = events.send(Event::WorkerExited(kind, session));
}

fn format_pod() -> Result<PodBytes> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(RATE);
    info.set_channels(1);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
    info.set_position(position);
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .map_err(|e| anyhow::anyhow!("building the audio format: {e:?}"))?
    .0
    .into_inner();
    Ok(PodBytes(bytes))
}

struct PodBytes(Vec<u8>);

impl PodBytes {
    fn as_pod(&self) -> &spa::pod::Pod {
        spa::pod::Pod::from_bytes(&self.0).expect("serialized pod")
    }
}
