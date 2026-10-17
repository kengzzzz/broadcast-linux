//! The File Processor: ffmpeg decodes and encodes, and the live workers run the effects.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

use crate::audio::{self, FRAME, RATE};
use crate::camera::{self, expand_home};
use crate::config::{CameraConfig, MicConfig};
use crate::nvidia::Installation;
use crate::paths::Paths;
use crate::progress::Progress;
use crate::worker::{Launch, Pipes, Worker};

pub const UPSCALE_HEIGHTS: [u32; 3] = [1080, 1440, 2160];

/// For file pickers. Anything ffmpeg reads works.
pub const EXTENSIONS: [&str; 14] = [
    "mp4", "mov", "mkv", "webm", "m4v", "avi", "wav", "mp3", "m4a", "aac", "flac", "ogg", "opus",
    "wma",
];

const POLL: Duration = Duration::from_millis(100);

/// Only the effect settings count. Sizes and frame rates come from each file.
#[derive(Clone, Debug, PartialEq)]
pub struct Effects {
    pub audio: MicConfig,
    pub video: CameraConfig,
    /// Super Resolution's output height. The width keeps the shape.
    pub upscale_height: u32,
}

impl Default for Effects {
    /// Every effect off.
    fn default() -> Self {
        let mut audio = MicConfig::default();
        audio.noise_removal.enabled = false;
        Self {
            audio,
            video: CameraConfig::default(),
            upscale_height: UPSCALE_HEIGHTS[0],
        }
    }
}

impl Effects {
    pub fn audio_on(&self) -> bool {
        !self.audio.stages().is_empty()
    }

    pub fn video_on(&self) -> bool {
        self.video.has_effects()
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.audio_on() || self.video_on(),
            "turn on at least one effect"
        );
        self.video.validate_effects()?;
        ensure!(
            !self.video.super_resolution.enabled || UPSCALE_HEIGHTS.contains(&self.upscale_height),
            "Super Resolution makes videos 1080, 1440 or 2160 pixels tall"
        );
        Ok(())
    }

    fn audio_delay(&self) -> usize {
        let profiled = self.audio.profile().is_some();
        self.audio
            .stages()
            .first()
            .map_or(0, |stage| stage.effect.delay(profiled))
    }
}

/// Why files can't be processed, or `None` when ffmpeg and ffprobe run.
pub fn ffmpeg_missing() -> Option<&'static str> {
    let runs = |tool: &str| {
        Command::new(tool)
            .arg("-version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    (!runs("ffmpeg") || !runs("ffprobe"))
        .then_some("processing files needs ffmpeg; install it with your package manager")
}

/// Returns the new file, in `output_dir` or else beside `input`. It never replaces a file.
pub fn process(
    paths: &Paths,
    input: &Path,
    effects: &Effects,
    output_dir: Option<&Path>,
    progress: &dyn Progress,
) -> Result<PathBuf> {
    effects.validate()?;
    let media = probe(input)?;
    let video = media.video.as_ref().filter(|_| effects.video_on());
    let audio = media.audio.as_ref().filter(|_| effects.audio_on());
    if video.is_none() && audio.is_none() {
        bail!(match (&media.video, &media.audio) {
            _ if media.picture => "it is a picture, not a video or audio file",
            (None, None) => "it has no video or audio",
            (None, Some(_)) => "it has no video, and only video effects are on",
            (Some(_), _) => "it has no audio, and only audio effects are on",
        });
    }
    if let Some(video) = video.filter(|_| effects.video.super_resolution.enabled) {
        ensure!(
            effects.upscale_height > video.height,
            "Super Resolution only makes videos larger, and this one is already {} pixels tall",
            video.height
        );
    }
    let container = Container::choose(input, media.video.is_some(), video.is_some());
    let dir = output_dir
        .or_else(|| input.parent())
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let stem = input
        .file_stem()
        .map_or_else(|| "output".into(), |s| s.to_string_lossy().into_owned());
    let job = Job {
        paths,
        install: Installation::find(paths)?,
        input,
        media: &media,
        effects,
        progress,
        temp: Temp::new(dir, &stem),
    };

    let output = job.temp.file("part");
    let processed_audio = match audio {
        Some(audio) if media.video.is_some() => {
            let file = job.temp.file("audio.part");
            job.audio_pass(audio, &file, &aac(audio), "ipod")?;
            Some(file)
        }
        Some(audio) => {
            job.audio_pass(
                audio,
                &output,
                &container.audio_codec(audio),
                container.muxer(),
            )?;
            None
        }
        None => None,
    };
    if let Some(video) = video {
        job.video_pass(video, processed_audio.as_deref(), &output)?;
    } else if let Some(audio) = &processed_audio {
        job.remux(audio, container, &output)?;
    }

    progress.check()?;
    publish(&output, dir, &stem, container.ext())
}

/// Moves `output` to the first free `STEM-broadcast[-N].EXT`, also when other jobs race for it.
fn publish(output: &Path, dir: &Path, stem: &str, ext: &str) -> Result<PathBuf> {
    for n in 1..10_000 {
        let name = dir.join(match n {
            1 => format!("{stem}-broadcast.{ext}"),
            n => format!("{stem}-broadcast-{n}.{ext}"),
        });
        if fs::symlink_metadata(&name).is_ok() {
            continue;
        }
        match rename_new(output, &name) {
            Ok(()) => return Ok(name),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("writing {}", name.display())),
        }
    }
    bail!("no free file name for the output in {}", dir.display())
}

