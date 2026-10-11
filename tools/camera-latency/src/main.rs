//! Camera delay with the old and new frame scheduling, using the real webcam, decoder and
//! camera worker but not the virtual camera. Delay runs from the webcam's capture
//! timestamp to the worker's finished frame.
//!
//! To emulate a slow GPU: `sudo nvidia-smi -lgc 600,600`, then `sudo nvidia-smi -rgc`.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufRead, BufReader, PipeWriter, Read, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use broadcast_linux::camera::Pace;
use broadcast_linux::config::{InputColor, InputFormat, LightPreset, ParallelDecode};
use broadcast_linux::frames::{Decoder, SharedFrames};
use broadcast_linux::nvidia::{self, Installation};
use broadcast_linux::paths::Paths;
use broadcast_linux::webcam::{self, Capture, Color};
use broadcast_linux::worker::{Launch, Worker};
use clap::{Parser, ValueEnum};

const SETTLE: Duration = Duration::from_secs(3);

/// Camera delay with the old and new frame scheduling. Close apps using the virtual camera
/// first, so the webcam is free.
#[derive(Parser)]
struct Args {
    /// Webcam device
    #[arg(long, default_value = "/dev/video0")]
    input: String,
    #[arg(long, default_value_t = 1920)]
    width: u32,
    #[arg(long, default_value_t = 1080)]
    height: u32,
    #[arg(long, default_value_t = 30)]
    fps: u32,
    /// Seconds measured per scheduling, after 3 s to settle
    #[arg(long, default_value_t = 20)]
    seconds: u64,
    #[arg(long, value_enum, default_value_t = Scheduling::Both)]
    scheduling: Scheduling,
    /// Leave out video noise removal
    #[arg(long)]
    no_denoise: bool,
    /// Leave out Studio Light
    #[arg(long)]
    no_studio_light: bool,
    /// Write each frame's times to PREFIX-old.csv and PREFIX-new.csv
    #[arg(long, value_name = "PREFIX")]
    csv: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Scheduling {
    /// Decode right away (0.4.3 and earlier)
    Old,
    /// Decode just before the worker is free
    New,
    Both,
}

#[derive(Clone, Copy)]
struct Frame {
    captured: Instant,
    dequeued: Instant,
    decoded: Instant,
    sent: Instant,
    started: Instant,
    done: Instant,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if std::env::var_os("BROADCAST_LINUX_LIBDIR").is_none() {
        // SAFETY: no other thread runs yet.
        unsafe { std::env::set_var("BROADCAST_LINUX_LIBDIR", "/usr/lib/broadcast-linux") };
    }
    let paths = Paths::new()?;
    let install = Installation::find(&paths)?;
    let runs: &[bool] = match args.scheduling {
        Scheduling::Old => &[false],
        Scheduling::New => &[true],
        Scheduling::Both => &[false, true],
    };
    for &new in runs {
        let name = if new { "new" } else { "old" };
        let frames = measure(&args, &paths, &install, new)?;
        report(name, &frames)?;
        if let Some(prefix) = &args.csv {
            let mut path = prefix.clone().into_os_string();
            path.push(format!("-{name}.csv"));
            write_csv(Path::new(&path), &frames)?;
        }
    }
    Ok(())
}

fn measure(args: &Args, paths: &Paths, install: &Installation, new: bool) -> Result<Vec<Frame>> {
    let (width, height) = (args.width, args.height);
    let format = webcam::choose(&args.input, InputFormat::Auto, width, height)?;
    let mut capture = Capture::open(&args.input, format, width, height, args.fps)?;
    let color = Color::resolve(format, InputColor::Auto, capture.reported_color());
    let deadline = Instant::now() + Duration::from_secs(5);
    let decoder = loop {
        if let Some(decoder) = capture.newest(Duration::from_millis(200), |frame| {
            Decoder::new(
                format,
                color,
                frame,
                width,
                height,
                args.fps,
                ParallelDecode::Auto,
            )
        })? {
            break decoder?;
        }
        if Instant::now() >= deadline {
            bail!("{} sent no frames", args.input);
        }
    };
    let out_bytes = usize::try_from(u64::from(width) * u64::from(height) * 2)?;
    let frames = Arc::new(SharedFrames::new(decoder.worker_bytes(), out_bytes, None)?);
    let (tokens_in, tokens) = io::pipe()?;
    let (worker, pipes) = Worker::spawn(Launch {
        program: "camera_stream",
        prefix: paths.prefix(),
        cwd: install.runtime.clone(),
        args: worker_args(args, install, &decoder, &frames)?,
        stdin: Some(Stdio::from(tokens_in)),
    })?;
    let (ready_tx, ready_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(pipes.stderr).lines().map_while(Result::ok) {
            if line.starts_with("Camera effects ready") {
                let _ = ready_tx.send(());
            } else if !line.starts_with("Processed") {
                eprintln!("worker: {line}");
            }
        }
    });
    ready_rx
        .recv_timeout(Duration::from_secs(120))
        .context("the worker did not load the effects")?;

