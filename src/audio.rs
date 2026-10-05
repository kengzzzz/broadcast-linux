use std::cell::RefCell;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, Producer, Split};

use crate::config::{MicConfig, SpeakerConfig, Stage};
use crate::nvidia::{self, Installation};
use crate::paths::Paths;
use crate::service::{Event, EventSender};
use crate::worker::{Launch, Worker};

const RATE: u32 = 48_000;
/// Broadcast's audio models all run on fixed 40 ms frames at 48 kHz.
const FRAME: usize = 1920;
/// Silence queued before the first processed frame to absorb worker jitter.
const CUSHION: usize = 960;
/// Ask PipeWire for 20 ms periods; the default can be hundreds of milliseconds.
const LATENCY: &str = "960/48000";
/// Frames in the worker beyond this are dropped so latency can't build up.
const MAX_IN_FLIGHT: u64 = 2;
const RING: usize = RATE as usize;

type Cons = ringbuf::HeapCons<f32>;
type Prod = ringbuf::HeapProd<f32>;
type Stream = (pw::stream::StreamRc, pw::stream::StreamListener<()>);
type Slot<T> = Rc<RefCell<Option<T>>>;

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

    pub(crate) fn node_name(self) -> &'static str {
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

#[derive(Default)]
struct Stats {
    dropped_frames: AtomicU64,
    underrun_samples: AtomicU64,
    trimmed_samples: AtomicU64,
    /// Set once processed audio flows; earlier underruns are just model loading.
    flowing: AtomicBool,
}

/// Mic: real mic -> input, output -> node. Speaker: node -> input, output -> real device.
pub struct AudioDevice {
    kind: Kind,
    core: pw::core::CoreRc,
    settings: Settings,
    events: EventSender,
    node: Stream,
    input: Slot<Prod>,
    feeder_thread: Slot<thread::Thread>,
    output: Slot<Cons>,
    stats: Arc<Stats>,
    readers: HashSet<u32>,
    session: Option<Session>,
    next_session: u64,
    failures: u32,
    /// Bumped whenever a pending idle timeout should be ignored.
    idle_token: u64,
}