/// Fails if `to` exists. Without `RENAME_NOREPLACE` support it does a plain rename, so the
/// caller checks first.
fn rename_new(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |path: &Path| std::ffi::CString::new(path.as_os_str().as_bytes());
    let (from_c, to_c) = (c(from)?, c(to)?);
    // SAFETY: both are valid NUL-terminated paths for the duration of the call.
    let status = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from_c.as_ptr(),
            libc::AT_FDCWD,
            to_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if status == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EINVAL | libc::ENOSYS) => fs::rename(from, to),
        _ => Err(error),
    }
}

struct Job<'a> {
    paths: &'a Paths,
    install: Installation,
    input: &'a Path,
    media: &'a Media,
    effects: &'a Effects,
    progress: &'a dyn Progress,
    temp: Temp,
}

impl Job<'_> {
    /// One worker per channel. Its delay is cut from the start, and silence after the end
    /// brings the last samples out.
    fn audio_pass(
        &self,
        audio: &AudioStream,
        output: &Path,
        codec: &[String],
        muxer: &str,
    ) -> Result<()> {
        self.progress.step("Running the audio effects");
        let channels = audio.channels.clamp(1, 2).to_string();
        let mut decoder = Tool::spawn(
            "ffmpeg (decoding audio)",
            ffmpeg()
                .arg("-i")
                .arg(self.input)
                .args(["-map", "0:a:0", "-af", "aresample=async=1:first_pts=0"])
                .args([
                    "-ac",
                    &channels,
                    "-ar",
                    &RATE.to_string(),
                    "-f",
                    "f32le",
                    "-",
                ])
                .stdout(Stdio::piped()),
        )?;
        let mut encoder = Tool::spawn(
            "ffmpeg (encoding audio)",
            ffmpeg()
                .args(["-f", "f32le", "-ar", &RATE.to_string(), "-ac", &channels])
                .args(["-i", "-"])
                .args(codec)
                .args(["-f", muxer])
                .arg(output)
                .stdin(Stdio::piped()),
        )?;
        let args = audio::worker_args(
            &self.install,
            &self.effects.audio.stages(),
            self.effects.audio.profile(),
            None,
        )?;
        let (mut workers, mut inputs, mut outputs, mut logs) = (vec![], vec![], vec![], vec![]);
        for _ in 0..audio.channels.clamp(1, 2) {
            let (worker, pipes) = Worker::spawn(Launch {
                program: "afx_stream",
                prefix: self.paths.prefix(),
                cwd: self.install.runtime.clone(),
                args: args.clone(),
                stdin: None,
            })?;
            workers.push(worker);
            inputs.push(pipes.stdin.context("no input pipe to the audio worker")?);
            outputs.push(pipes.stdout);
            logs.push(Log::watch(pipes.stderr, |_| {}));
        }

        let delay = self.effects.audio_delay();
        let length = Arc::new(OnceLock::new());
        let done = Arc::new(AtomicU64::new(0));
        let source = decoder.stdout()?;
        let sink = encoder
            .child
            .stdin
            .take()
            .context("no input pipe to ffmpeg")?;
        let split = thread::spawn({
            let length = Arc::clone(&length);
            move || split(source, inputs, &length, delay)
        });
        let join = thread::spawn({
            let (length, done) = (Arc::clone(&length), Arc::clone(&done));
            move || join(outputs, sink, &length, delay, &done)
        });
        let total = self.media.duration_ms;
        let mut tools = vec![decoder, encoder];
        self.wait(
            || split.is_finished() && join.is_finished() && tools_done(&mut tools, &mut workers),
            || done.load(Ordering::Relaxed) * 1000 / u64::from(RATE),
            total,
        )?;
        let split = split.join().unwrap_or(Ok(()));
        let joined = join.join().unwrap_or(Ok(0));
        let statuses = wait_tools(&mut tools, &mut workers)?;
        // A worker exits cleanly when ffmpeg stops reading, so a failed one is the cause.
        for (status, log) in statuses.workers.iter().zip(&logs) {
            if !status.success() {
                bail!(log.worker_failure("audio"));
            }
        }
        for (status, tool) in statuses.tools.iter().zip(&tools).rev() {
            if !status.success() {
                bail!(tool.failure());
            }
        }
        split.context("feeding the audio worker")?;
        let emitted = joined.context("reading the audio worker")?;
        let expected = length.get().copied().unwrap_or_default();
        ensure!(
            emitted == expected,
            "the audio worker returned {emitted} of {expected} samples"
        );
        Ok(())
    }

    fn video_pass(&self, video: &VideoStream, audio: Option<&Path>, output: &Path) -> Result<()> {
        self.progress.step("Running the video effects");
        let out = if self.effects.video.super_resolution.enabled {
            let target = self.effects.upscale_height;
            let width = u64::from(video.width) * u64::from(target) / u64::from(video.height);
            (even(width), target)
        } else {
            (video.width, video.height)
        };
        let mut decoder = self.video_decoder(video)?;
        let (worker, pipes) = self.camera_worker(video, out, decoder.stdout()?)?;
        let frames = Arc::new(AtomicU64::new(0));
        let log = Log::watch(pipes.stderr, {
            let frames = Arc::clone(&frames);
            move |line| {
                // "Processed <n> frames in <s> s (<fps> fps)"
                if let Some(n) = line
                    .strip_prefix("Processed ")
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|n| n.parse().ok())
                {
                    frames.store(n, Ordering::Relaxed);
                }
            }
        });
        let encoder = self.video_encoder(video, out, audio, pipes.stdout, output)?;

        let (num, den) = video.rate;
        let mut tools = vec![decoder, encoder];
        let mut workers = vec![worker];
        self.wait(
            || tools_done(&mut tools, &mut workers),
            || frames.load(Ordering::Relaxed) * 1000 * u64::from(den) / u64::from(num),
            self.media.duration_ms,
        )?;
        let statuses = wait_tools(&mut tools, &mut workers)?;
        let worker_failed = !statuses.workers[0].success();
        let failure = || log.worker_failure("camera");
        // When the encoder stops reading, the worker fails too, but the encoder's error is
        // the useful one.
        if worker_failed && !log.last().starts_with("Output pipe closed") {
            bail!(failure());
        }
        for (status, tool) in statuses.tools.iter().zip(&tools).rev() {
            if !status.success() {
                bail!(tool.failure());
            }
        }
        if worker_failed {
            bail!(failure());
        }
        Ok(())
    }

    /// Constant frame rate from the file's start, so the video lines up with the audio.
    fn video_decoder(&self, video: &VideoStream) -> Result<Tool> {
        let (width, height) = (video.width, video.height);
        let mut scale = format!("scale={width}:{height}:flags=bicubic");
        if video.colors.matrix.is_none() {
            scale += if video.bt601() {
                ":in_color_matrix=bt601"
            } else {
                ":in_color_matrix=bt709"
            };
        }
        let (num, den) = video.rate;
        Tool::spawn(
            "ffmpeg (decoding video)",
            ffmpeg()
                .arg("-i")
                .arg(self.input)
                .args([
                    "-map",
                    "0:V:0",
                    "-vf",
                    &format!("{scale},format=bgr24,setsar=1"),
                ])
                .args(["-fps_mode", "cfr", "-r", &format!("{num}/{den}")])
                .args(["-f", "rawvideo", "-pix_fmt", "bgr24", "-"])
                .stdout(Stdio::piped()),
        )
    }

    fn camera_worker(
        &self,
        video: &VideoStream,
        out: (u32, u32),
        frames: ChildStdout,
    ) -> Result<(Worker, Pipes)> {
        let config = &self.effects.video;
        let (width, height) = (video.width, video.height);
        let background = match &config.background {
            Some(image) => {
                let file = self.temp.file("background.bgr");
                let bgr = camera::background_bgr(&expand_home(image), width, height)?;
                fs::write(&file, bgr)?;
                Some(file)
            }
            None => None,
        };
        let mut args = camera::effect_args(
            config,
            &self.install,
            (width, height),
            out,
            background.as_deref(),
        )?;
        let (num, den) = video.rate;
        args.extend([
            "--input".into(),
            "bgr24".into(),
            "--out-color".into(),
            if video.bt601() { "601" } else { "709" }.into(),
            "--frame-ms".into(),
            (1000.0 * f64::from(den) / f64::from(num)).to_string(),
        ]);
        Worker::spawn(Launch {
            program: "camera_stream",
            prefix: self.paths.prefix(),
            cwd: self.install.runtime.clone(),
            args,
            stdin: Some(Stdio::from(frames)),
        })
    }

    /// Adds the processed `audio`, else the file's own.
    fn video_encoder(
        &self,
        video: &VideoStream,
        (width, height): (u32, u32),
        audio: Option<&Path>,
        frames: ChildStdout,
        output: &Path,
    ) -> Result<Tool> {
        let (num, den) = video.rate;
        let rate = u64::from(num) * u64::from(self.effects.video.frame_multiplier());
        let mut encoder = ffmpeg();
        encoder
            .args(["-f", "rawvideo", "-pix_fmt", "yuyv422"])
            .args(["-video_size", &format!("{width}x{height}")])
            .args(["-framerate", &format!("{rate}/{den}"), "-i", "-"]);
        match (audio, &self.media.audio) {
            (Some(file), _) => {
                encoder.arg("-i").arg(file).args(["-c:a", "copy"]);
            }
            (None, Some(original)) => {
                encoder.arg("-i").arg(self.input);
                if matches!(original.codec.as_str(), "aac" | "mp3") {
                    encoder.args(["-c:a", "copy"]);
                } else {
                    encoder.args(aac(original));
                }
            }
            (None, None) => {}
        }
        encoder.args(["-map", "0:v:0"]);
        if audio.is_some() || self.media.audio.is_some() {
            encoder.args(["-map", "1:a:0"]);
        }
        let [matrix, primaries, transfer] = video.output_colors(video.bt601());
        // Newer ffmpeg takes the tags from the frames, older from the options.
        let mut filters = format!(
            "setparams=colorspace={matrix}:color_primaries={primaries}:color_trc={transfer}:range=tv"
        );
        // Frames are only made between inputs, so the last one is held for M - 1 more.
        let multiplier = self.effects.video.frame_multiplier();
        if multiplier > 1 {
            let _ = write!(filters, ",tpad=stop_mode=clone:stop={}", multiplier - 1);
        }
        encoder
            .args(Encoder::find()?.args((width, height), (rate, den)))
            .args(["-vf", &filters, "-pix_fmt", "yuv420p", "-color_range", "tv"])
            .args(["-colorspace", matrix, "-color_primaries", primaries])
            .args([
                "-color_trc",
                transfer,
                "-movflags",
                "+faststart",
                "-f",
                "mp4",
            ])
            .arg(output)
            .stdin(Stdio::from(frames));
        Tool::spawn("ffmpeg (encoding video)", &mut encoder)
    }

    fn remux(&self, audio: &Path, container: Container, output: &Path) -> Result<()> {
        self.progress.step("Saving the new audio with the video");
        let mut command = ffmpeg();
        command
            .arg("-i")
            .arg(self.input)
            .arg("-i")
            .arg(audio)
            .args(["-map", "0:V:0", "-map", "1:a:0", "-c", "copy"]);
        if matches!(container, Container::Mp4 | Container::Mov) {
            command.args(["-movflags", "+faststart"]);
        }
        command.args(["-f", container.muxer()]).arg(output);
        let mut tools = vec![Tool::spawn("ffmpeg (saving)", &mut command)?];
        self.wait(|| tools_done(&mut tools, &mut []), || 0, 0)?;
        let statuses = wait_tools(&mut tools, &mut [])?;
        if !statuses.tools[0].success() {
            bail!(tools[0].failure());
        }
        Ok(())
    }

    /// Polls until `finished`. `done` and `total` are milliseconds. Fails with `Cancelled`.
    fn wait(
        &self,
        mut finished: impl FnMut() -> bool,
        done: impl Fn() -> u64,
        total: u64,
    ) -> Result<()> {
        loop {
            self.progress.check()?;
            if total > 0 {
                self.progress.advance(done().min(total), total);
            }
            if finished() {
                return Ok(());
            }
            thread::sleep(POLL);
        }
    }
}

