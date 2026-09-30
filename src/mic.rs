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

use crate::config::MicConfig;
use crate::nvidia::{self, Installation};
use crate::paths::Paths;
use crate::service::{Event, EventSender};
use crate::worker::{Launch, Worker};

pub const NODE_NAME: &str = "broadcast_linux_mic";
const RATE: u32 = 48_000;
/// Broadcast's audio models all run on fixed 40 ms frames at 48 kHz.
const FRAME: usize = 1920;
/// Silence queued ahead of the first processed frame. The worker returns audio in
/// whole 40 ms frames, so this slack absorbs its run-time jitter.
const CUSHION: usize = 960;
/// Ask PipeWire for 20 ms periods; the default can be hundreds of milliseconds.
const LATENCY: &str = "960/48000";
/// Frames sent to the worker but not yet returned. Anything beyond this is dropped
/// so latency cannot build up.
const MAX_IN_FLIGHT: u64 = 2;
const RING: usize = RATE as usize;

static DROPPED_FRAMES: AtomicU64 = AtomicU64::new(0);
static UNDERRUN_SAMPLES: AtomicU64 = AtomicU64::new(0);
static TRIMMED_SAMPLES: AtomicU64 = AtomicU64::new(0);
/// Set once processed audio starts flowing; underruns before that are just the
/// model loading.
static FLOWING: AtomicBool = AtomicBool::new(false);

type Cons = ringbuf::HeapCons<f32>;
type Prod = ringbuf::HeapProd<f32>;

pub struct Mic {
    core: pw::core::CoreRc,
    config: MicConfig,
    events: EventSender,
    output: pw::stream::StreamRc,
    _output_listener: pw::stream::StreamListener<()>,
    output_ring: Rc<RefCell<Option<Cons>>>,
    readers: HashSet<u32>,
    session: Option<Session>,
    next_session: u64,
    failures: u32,
}

struct Session {
    id: u64,
    input: String,
    /// Dropped while paused, which releases the real mic.
    capture: Option<Capture>,
    feeder: Option<mpsc::Sender<Cons>>,
    worker: Option<Worker>,
    shared: Arc<Shared>,
    ready: bool,
}

type Capture = (pw::stream::StreamRc, pw::stream::StreamListener<()>);

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    active: AtomicBool,
    reprime: AtomicBool,
}

impl Drop for Session {
    fn drop(&mut self) {
        // The I/O threads are deliberately not joined: this runs inside the event
        // channel's callback, which holds the channel lock, and the stdout thread
        // sends an event when it exits, so joining it would deadlock. The feeder
        // stops on `stop`, the other two on EOF once the worker is killed, and
        // their late events carry a stale session id.
        self.shared.stop.store(true, Ordering::Relaxed);
        drop(self.worker.take());
    }
}