struct Session {
    id: u64,
    target: String,
    /// The real device's stream; dropped while paused, which releases it.
    stream: Option<Stream>,
    feeder: Option<mpsc::Sender<Cons>>,
    worker: Option<Worker>,
    shared: Arc<Shared>,
    ready: bool,
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    active: AtomicBool,
    reprime: AtomicBool,
    sent: AtomicU64,
    received: AtomicU64,
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
        let input = Slot::default();
        let feeder_thread = Slot::default();
        let output = Slot::default();
        let stats = Arc::<Stats>::default();
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
        };
        let name = format!("broadcast-linux-{}", kind.label());
        let flags = pw::stream::StreamFlags::MAP_BUFFERS;
        let node = match kind {
            Kind::Mic => play_stream(core, &name, props, flags, output.clone(), stats.clone())?,
            Kind::Speaker => record_stream(
                core,
                &name,
                props,
                flags,
                input.clone(),
                feeder_thread.clone(),
            )?,
        };
        Ok(Self {
            kind,
            core: core.clone(),
            settings,
            events,
            node,
            input,
            feeder_thread,
            output,
            stats,
            readers: HashSet::new(),
            session: None,
            next_session: 0,
            failures: 0,
            idle_token: 0,
        })
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn node_id(&self) -> u32 {
        self.node.0.node_id()
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

    pub fn link_added(&mut self, link: u32, output_node: u32, input_node: u32) -> bool {
        let node = match self.kind {
            Kind::Mic => output_node,
            Kind::Speaker => input_node,
        };
        node == self.node_id() && self.readers.insert(link)
    }

    pub fn link_removed(&mut self, link: u32) -> bool {
        self.readers.remove(&link)
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
                self.clear_rings();
                eprintln!("{}: could not start: {e:#}", self.kind.label());
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
            self.clear_rings();
            let was_active = session.shared.active.load(Ordering::Relaxed);
            drop(session);
            if was_active {
                self.log_stats("stopped");
            } else {
                eprintln!("{}: stopped", self.kind.label());
            }
        }
    }

    fn clear_rings(&self) {
        *self.input.borrow_mut() = None;
        *self.output.borrow_mut() = None;
    }

    fn pause(&mut self) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        if session.stream.take().is_none() {
            return;
        }
        *self.input.borrow_mut() = None;
        session.shared.active.store(false, Ordering::Relaxed);
        self.log_stats("paused; the model stays loaded");
    }

    fn resume(&mut self) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        if session.stream.is_none() {
            let (mut prod, cons) = HeapRb::<f32>::new(RING).split();
            if let Some(feeder) = &session.feeder {
                let _ = feeder.send(cons);
                session.shared.reprime.store(true, Ordering::Relaxed);
                if let Some(ring) = self.output.borrow_mut().as_mut() {
                    ring.clear();
                }
            } else {
                prod.push_iter(std::iter::repeat_n(0.0, CUSHION));
                *self.output.borrow_mut() = Some(cons);
                self.stats.flowing.store(true, Ordering::Relaxed);
            }
            *self.input.borrow_mut() = Some(prod);
            match self.session_stream(&session.target) {
                Ok(stream) => {
                    session.stream = Some(stream);
                    session.shared.active.store(true, Ordering::Relaxed);
                    eprintln!("{}: resumed", self.kind.label());
                }
                Err(e) => {
                    eprintln!("{}: could not resume: {e:#}", self.kind.label());
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
            eprintln!("{}: effect running", self.kind.label());
        }
    }

    pub fn worker_exited(&mut self, session: u64) -> Option<Duration> {
        let current = self.session.as_ref().filter(|s| s.id == session)?;
        let was_ready = current.ready;
        self.stop();
        self.failures += 1;
        if !was_ready && self.failures > 2 {
            eprintln!(
                "{}: the effect failed to load twice; giving up until the next use",
                self.kind.label()
            );
            self.failures = 0;
            return None;
        }
        let delay = Duration::from_secs(1 << self.failures.min(5));
        eprintln!(
            "{}: worker {} ; restarting in {}s",
            self.kind.label(),
            if was_ready {
                "crashed"
            } else {
                "failed to start"
            },
            delay.as_secs()
        );
        Some(delay)
    }

    fn start_session(&mut self, id: u64, paths: &Paths) -> Result<Session> {
        let target = resolve_target(self.kind, &self.settings.target)?;
        let (in_prod, in_cons) = HeapRb::<f32>::new(RING).split();
        let (mut out_prod, out_cons) = HeapRb::<f32>::new(RING).split();
        let shared = Arc::new(Shared::default());
        shared.active.store(true, Ordering::Relaxed);

        if self.settings.stages.is_empty() {
            out_prod.push_iter(std::iter::repeat_n(0.0, CUSHION));
            *self.input.borrow_mut() = Some(out_prod);
            *self.output.borrow_mut() = Some(out_cons);
            let stream = self.session_stream(&target)?;
            self.stats.flowing.store(true, Ordering::Relaxed);
            return Ok(Session {
                id,
                target,
                stream: Some(stream),
                feeder: None,
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
        *self.input.borrow_mut() = Some(in_prod);
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
        let (feeder, inputs) = mpsc::channel::<Cons>();
        let kind = self.kind;
        let events = self.events.clone();
        thread::spawn(move || watch_stderr(pipes.stderr, &frame_tx, &events, kind, id));
        {
            let (shared, stats) = (shared.clone(), self.stats.clone());
            let feeder = thread::spawn(move || {
                feed_worker(stdin, in_cons, &inputs, &frame_rx, &shared, &stats);
            });
            *self.feeder_thread.borrow_mut() = Some(feeder.thread().clone());
        }
        let events = self.events.clone();
        {
            let (shared, stats) = (shared.clone(), self.stats.clone());
            thread::spawn(move || {
                drain_worker(pipes.stdout, out_prod, &shared, &stats, &events, kind, id);
            });
        }

        *self.output.borrow_mut() = Some(out_cons);
        Ok(Session {
            id,
            target,
            stream: Some(stream),
            feeder: Some(feeder),
            worker: Some(worker),
            shared,
            ready: false,
        })
    }

    fn session_stream(&self, target: &str) -> Result<Stream> {
        let flags = pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS;
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
                    "target.object" => target,
                    "node.dont-reconnect" => "true",
                    "node.dont-fallback" => "true",
                },
                flags,
                self.input.clone(),
                self.feeder_thread.clone(),
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
                    "target.object" => target,
                    "node.dont-reconnect" => "true",
                    "node.dont-fallback" => "true",
                },
                flags,
                self.output.clone(),
                self.stats.clone(),
            ),
        }
    }

    fn log_stats(&self, what: &str) {
        let ms = |samples: &AtomicU64| samples.swap(0, Ordering::Relaxed) * 1000 / u64::from(RATE);
        eprintln!(
            "{}: {what} (dropped {} frames, underrun {} ms, trimmed {} ms)",
            self.kind.label(),
            self.stats.dropped_frames.swap(0, Ordering::Relaxed),
            ms(&self.stats.underrun_samples),
            ms(&self.stats.trimmed_samples),
        );
        self.stats.flowing.store(false, Ordering::Relaxed);
    }
}