/// Ends with the delay plus a frame of silence, since workers only return whole frames.
fn split(
    mut source: ChildStdout,
    mut inputs: Vec<ChildStdin>,
    length: &OnceLock<u64>,
    delay: usize,
) -> io::Result<()> {
    let channels = inputs.len();
    let mut chunk = vec![0u8; FRAME * channels * 4];
    let mut lanes = vec![vec![0u8; FRAME * 4]; channels];
    let mut samples = 0u64;
    loop {
        let n = read_full(&mut source, &mut chunk)?;
        let whole = n / (channels * 4);
        for (c, lane) in lanes.iter_mut().enumerate() {
            for i in 0..whole {
                let at = (i * channels + c) * 4;
                lane[i * 4..i * 4 + 4].copy_from_slice(&chunk[at..at + 4]);
            }
        }
        // A chunk per worker in turn, so their outputs stay in step for `join`.
        for (input, lane) in inputs.iter_mut().zip(&lanes) {
            input.write_all(&lane[..whole * 4])?;
        }
        samples += whole as u64;
        if n < chunk.len() {
            break;
        }
    }
    // Set before the silence goes in, so `join` knows the length before it sees any.
    let _ = length.set(samples);
    let silence = [0u8; FRAME * 4];
    let mut left = (delay + FRAME) * 4;
    while left > 0 {
        let n = left.min(silence.len());
        for input in &mut inputs {
            input.write_all(&silence[..n])?;
        }
        left -= n;
    }
    Ok(())
}