impl Mic {
    pub fn new(core: &pw::core::CoreRc, config: MicConfig, events: EventSender) -> Result<Self> {
        let output = pw::stream::StreamRc::new(
            core.clone(),
            "broadcast-linux-mic",
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CLASS => "Audio/Source",
                *pw::keys::NODE_NAME => NODE_NAME,
                *pw::keys::NODE_DESCRIPTION => config.name.as_str(),
                *pw::keys::AUDIO_CHANNELS => "1",
                *pw::keys::NODE_LATENCY => LATENCY,
            },
        )?;
        let output_ring: Rc<RefCell<Option<Cons>>> = Rc::default();
        let listener = output
            .add_local_listener_with_user_data(())
            .process({
                let ring = output_ring.clone();
                move |stream, ()| fill_output(stream, &mut ring.borrow_mut())
            })
            .register()?;
        output.connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [format_pod()?.as_pod()],
        )?;
        Ok(Self {
            core: core.clone(),
            config,
            events,
            output,
            _output_listener: listener,
            output_ring,
            readers: HashSet::new(),
            session: None,
            next_session: 0,
            failures: 0,
        })
    }

    pub fn node_id(&self) -> u32 {
        self.output.node_id()
    }

    pub fn has_session(&self) -> bool {
        self.session.is_some()
    }

    pub fn readers(&self) -> usize {
        self.readers.len()
    }

    pub fn link_added(&mut self, link: u32, output_node: u32) -> bool {
        output_node == self.node_id() && self.readers.insert(link)
    }

    pub fn link_removed(&mut self, link: u32) -> bool {
        self.readers.remove(&link)
    }

    pub fn set_config(&mut self, config: MicConfig) -> bool {
        let changed = config != self.config;
        if config.name != self.config.name {
            eprintln!("mic: the device name only changes after the service restarts");
        }
        self.config = config;
        changed
    }

    pub fn unload_after(&self) -> Duration {
        Duration::from_secs(self.config.unload_after_minutes * 60)
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
                    "mic: started {} on {} (session {id})",
                    chain_name(&self.config),
                    session.input
                );
                self.session = Some(session);
            }
            Err(e) => eprintln!("mic: could not start: {e:#}"),
        }
    }

    /// Called once the idle timeout after the last reader has passed. Returns
    /// whether the session was only paused, so it still needs unloading later.
    pub fn idle(&mut self) -> bool {
        let has_model = self.session.as_ref().is_some_and(|s| s.worker.is_some());
        if has_model && self.config.unload_after_minutes > 0 {
            self.pause();
            self.session.is_some()
        } else {
            self.stop();
            false
        }
    }

    pub fn stop(&mut self) {
        if let Some(session) = self.session.take() {
            *self.output_ring.borrow_mut() = None;
            let was_active = session.shared.active.load(Ordering::Relaxed);
            drop(session);
            if was_active {
                log_stats("stopped");
            } else {
                eprintln!("mic: stopped");
            }
        }
    }

    fn pause(&mut self) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        if session.capture.take().is_none() {
            return;
        }
        session.shared.active.store(false, Ordering::Relaxed);
        log_stats("paused; the model stays loaded");
    }

    fn resume(&mut self) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        if session.capture.is_none() {
            let (mut prod, cons) = HeapRb::<f32>::new(RING).split();
            if let Some(feeder) = &session.feeder {
                let _ = feeder.send(cons);
                session.shared.reprime.store(true, Ordering::Relaxed);
                if let Some(ring) = self.output_ring.borrow_mut().as_mut() {
                    ring.clear();
                }
            } else {
                prod.push_iter(std::iter::repeat_n(0.0, CUSHION));
                *self.output_ring.borrow_mut() = Some(cons);
                FLOWING.store(true, Ordering::Relaxed);
            }
            match self.capture_stream(&session.input, prod) {
                Ok(capture) => {
                    session.capture = Some(capture);
                    session.shared.active.store(true, Ordering::Relaxed);
                    eprintln!("mic: resumed");
                }
                Err(e) => {
                    eprintln!("mic: could not resume: {e:#}");
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
            eprintln!("mic: effect running");
        }
    }

    pub fn worker_exited(&mut self, session: u64) -> Option<Duration> {
        let current = self.session.as_ref().filter(|s| s.id == session)?;
        let was_ready = current.ready;
        self.stop();
        self.failures += 1;
        if !was_ready && self.failures > 2 {
            eprintln!("mic: the effect failed to load twice; giving up until the next use");
            self.failures = 0;
            return None;
        }
        let delay = Duration::from_secs(1 << self.failures.min(5));
        eprintln!(
            "mic: worker {} ; restarting in {}s",
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
        let input = resolve_input(&self.config.input)?;
        let stages = self.config.stages();
        let (in_prod, in_cons) = HeapRb::<f32>::new(RING).split();
        let (mut out_prod, out_cons) = HeapRb::<f32>::new(RING).split();
        let shared = Arc::new(Shared::default());
        shared.active.store(true, Ordering::Relaxed);

        if stages.is_empty() {
            out_prod.push_iter(std::iter::repeat_n(0.0, CUSHION));
            let capture = self.capture_stream(&input, out_prod)?;
            *self.output_ring.borrow_mut() = Some(out_cons);
            FLOWING.store(true, Ordering::Relaxed);
            return Ok(Session {
                id,
                input,
                capture: Some(capture),
                feeder: None,
                worker: None,
                shared,
                ready: true,
            });
        }

        let install = Installation::find(paths)?;
        let mut args = vec!["0".to_owned()];
        for stage in &stages {
            let (folder, file) = stage.effect.model();
            args.extend([
                stage.effect.selector().to_owned(),
                nvidia::windows_path(&install.model(folder, file)?),
                stage.strength.to_string(),
            ]);
        }
        let capture = self.capture_stream(&input, in_prod)?;

        let (worker, pipes) = Worker::spawn(Launch {
            program: "afx_stream",
            prefix: paths.prefix(),
            cwd: install.runtime.clone(),
            args,
            stdin: None,
        })?;
        let stdin = pipes.stdin.context("worker stdin")?;

        let sent = Arc::new(AtomicU64::new(0));
        let received = Arc::new(AtomicU64::new(0));
        let (frame_tx, frame_rx) = mpsc::channel::<usize>();
        let (feeder, inputs) = mpsc::channel::<Cons>();
        let events = self.events.clone();
        thread::spawn(move || watch_stderr(pipes.stderr, &frame_tx, &events, id));
        {
            let (sent, received, shared) = (sent.clone(), received.clone(), shared.clone());
            thread::spawn(move || {
                feed_worker(
                    stdin, in_cons, &inputs, &frame_rx, &sent, &received, &shared,
                );
            });
        }
        let events = self.events.clone();
        {
            let shared = shared.clone();
            thread::spawn(move || {
                drain_worker(pipes.stdout, out_prod, &received, &shared, &events, id);
            });
        }

        *self.output_ring.borrow_mut() = Some(out_cons);
        Ok(Session {
            id,
            input,
            capture: Some(capture),
            feeder: Some(feeder),
            worker: Some(worker),
            shared,
            ready: false,
        })
    }

    fn capture_stream(&self, input: &str, mut ring: Prod) -> Result<Capture> {
        let stream = pw::stream::StreamRc::new(
            self.core.clone(),
            "broadcast-linux-capture",
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Communication",
                *pw::keys::NODE_NAME => "broadcast_linux_capture",
                *pw::keys::AUDIO_CHANNELS => "1",
                *pw::keys::NODE_LATENCY => LATENCY,
                "target.object" => input,
                // Never fall back to another source (possibly our own) if the mic goes away.
                "node.dont-reconnect" => "true",
                "node.dont-fallback" => "true",
            },
        )?;
        let listener = stream
            .add_local_listener_with_user_data(())
            .process(move |stream, ()| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let data = &mut buffer.datas_mut()[0];
                let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
                if let Some(bytes) = data.data() {
                    let end = (offset + size).min(bytes.len());
                    let samples = bytes[offset..end]
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                    ring.push_iter(samples);
                }
            })
            .register()?;
        stream.connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [format_pod()?.as_pod()],
        )?;
        Ok((stream, listener))
    }
}