    let in_flight = Arc::new(Mutex::new(vec![None; frames.slots()]));
    let stop = Arc::new(AtomicBool::new(false));
    let (free, free_slots) = mpsc::channel();
    for slot in 0..frames.slots() {
        free.send((slot, None))?;
    }
    let feed = {
        let (frames, in_flight, stop) = (
            Arc::clone(&frames),
            Arc::clone(&in_flight),
            Arc::clone(&stop),
        );
        thread::spawn(move || {
            run_feed(
                capture,
                decoder,
                &frames,
                &free_slots,
                tokens,
                &in_flight,
                new,
                &stop,
            )
        })
    };
    let result = collect(
        File::from(OwnedFd::from(pipes.stdout)),
        &in_flight,
        &free,
        args.seconds,
    );
    stop.store(true, Ordering::Relaxed);
    drop(worker);
    if let Ok(Err(e)) = feed.join()
        && result.is_err()
    {
        eprintln!("feed: {e:#}");
    }
    result
}

fn worker_args(
    args: &Args,
    install: &Installation,
    decoder: &Decoder,
    frames: &SharedFrames,
) -> Result<Vec<String>> {
    let mut worker = vec![
        nvidia::windows_path(&install.model_dir(nvidia::BACKGROUND_MODELS)?),
        "--size".into(),
        format!("{}x{}", args.width, args.height),
        "--input".into(),
        decoder.layout().worker_name().into(),
        "--color".into(),
        decoder.color().worker_name().into(),
        "--shm".into(),
        frames.path().context("no shared frames for the worker")?,
    ];
    if !args.no_denoise {
        let models = install.model_dir(nvidia::VIDEO_DENOISE_MODELS)?;
        worker.extend(["--denoise".into(), nvidia::windows_path(&models)]);
    }
    if !args.no_studio_light {
        let models = install.model_dir(nvidia::STUDIO_LIGHT_MODELS)?;
        let hdr = install.studio_light(LightPreset::Neutral.file())?;
        worker.extend([
            "--relight".into(),
            nvidia::windows_path(&models),
            "--hdr".into(),
            hdr.display().to_string(),
        ]);
    }
    Ok(worker)
}

/// A frame's times up to sending it to the worker.
#[derive(Clone, Copy)]
struct Queued {
    captured: Instant,
    dequeued: Instant,
    decoded: Instant,
    sent: Instant,
}

type InFlight = Mutex<Vec<Option<Queued>>>;