/// Drops the delay and the padding. Returns the samples written per channel.
fn join(
    mut outputs: Vec<ChildStdout>,
    mut sink: ChildStdin,
    length: &OnceLock<u64>,
    delay: usize,
    done: &AtomicU64,
) -> io::Result<u64> {
    let channels = outputs.len();
    let mut lanes = vec![vec![0u8; FRAME * 4]; channels];
    let mut mixed = vec![0u8; FRAME * channels * 4];
    let (mut position, mut written) = (0u64, 0u64);
    'frames: loop {
        for (output, lane) in outputs.iter_mut().zip(&mut lanes) {
            if read_full(output, lane)? < lane.len() {
                break 'frames;
            }
        }
        let (from, to) = kept(position, delay, length.get().copied());
        position += FRAME as u64;
        for i in from..to {
            for (c, lane) in lanes.iter().enumerate() {
                let at = ((i - from) * channels + c) * 4;
                mixed[at..at + 4].copy_from_slice(&lane[i * 4..i * 4 + 4]);
            }
        }
        if to > from {
            sink.write_all(&mixed[..(to - from) * channels * 4])?;
            written += (to - from) as u64;
            done.store(written, Ordering::Relaxed);
        }
    }
    Ok(written)
}

/// The input's part of the output frame at `position`. Output sample `delay + i` is input
/// sample `i`, of `length` once known.
fn kept(position: u64, delay: usize, length: Option<u64>) -> (usize, usize) {
    let frame_end = position + FRAME as u64;
    let start = (delay as u64).clamp(position, frame_end);
    let end = length.map_or(frame_end, |length| {
        (delay as u64 + length).clamp(start, frame_end)
    });
    let at = |sample: u64| usize::try_from(sample - position).unwrap_or(0);
    (at(start), at(end))
}