fn log_stats(what: &str) {
    let ms = |samples: u64| samples * 1000 / u64::from(RATE);
    eprintln!(
        "mic: {what} (dropped {} frames, underrun {} ms, trimmed {} ms)",
        DROPPED_FRAMES.swap(0, Ordering::Relaxed),
        ms(UNDERRUN_SAMPLES.swap(0, Ordering::Relaxed)),
        ms(TRIMMED_SAMPLES.swap(0, Ordering::Relaxed)),
    );
    FLOWING.store(false, Ordering::Relaxed);
}

fn chain_name(config: &MicConfig) -> String {
    let names: Vec<_> = config
        .stages()
        .iter()
        .map(|s| s.effect.selector())
        .collect();
    if names.is_empty() {
        "passthrough (no effects enabled)".into()
    } else {
        names.join("+")
    }
}

fn resolve_input(input: &str) -> Result<String> {
    let name = if input == "default" {
        let out = Command::new("pactl")
            .arg("get-default-source")
            .output()
            .context("running pactl get-default-source")?;
        String::from_utf8(out.stdout)?.trim().to_owned()
    } else {
        input.to_owned()
    };
    if name.is_empty() {
        bail!("no default microphone is set");
    }
    if name == NODE_NAME {
        bail!("the default source is {NODE_NAME} itself; set [mic] input to the real microphone");
    }
    Ok(name)
}