/// The feed loop of `src/camera.rs`, with the old scheduling when `new` is false.
#[allow(clippy::too_many_arguments)]
fn run_feed(
    mut capture: Capture,
    mut decoder: Decoder,
    frames: &SharedFrames,
    free: &Receiver<(usize, Option<Instant>)>,
    mut tokens: PipeWriter,
    in_flight: &InFlight,
    new: bool,
    stop: &AtomicBool,
) -> Result<()> {
    let mut pace = Pace::default();
    let mut slots = VecDeque::new();
    let mut warned = false;
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let wake = pace.next_decode().filter(|&at| new && at > now);
        if slots.is_empty() || wake.is_some() {
            let wait = wake.map_or(Duration::from_millis(100), |at| {
                (at - now).min(Duration::from_millis(100))
            });
            match free.recv_timeout(wait) {
                Ok((slot, done)) => {
                    if let Some(done) = done {
                        pace.done(done);
                    }
                    slots.push_back(slot);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            continue;
        }
        let k = slots[0];
        let mut times = None;
        let filled = capture.newest(Duration::from_millis(100), |frame| -> Result<bool> {
            let dequeued = Instant::now();
            // SAFETY: slot k came back as free, so nothing else uses it until its number
            // is sent on below.
            let filled = decoder.fill_for_worker(frame, unsafe { frames.input(k) })?;
            times = Some((dequeued, Instant::now()));
            Ok(filled)
        })?;
        match (filled, times) {
            (Some(Ok(true)), Some((dequeued, decode_end))) => {
                let captured = capture.captured().unwrap_or_else(|| {
                    if !warned {
                        warned = true;
                        eprintln!("the webcam gives no capture times, so its part counts as 0");
                    }
                    dequeued
                });
                let sent = Instant::now();
                in_flight.lock().unwrap()[k] = Some(Queued {
                    captured,
                    dequeued,
                    decoded: decode_end,
                    sent,
                });
                tokens.write_all(&[u8::try_from(k)?])?;
                slots.pop_front();
                pace.sent(sent, decode_end - dequeued);
            }
            (Some(Err(e)), _) => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

fn collect(
    mut done_tokens: File,
    in_flight: &InFlight,
    free: &Sender<(usize, Option<Instant>)>,
    seconds: u64,
) -> Result<Vec<Frame>> {
    let from = Instant::now() + SETTLE;
    let until = from + Duration::from_secs(seconds);
    let mut frames = Vec::new();
    let mut last_done: Option<Instant> = None;
    let mut token = [0u8];
    loop {
        if done_tokens.read(&mut token)? == 0 {
            bail!("the worker exited");
        }
        let done = Instant::now();
        let k = usize::from(token[0]);
        let sent = in_flight
            .lock()
            .unwrap()
            .get_mut(k)
            .and_then(Option::take)
            .context("the worker sent a bad frame slot")?;
        let started = last_done.map_or(sent.sent, |last| last.max(sent.sent));
        last_done = Some(done);
        free.send((k, Some(done)))?;
        if done > until {
            return Ok(frames);
        }
        if done >= from {
            frames.push(Frame {
                captured: sent.captured,
                dequeued: sent.dequeued,
                decoded: sent.decoded,
                sent: sent.sent,
                started,
                done,
            });
        }
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn report(name: &str, frames: &[Frame]) -> Result<()> {
    let (Some(first), Some(last)) = (frames.first(), frames.last()) else {
        bail!("no frames measured");
    };
    if frames.len() < 2 {
        bail!("fewer than 2 frames measured");
    }
    let count = f64::from(u32::try_from(frames.len())?);
    let mean =
        |step: fn(&Frame) -> Duration| frames.iter().map(|f| ms(step(f))).sum::<f64>() / count;
    let mut delays: Vec<Duration> = frames.iter().map(|f| f.done - f.captured).collect();
    delays.sort_unstable();
    let at = |percent: usize| ms(delays[(delays.len() - 1) * percent / 100]);
    let fps = (count - 1.0) / (last.done - first.done).as_secs_f64();
    println!(
        "{name}: {fps:.1} fps, delay mean {:.1} ms, p50 {:.1}, p95 {:.1}, max {:.1}",
        mean(|f| f.done - f.captured),
        at(50),
        at(95),
        at(100)
    );
    println!(
        "     webcam {:.1} + decode {:.1} + wait for worker {:.1} + effects {:.1}",
        mean(|f| f.dequeued - f.captured),
        mean(|f| f.decoded - f.dequeued),
        mean(|f| f.started - f.sent),
        mean(|f| f.done - f.started)
    );
    Ok(())
}

fn write_csv(path: &Path, frames: &[Frame]) -> Result<()> {
    let Some(first) = frames.first() else {
        return Ok(());
    };
    let origin = first.captured;
    let mut csv = io::BufWriter::new(File::create(path)?);
    writeln!(
        csv,
        "captured_ms,dequeued_ms,decoded_ms,sent_ms,started_ms,done_ms"
    )?;
    for f in frames {
        let times = [f.captured, f.dequeued, f.decoded, f.sent, f.started, f.done]
            .map(|at| format!("{:.3}", ms(at.saturating_duration_since(origin))));
        writeln!(csv, "{}", times.join(","))?;
    }
    csv.flush()?;
    println!("wrote {}", path.display());
    Ok(())
}