/// Returns less than `buf.len()` only at the end.
fn read_full(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

fn ffmpeg() -> Command {
    let mut command = Command::new("ffmpeg");
    command.args(["-nostdin", "-hide_banner", "-v", "error", "-y"]);
    command
}

fn aac(audio: &AudioStream) -> Vec<String> {
    let rate = 96 * audio.channels.clamp(1, 2);
    ["-c:a", "aac", "-b:a", &format!("{rate}k")]
        .map(String::from)
        .to_vec()
}

struct Tool {
    what: &'static str,
    child: Child,
    log: Log,
}

impl Tool {
    fn spawn(what: &'static str, command: &mut Command) -> Result<Self> {
        let mut child = command
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| match e.kind() {
                ErrorKind::NotFound => anyhow::anyhow!(
                    "processing files needs ffmpeg; install it with your package manager"
                ),
                _ => anyhow::Error::new(e).context("starting ffmpeg"),
            })?;
        let stderr = child.stderr.take().context("no ffmpeg error pipe")?;
        Ok(Self {
            what,
            child,
            log: Log::watch(stderr, |_| {}),
        })
    }

    fn stdout(&mut self) -> Result<ChildStdout> {
        self.child.stdout.take().context("no ffmpeg output pipe")
    }

    fn failure(&self) -> String {
        match self.log.last() {
            line if line.is_empty() => format!("{} failed", self.what),
            line => format!("{} failed: {line}", self.what),
        }
    }
}

impl Drop for Tool {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// On its own thread, so a full stderr pipe never stalls the child.
struct Log {
    last: Arc<Mutex<String>>,
    thread: JoinHandle<()>,
}

impl Log {
    fn watch(
        stderr: impl Read + Send + 'static,
        mut each: impl FnMut(&str) + Send + 'static,
    ) -> Self {
        let last = Arc::new(Mutex::new(String::new()));
        let thread = thread::spawn({
            let last = Arc::clone(&last);
            move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    each(&line);
                    let mut last = last.lock().unwrap();
                    // A usage text says more by its first line than its last.
                    if !line.trim().is_empty()
                        && !line.starts_with("Processed ")
                        && !last.starts_with("Usage:")
                    {
                        *last = line;
                    }
                }
            }
        });
        Self { last, thread }
    }

    /// Waits up to a second for the reader to finish, in case another process holds the pipe.
    fn last(&self) -> String {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !self.thread.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        self.last.lock().unwrap().clone()
    }

    fn worker_failure(&self, kind: &str) -> String {
        let last = self.last();
        if last.starts_with("Usage:") || last.is_empty() {
            format!(
                "the {kind} worker didn't start; the workers in {} may be from another version",
                crate::paths::tilde(&crate::paths::workers_dir())
            )
        } else {
            format!("the {kind} worker failed: {last}")
        }
    }
}

struct Statuses {
    tools: Vec<ExitStatus>,
    workers: Vec<ExitStatus>,
}

fn tools_done(tools: &mut [Tool], workers: &mut [Worker]) -> bool {
    tools
        .iter_mut()
        .all(|tool| matches!(tool.child.try_wait(), Ok(Some(_))))
        && workers
            .iter_mut()
            .all(|worker| matches!(worker.try_wait(), Ok(Some(_))))
}

/// Gives every process 10 s to exit.
fn wait_tools(tools: &mut [Tool], workers: &mut [Worker]) -> Result<Statuses> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let tool_statuses: Vec<Option<ExitStatus>> = tools
            .iter_mut()
            .map(|tool| tool.child.try_wait())
            .collect::<io::Result<_>>()?;
        let worker_statuses: Vec<Option<ExitStatus>> = workers
            .iter_mut()
            .map(Worker::try_wait)
            .collect::<Result<_>>()?;
        let all = tool_statuses.iter().chain(&worker_statuses);
        if all.clone().all(Option::is_some) {
            return Ok(Statuses {
                tools: tool_statuses.into_iter().flatten().collect(),
                workers: worker_statuses.into_iter().flatten().collect(),
            });
        }
        if Instant::now() >= deadline {
            bail!("ffmpeg or a worker did not finish");
        }
        thread::sleep(POLL);
    }
}

struct Temp {
    base: PathBuf,
    files: Mutex<Vec<PathBuf>>,
}

impl Temp {
    fn new(dir: &Path, stem: &str) -> Self {
        Self {
            base: dir.join(format!(".{stem}-broadcast-{}", std::process::id())),
            files: Mutex::default(),
        }
    }

    fn file(&self, suffix: &str) -> PathBuf {
        let mut path = self.base.clone().into_os_string();
        path.push(format!(".{suffix}"));
        let path = PathBuf::from(path);
        self.files.lock().unwrap().push(path.clone());
        path
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        for file in self.files.get_mut().unwrap().iter() {
            let _ = fs::remove_file(file);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Container {
    Mp4,
    Mov,
    Mkv,
    M4a,
    Wav,
    Flac,
}

impl Container {
    /// Video effects make MP4. New audio keeps an MP4 or MOV, others become MKV. WAV and FLAC
    /// stay, other audio becomes M4A.
    fn choose(input: &Path, has_video: bool, video_effects: bool) -> Self {
        let ext = input
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        match (has_video, video_effects, ext.as_str()) {
            (true, true, _) | (true, false, "mp4" | "m4v") => Self::Mp4,
            (true, false, "mov") => Self::Mov,
            (true, false, _) => Self::Mkv,
            (false, _, "wav") => Self::Wav,
            (false, _, "flac") => Self::Flac,
            (false, _, _) => Self::M4a,
        }
    }

    fn ext(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mov => "mov",
            Self::Mkv => "mkv",
            Self::M4a => "m4a",
            Self::Wav => "wav",
            Self::Flac => "flac",
        }
    }

    fn muxer(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mov => "mov",
            Self::Mkv => "matroska",
            Self::M4a => "ipod",
            Self::Wav => "wav",
            Self::Flac => "flac",
        }
    }

    fn audio_codec(self, audio: &AudioStream) -> Vec<String> {
        let codec = match self {
            Self::Wav if audio.codec.starts_with("pcm_") => audio.codec.as_str(),
            Self::Wav => "pcm_s16le",
            Self::Flac => "flac",
            _ => return aac(audio),
        };
        let mut args = vec!["-c:a".into(), codec.into()];
        if self == Self::Wav {
            // Over 4 GiB, a plain WAV header can't hold the size.
            args.extend(["-rf64".into(), "auto".into()]);
        }
        args
    }
}

/// The H.264 encoders tried, best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoder {
    X264,
    Nvenc,
    OpenH264,
}