fn record_stream(
    core: &pw::core::CoreRc,
    name: &str,
    props: pw::properties::PropertiesBox,
    flags: pw::stream::StreamFlags,
    ring: Slot<Prod>,
    feeder: Slot<thread::Thread>,
) -> Result<Stream> {
    let stream = pw::stream::StreamRc::new(core.clone(), name, props)?;
    let listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, ()| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let mut ring = ring.borrow_mut();
            let Some(ring) = ring.as_mut() else {
                return;
            };
            let data = &mut buffer.datas_mut()[0];
            let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
            if let Some(bytes) = data.data() {
                let end = (offset + size).min(bytes.len());
                let samples = bytes[offset..end]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_le_bytes(*b));
                ring.push_iter(samples);
            }
            if let Some(feeder) = feeder.borrow().as_ref() {
                feeder.unpark();
            }
        })
        .register()?;
    stream.connect(
        spa::utils::Direction::Input,
        None,
        flags,
        &mut [format_pod()?.as_pod()],
    )?;
    Ok((stream, listener))
}

fn play_stream(
    core: &pw::core::CoreRc,
    name: &str,
    props: pw::properties::PropertiesBox,
    flags: pw::stream::StreamFlags,
    ring: Slot<Cons>,
    stats: Arc<Stats>,
) -> Result<Stream> {
    let stream = pw::stream::StreamRc::new(core.clone(), name, props)?;
    let listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, ()| fill_output(stream, &mut ring.borrow_mut(), &stats))
        .register()?;
    stream.connect(
        spa::utils::Direction::Output,
        None,
        flags,
        &mut [format_pod()?.as_pod()],
    )?;
    Ok((stream, listener))
}

fn chain_name(stages: &[Stage]) -> String {
    let names: Vec<_> = stages.iter().map(|s| s.effect.selector()).collect();
    if names.is_empty() {
        "passthrough (no effects enabled)".into()
    } else {
        names.join("+")
    }
}

pub(crate) fn resolve_target(kind: Kind, target: &str) -> Result<String> {
    let (pactl, key, device) = match kind {
        Kind::Mic => ("get-default-source", "input", "microphone"),
        Kind::Speaker => ("get-default-sink", "output", "output device"),
    };
    let name = if target == "default" {
        let out = Command::new("pactl")
            .arg(pactl)
            .output()
            .with_context(|| format!("running pactl {pactl}"))?;
        String::from_utf8(out.stdout)?.trim().to_owned()
    } else {
        target.to_owned()
    };
    if name.is_empty() {
        bail!("no default {device} is set");
    }
    if name == kind.node_name() {
        let label = kind.label();
        bail!("{name} is this {label} itself; set [{label}] {key} to the real {device}");
    }
    Ok(name)
}