fn fill_output(stream: &pw::stream::Stream, ring: &mut Option<Cons>) {
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
            // Clock drift between the mic and this node's driver would slowly grow
            // the queue. What is left after this period should never exceed one
            // frame plus the cushion; trim anything beyond a margin above that.
            let keep = n + FRAME + CUSHION;
            if ring.occupied_len() > keep + 960 {
                let excess = ring.occupied_len() - keep;
                TRIMMED_SAMPLES.fetch_add(excess as u64, Ordering::Relaxed);
                ring.skip(excess);
            }
            for (slot, sample) in bytes[..n * 4].chunks_exact_mut(4).zip(ring.pop_iter()) {
                slot.copy_from_slice(&sample.to_le_bytes());
                filled += 1;
            }
        }
        if ring.is_some() && FLOWING.load(Ordering::Relaxed) {
            UNDERRUN_SAMPLES.fetch_add(n.saturating_sub(filled) as u64, Ordering::Relaxed);
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
    session: u64,
) {
    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        eprintln!("mic worker: {line}");
        // "<effect> ready; <n> samples per frame at 48 kHz mono f32"
        if let Some(n) = line
            .split_once(" ready; ")
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .and_then(|n| n.parse::<usize>().ok())
        {
            if n != FRAME {
                eprintln!("mic worker: unexpected frame size {n}, expected {FRAME}");
            }
            let _ = frame.send(n);
            let _ = events.send(Event::MicWorkerReady(session));
        }
    }
}

fn feed_worker(
    mut stdin: impl Write,
    mut ring: Cons,
    inputs: &mpsc::Receiver<Cons>,
    frame: &mpsc::Receiver<usize>,
    sent: &AtomicU64,
    received: &AtomicU64,
    shared: &Shared,
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
            thread::sleep(Duration::from_millis(if paused { 50 } else { 2 }));
            continue;
        }
        ring.pop_slice(&mut samples);
        if sent.load(Ordering::Relaxed) - received.load(Ordering::Relaxed) >= MAX_IN_FLIGHT {
            DROPPED_FRAMES.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        for (b, s) in bytes.chunks_exact_mut(4).zip(&samples) {
            b.copy_from_slice(&s.to_le_bytes());
        }
        if stdin
            .write_all(&bytes)
            .and_then(|()| stdin.flush())
            .is_err()
        {
            return;
        }
        sent.fetch_add(1, Ordering::Relaxed);
    }
}

fn drain_worker(
    mut stdout: impl Read,
    mut ring: Prod,
    received: &AtomicU64,
    shared: &Shared,
    events: &EventSender,
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
            FLOWING.store(true, Ordering::Relaxed);
        }
        carry.extend_from_slice(&buf[..n]);
        let whole = carry.len() / 4 * 4;
        let samples = carry[..whole]
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        frame_samples += samples.len();
        ring.push_iter(samples);
        carry.drain(..whole);
        while frame_samples >= FRAME {
            frame_samples -= FRAME;
            received.fetch_add(1, Ordering::Relaxed);
        }
    }
    let _ = events.send(Event::MicWorkerExited(session));
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