impl Encoder {
    /// The first that works. Only a success is cached.
    fn find() -> Result<Self> {
        static FOUND: Mutex<Option<Encoder>> = Mutex::new(None);
        let mut found = FOUND.lock().unwrap();
        if found.is_none() {
            *found = [Self::X264, Self::Nvenc, Self::OpenH264]
                .into_iter()
                .find(|encoder| encoder.works());
        }
        found.context("ffmpeg has no working H.264 encoder (libx264, h264_nvenc or libopenh264)")
    }

    fn name(self) -> &'static str {
        match self {
            Self::X264 => "libx264",
            Self::Nvenc => "h264_nvenc",
            Self::OpenH264 => "libopenh264",
        }
    }

    /// Some builds list encoders that fail to start.
    fn works(self) -> bool {
        ffmpeg()
            .args([
                "-f",
                "lavfi",
                "-i",
                "color=size=320x240:rate=30:duration=0.1",
            ])
            .args([
                "-c:v",
                self.name(),
                "-pix_fmt",
                "yuv420p",
                "-f",
                "null",
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn args(self, (width, height): (u32, u32), (num, den): (u64, u32)) -> Vec<String> {
        let args: &[&str] = match self {
            Self::X264 => &["-c:v", "libx264", "-preset", "medium", "-crf", "18"],
            Self::Nvenc => &[
                "-c:v",
                "h264_nvenc",
                "-preset",
                "p5",
                "-tune",
                "hq",
                "-rc",
                "vbr",
                "-cq",
                "19",
                "-b:v",
                "0",
            ],
            Self::OpenH264 => &["-c:v", "libopenh264"],
        };
        let mut args: Vec<String> = args.iter().map(|&a| a.to_owned()).collect();
        if self == Self::OpenH264 {
            // It has no quality mode: about 0.15 bits per pixel.
            let bits = u64::from(width) * u64::from(height) * num / u64::from(den.max(1)) * 3 / 20;
            args.extend(["-b:v".into(), format!("{}k", bits / 1000)]);
        }
        args
    }
}

struct Media {
    duration_ms: u64,
    picture: bool,
    video: Option<VideoStream>,
    audio: Option<AudioStream>,
}

#[derive(Debug, PartialEq, Eq)]
struct VideoStream {
    /// The shown size, after the pixel shape and rotation, rounded down to even numbers.
    width: u32,
    height: u32,
    rate: (u32, u32),
    colors: Colors,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Colors {
    matrix: Option<String>,
    primaries: Option<String>,
    transfer: Option<String>,
}

impl VideoStream {
    /// SD video without color tags is taken as BT.601, like players do.
    fn bt601(&self) -> bool {
        match self.colors.matrix.as_deref() {
            Some(matrix) => matches!(matrix, "smpte170m" | "bt470bg"),
            None => self.height <= 576,
        }
    }

    /// Matrix, primaries and transfer. The worker only changes the matrix. Names `setparams`
    /// doesn't know fall back to the matrix's.
    fn output_colors(&self, bt601: bool) -> [&str; 3] {
        const PRIMARIES: [&str; 10] = [
            "bt709",
            "bt470m",
            "bt470bg",
            "smpte170m",
            "smpte240m",
            "film",
            "bt2020",
            "smpte428",
            "smpte431",
            "smpte432",
        ];
        const TRANSFERS: [&str; 9] = [
            "bt709",
            "smpte170m",
            "smpte240m",
            "linear",
            "iec61966-2-1",
            "bt2020-10",
            "bt2020-12",
            "smpte2084",
            "arib-std-b67",
        ];
        let default = if bt601 { "smpte170m" } else { "bt709" };
        let known = |tag: &Option<String>, names: &[&'static str]| {
            names
                .iter()
                .copied()
                .find(|&name| tag.as_deref() == Some(name))
                .unwrap_or(default)
        };
        [
            default,
            known(&self.colors.primaries, &PRIMARIES),
            known(&self.colors.transfer, &TRANSFERS),
        ]
    }
}

#[derive(Debug, PartialEq, Eq)]
struct AudioStream {
    codec: String,
    channels: u32,
}

fn probe(input: &Path) -> Result<Media> {
    ensure!(input.exists(), "it doesn't exist");
    ensure!(input.is_file(), "it is not a file");
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(input)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| match e.kind() {
            ErrorKind::NotFound => anyhow::anyhow!(
                "processing files needs ffmpeg; install it with your package manager"
            ),
            _ => anyhow::Error::new(e).context("running ffprobe"),
        })?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        bail!(
            "ffmpeg can't read it: {}",
            error.lines().last().unwrap_or_default().trim()
        );
    }
    parse_probe(&serde_json::from_slice(&output.stdout).context("reading ffprobe's output")?)
}

fn parse_probe(json: &Value) -> Result<Media> {
    let format = &json["format"];
    let format_name = format["format_name"].as_str().unwrap_or_default();
    let streams = json["streams"].as_array().map_or(&[][..], Vec::as_slice);
    let picture = format_name == "image2" || format_name.ends_with("_pipe");
    let video = streams
        .iter()
        .filter(|_| !picture)
        .find(|s| s["codec_type"] == "video" && s["disposition"]["attached_pic"] != 1)
        .map(video_stream)
        .transpose()?;
    let audio = streams
        .iter()
        .find(|s| s["codec_type"] == "audio")
        .map(|s| AudioStream {
            codec: s["codec_name"].as_str().unwrap_or_default().to_owned(),
            channels: s["channels"]
                .as_u64()
                .and_then(|c| u32::try_from(c).ok())
                .unwrap_or(2),
        });
    let seconds = |value: &Value| value.as_str().and_then(|s| s.parse::<f64>().ok());
    let duration = seconds(&format["duration"])
        .or_else(|| streams.iter().find_map(|s| seconds(&s["duration"])))
        .unwrap_or_default();
    Ok(Media {
        duration_ms: whole(duration * 1000.0),
        picture,
        video,
        audio,
    })
}

fn video_stream(stream: &Value) -> Result<VideoStream> {
    let size = |key: &str| stream[key].as_u64().filter(|&n| n > 0);
    let (Some(mut width), Some(height)) = (size("width"), size("height")) else {
        bail!("its video has no size");
    };
    if let Some((num, den)) = ratio(stream["sample_aspect_ratio"].as_str(), ':') {
        width = width * u64::from(num) / u64::from(den);
    }
    let rotation = stream["side_data_list"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|data| data["rotation"].as_f64())
        .or_else(|| stream["tags"]["rotate"].as_str()?.parse().ok())
        .unwrap_or_default();
    let (width, height) = if (rotation.abs() % 180.0 - 90.0).abs() < 1.0 {
        (height, width)
    } else {
        (width, height)
    };
    let rate = ratio(stream["avg_frame_rate"].as_str(), '/')
        .or_else(|| ratio(stream["r_frame_rate"].as_str(), '/'))
        .map_or((30, 1), snap_rate);
    let tag = |key: &str| {
        stream[key]
            .as_str()
            .filter(|v| !matches!(*v, "unknown" | "reserved" | "unspecified" | ""))
            .map(str::to_owned)
    };
    Ok(VideoStream {
        width: even(width),
        height: even(height),
        rate,
        colors: Colors {
            matrix: tag("color_space"),
            primaries: tag("color_primaries"),
            transfer: tag("color_transfer"),
        },
    })
}

/// `"30000/1001"` as numbers, when both are positive.
fn ratio(text: Option<&str>, separator: char) -> Option<(u32, u32)> {
    let (num, den) = text?.split_once(separator)?;
    let (num, den) = (num.trim().parse().ok()?, den.trim().parse().ok()?);
    (num > 0 && den > 0).then_some((num, den))
}

/// Snaps to a usual rate within 0.5%, so a phone's variable 30.09 fps becomes 30.
fn snap_rate((num, den): (u32, u32)) -> (u32, u32) {
    const USUAL: [(u32, u32); 14] = [
        (24000, 1001),
        (24, 1),
        (25, 1),
        (30000, 1001),
        (30, 1),
        (48, 1),
        (50, 1),
        (60000, 1001),
        (60, 1),
        (90, 1),
        (100, 1),
        (120, 1),
        (144, 1),
        (240, 1),
    ];
    let fps = |(num, den): (u32, u32)| f64::from(num) / f64::from(den);
    let off = |usual| (fps(usual) / fps((num, den)) - 1.0).abs();
    USUAL
        .into_iter()
        .filter(|&usual| off(usual) < 0.005)
        .min_by(|&a, &b| off(a).total_cmp(&off(b)))
        .unwrap_or((num, den))
}

/// Rounded down to an even number, at least 2.
fn even(n: u64) -> u32 {
    u32::try_from(n / 2 * 2).unwrap_or(u32::MAX - 1).max(2)
}

/// `x` rounded, with NaN and negatives as 0.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn whole(x: f64) -> u64 {
    x.max(0.0).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(json: &str) -> VideoStream {
        let media = parse_probe(&serde_json::from_str(json).unwrap()).unwrap();
        media.video.unwrap()
    }

    #[test]
    fn reads_the_shown_size_and_rate() {
        let phone = video(
            r#"{"format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "10.5"},
                "streams": [{"codec_type": "video", "width": 1920, "height": 1080,
                    "avg_frame_rate": "1715/57", "r_frame_rate": "30/1",
                    "color_space": "bt709", "color_primaries": "bt709",
                    "side_data_list": [{"side_data_type": "Display Matrix", "rotation": -90}]}]}"#,
        );
        assert_eq!((phone.width, phone.height), (1080, 1920), "rotated");
        assert_eq!(phone.rate, (30, 1), "30.09 fps snaps to 30");
        assert!(!phone.bt601());
        assert_eq!(phone.output_colors(false), ["bt709", "bt709", "bt709"]);

        let odd = video(
            r#"{"format": {"format_name": "matroska,webm"},
                "streams": [{"codec_type": "video", "width": 641, "height": 361,
                    "sample_aspect_ratio": "1:1", "avg_frame_rate": "0/0",
                    "r_frame_rate": "30000/1001", "color_space": "unknown"}]}"#,
        );
        assert_eq!((odd.width, odd.height), (640, 360), "rounded down to even");
        assert_eq!(odd.rate, (30000, 1001));
        assert!(odd.bt601(), "untagged SD");
        assert_eq!(odd.colors, Colors::default());

        let anamorphic = video(
            r#"{"format": {}, "streams": [{"codec_type": "video", "width": 720,
                "height": 576, "sample_aspect_ratio": "64:45", "avg_frame_rate": "25/1",
                "color_space": "bt470bg", "tags": {"rotate": "180"}}]}"#,
        );
        assert_eq!((anamorphic.width, anamorphic.height), (1024, 576));
        assert_eq!(anamorphic.rate, (25, 1));
        assert_eq!(anamorphic.output_colors(true), ["smpte170m"; 3]);
    }

    #[test]
    fn skips_cover_art_and_pictures() {
        let json = r#"{"format": {"format_name": "mp3", "duration": "3.25"},
            "streams": [{"codec_type": "audio", "codec_name": "mp3", "channels": 1},
                {"codec_type": "video", "width": 500, "height": 500,
                 "disposition": {"attached_pic": 1}}]}"#;
        let media = parse_probe(&serde_json::from_str(json).unwrap()).unwrap();
        assert!(media.video.is_none());
        assert_eq!(media.duration_ms, 3250);
        assert_eq!(
            media.audio,
            Some(AudioStream {
                codec: "mp3".into(),
                channels: 1
            })
        );
        let json = r#"{"format": {"format_name": "png_pipe"},
            "streams": [{"codec_type": "video", "width": 64, "height": 64}]}"#;
        let media = parse_probe(&serde_json::from_str(json).unwrap()).unwrap();
        assert!(media.video.is_none() && media.audio.is_none());
    }

    #[test]
    fn cuts_the_delay_and_the_padding() {
        let frame = FRAME as u64;
        assert_eq!(kept(0, 3360, None), (FRAME, FRAME), "all delay");
        assert_eq!(kept(frame, 3360, None), (1440, FRAME));
        assert_eq!(kept(2 * frame, 3360, None), (0, FRAME));
        // 5000 input samples end at output sample 8360, in the frame from 7680.
        assert_eq!(kept(4 * frame, 3360, Some(5000)), (0, 680));
        assert_eq!(kept(5 * frame, 3360, Some(5000)), (0, 0));
        // A file shorter than the delay.
        assert_eq!(kept(frame, 3360, Some(100)), (1440, 1540));
        assert_eq!(kept(0, 0, Some(10)), (0, 10));
        let total: usize = (0..6)
            .map(|k| kept(k * frame, 1920, Some(7000)))
            .map(|(from, to)| to - from)
            .sum();
        assert_eq!(total, 7000);
    }

    #[test]
    fn never_replaces_a_file() {
        let dir =
            std::env::temp_dir().join(format!("broadcast-linux-publish-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let made = |text: &str| {
            let part = dir.join(".part");
            fs::write(&part, text).unwrap();
            part
        };
        let first = publish(&made("one"), &dir, "clip", "mp4").unwrap();
        let second = publish(&made("two"), &dir, "clip", "mp4").unwrap();
        assert_eq!(first, dir.join("clip-broadcast.mp4"));
        assert_eq!(second, dir.join("clip-broadcast-2.mp4"));
        let taken = rename_new(&made("three"), &first).unwrap_err();
        assert_eq!(taken.kind(), ErrorKind::AlreadyExists, "another job's file");
        assert_eq!(fs::read_to_string(&first).unwrap(), "one");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn picks_the_container() {
        let choose =
            |name: &str, video, effects| Container::choose(Path::new(name), video, effects);
        assert_eq!(choose("a.MOV", true, true), Container::Mp4);
        assert_eq!(choose("a.mov", true, false), Container::Mov);
        assert_eq!(choose("a.webm", true, false), Container::Mkv);
        assert_eq!(choose("a.m4v", true, false), Container::Mp4);
        assert_eq!(choose("a.wav", false, false), Container::Wav);
        assert_eq!(choose("a.flac", false, true), Container::Flac);
        assert_eq!(choose("a.mp3", false, false), Container::M4a);
        assert_eq!(choose("noext", false, false), Container::M4a);
    }

    #[test]
    fn file_effects_start_off() {
        let effects = Effects::default();
        assert!(!effects.audio_on() && !effects.video_on());
        assert!(effects.validate().is_err());
        let mut effects = Effects::default();
        effects.video.frame_generation.enabled = true;
        effects.video.frame_generation.multiplier = 4;
        effects.validate().expect("any frame rate works for files");
        effects.audio.studio_voice.enabled = true;
        assert_eq!(effects.audio_delay(), 1920);
        effects.audio.studio_voice.profile = crate::config::MicProfile::Warm;
        assert_eq!(effects.audio_delay(), 3360);
    }
}