fn fill_output(stream: &pw::stream::Stream, ring: &mut Option<Cons>, stats: &Stats) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    // Fill only what this period needs; the buffer itself is much larger.
    let requested = usize::try_from(buffer.requested()).unwrap_or(0);
    let data = &mut buffer.datas_mut()[0];
    let n = if let Some(bytes) = data.data() {
        let capacity = bytes.len() / 4;
        let n = if requested == 0 {
            capacity.min(960)
        } else {
            requested.min(capacity)
        };
        let mut filled = 0;
        if let Some(ring) = ring.as_mut() {
            // Clock drift grows the queue; trim it back to a frame plus the cushion.
            let keep = n + FRAME + CUSHION;
            if ring.occupied_len() > keep + 960 {
                let excess = ring.occupied_len() - keep;
                stats
                    .trimmed_samples
                    .fetch_add(excess as u64, Ordering::Relaxed);
                ring.skip(excess);
            }
            for (slot, sample) in bytes[..n * 4]
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(ring.pop_iter())
            {
                *slot = sample.to_le_bytes();
                filled += 1;
            }
        }
        if ring.is_some() && stats.flowing.load(Ordering::Relaxed) {
            stats
                .underrun_samples
                .fetch_add(n.saturating_sub(filled) as u64, Ordering::Relaxed);
        }
        bytes[filled * 4..n * 4].fill(0);
        n
    } else {
        0
    };
    let chunk = data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = 4;
    *chunk.size_mut() = u32::try_from(n * 4).unwrap_or(0);
}

fn watch_stderr(
    stderr: impl Read,
    frame: &mpsc::Sender<usize>,
    events: &EventSender,
    kind: Kind,
    session: u64,
) {
    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        eprintln!("{} worker: {line}", kind.label());
        // "<effect> ready; <n> samples per frame at 48 kHz mono f32"
        if let Some(n) = line
            .split_once(" ready; ")
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .and_then(|n| n.parse::<usize>().ok())
        {
            if n != FRAME {
                eprintln!(
                    "{} worker: unexpected frame size {n}, expected {FRAME}",
                    kind.label()
                );
            }
            let _ = frame.send(n);
            let _ = events.send(Event::WorkerReady(kind, session));
        }
    }
}

fn feed_worker(
    mut stdin: impl Write,
    mut ring: Cons,
    inputs: &mpsc::Receiver<Cons>,
    frame: &mpsc::Receiver<usize>,
    shared: &Shared,
    stats: &Stats,
) {
    let frame = loop {
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        match frame.recv_timeout(Duration::from_millis(20)) {
            Ok(n) => break n,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                ring.clear();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    };
    ring.clear();
    let mut samples = vec![0f32; frame];
    let mut bytes = vec![0u8; frame * 4];
    while !shared.stop.load(Ordering::Relaxed) {
        while let Ok(input) = inputs.try_recv() {
            ring = input;
        }
        if ring.occupied_len() < frame {
            let paused = !shared.active.load(Ordering::Relaxed);
            thread::park_timeout(Duration::from_millis(if paused { 50 } else { 20 }));
            continue;
        }
        ring.pop_slice(&mut samples);
        let in_flight =
            shared.sent.load(Ordering::Relaxed) - shared.received.load(Ordering::Relaxed);
        if in_flight >= MAX_IN_FLIGHT {
            stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        for (b, s) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(&samples) {
            *b = s.to_le_bytes();
        }
        if stdin
            .write_all(&bytes)
            .and_then(|()| stdin.flush())
            .is_err()
        {
            return;
        }
        shared.sent.fetch_add(1, Ordering::Relaxed);
    }
}

fn drain_worker(
    mut stdout: impl Read,
    mut ring: Prod,
    shared: &Shared,
    stats: &Stats,
    events: &EventSender,
    kind: Kind,
    session: u64,
) {
    let mut buf = [0u8; 16_384];
    let mut carry = Vec::with_capacity(4);
    let mut frame_samples = 0;
    let mut primed = false;
    loop {
        let n = match stdout.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if !primed || shared.reprime.swap(false, Ordering::Relaxed) {
            ring.push_iter(std::iter::repeat_n(0.0, CUSHION));
            primed = true;
            stats.flowing.store(true, Ordering::Relaxed);
        }
        carry.extend_from_slice(&buf[..n]);
        let whole = carry.len() / 4 * 4;
        let samples = carry[..whole]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b));
        frame_samples += samples.len();
        ring.push_iter(samples);
        carry.drain(..whole);
        while frame_samples >= FRAME {
            frame_samples -= FRAME;
            shared.received.fetch_add(1, Ordering::Relaxed);
        }
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
