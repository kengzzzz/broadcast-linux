//! Measures the delay between two WASAPI endpoints, lined up by the device timestamp on
//! every packet so neither stream's buffering counts. Writes the stereo f32 layout that
//! tools/measure/analyze.py reads: left = reference, right = processed.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::{
    collections::HashMap,
    fmt::Write as _,
    fs,
    io::{BufWriter, Write},
    path::PathBuf,
    process::ExitCode,
    ptr, thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use rustfft::{FftPlanner, num_complex::Complex};
use windows::Win32::{
    Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
    Media::Audio::{
        AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
        AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE_ACTIVE, EDataFlow,
        IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
        MMDeviceEnumerator, WAVEFORMATEX, eCapture, eConsole, eRender,
    },
    System::{
        Com::{
            CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
            STGM_READ,
        },
        Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
    },
};

const RATE: u32 = 48_000;
const HNS_PER_FRAME: f64 = 1e7 / RATE as f64;
const BUFFER_HNS: i64 = 5_000_000;
const DISCONTINUITY: u32 = 1;
const SILENT: u32 = 2;
const TIMESTAMP_ERROR: u32 = 4;

const USAGE: &str = "\
winbench list
winbench analyze FILE.f32
winbench measure --ref SPEC --out SPEC --name NAME [--play WAV --play-dev DEVICE]
                 [--seconds 30] [--warmup 8] [--dir results]

SPEC is cap:<part of a capture device name> or loop:<part of a playback device name>
(loopback of what that device plays). DEVICE is part of a playback device name, or
default. Writes DIR/NAME.f32 (stereo, left = ref, right = out), NAME.csv (packets)
and NAME.txt (the summary also printed here). analyze reruns the delay analysis on
a saved NAME.f32.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .map_err(anyhow::Error::from)
        .and_then(|()| match args.first().map(String::as_str) {
            Some("list") => list(),
            Some("measure") => measure(&Options::parse(&args[1..])?),
            Some("analyze") if args.len() == 2 => analyze_file(&args[1]),
            _ => Err(anyhow!("{USAGE}")),
        });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

struct Options {
    reference: String,
    out: String,
    name: String,
    play: Option<PathBuf>,
    play_dev: String,
    seconds: u32,
    warmup: u32,
    dir: PathBuf,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self> {
        let mut map = HashMap::new();
        let mut it = args.iter();
        while let Some(key) = it.next() {
            let key = key
                .strip_prefix("--")
                .ok_or_else(|| anyhow!("unexpected {key:?}\n{USAGE}"))?;
            let value = it.next().ok_or_else(|| anyhow!("--{key} needs a value"))?;
            map.insert(key.to_owned(), value.clone());
        }
        let mut take = |key: &str| map.remove(key);
        let number = |v: Option<String>, default: u32| -> Result<u32> {
            v.map_or(Ok(default), |v| v.parse().context("not a number"))
        };
        let options = Self {
            reference: take("ref").ok_or_else(|| anyhow!("--ref is required\n{USAGE}"))?,
            out: take("out").ok_or_else(|| anyhow!("--out is required\n{USAGE}"))?,
            name: take("name").ok_or_else(|| anyhow!("--name is required\n{USAGE}"))?,
            play: take("play").map(PathBuf::from),
            play_dev: take("play-dev").unwrap_or_else(|| "default".into()),
            seconds: number(take("seconds"), 30)?,
            warmup: number(take("warmup"), 8)?,
            dir: take("dir").map_or_else(|| "results".into(), PathBuf::from),
        };
        if let Some(key) = map.keys().next() {
            bail!("unknown option --{key}\n{USAGE}");
        }
        Ok(options)
    }
}

struct Endpoint {
    device: IMMDevice,
    name: String,
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    Ok(unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? })
}

fn endpoints(en: &IMMDeviceEnumerator, flow: EDataFlow) -> Result<Vec<Endpoint>> {
    unsafe {
        let all = en.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
        (0..all.GetCount()?)
            .map(|i| {
                let device = all.Item(i)?;
                let name = device
                    .OpenPropertyStore(STGM_READ)?
                    .GetValue(&PKEY_Device_FriendlyName)?
                    .to_string();
                Ok(Endpoint { device, name })
            })
            .collect()
    }
}

fn find(en: &IMMDeviceEnumerator, flow: EDataFlow, pattern: &str) -> Result<Endpoint> {
    let kind = if flow == eCapture {
        "capture"
    } else {
        "playback"
    };
    if pattern.eq_ignore_ascii_case("default") {
        let device = unsafe { en.GetDefaultAudioEndpoint(flow, eConsole)? };
        let name = unsafe {
            device
                .OpenPropertyStore(STGM_READ)?
                .GetValue(&PKEY_Device_FriendlyName)?
                .to_string()
        };
        return Ok(Endpoint { device, name });
    }
    let needle = pattern.to_lowercase();
    let mut hits: Vec<Endpoint> = endpoints(en, flow)?
        .into_iter()
        .filter(|e| e.name.to_lowercase().contains(&needle))
        .collect();
    if let Some(exact) = hits
        .iter()
        .position(|e| e.name.eq_ignore_ascii_case(pattern))
    {
        return Ok(hits.swap_remove(exact));
    }
    match hits.len() {
        1 => Ok(hits.remove(0)),
        0 => bail!("no {kind} device matches {pattern:?} (see winbench list)"),
        _ => bail!(
            "{pattern:?} matches several {kind} devices: {}",
            hits.iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn mix_format(device: &IMMDevice) -> Result<String> {
    unsafe {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let format = client.GetMixFormat()?;
        let f = *format;
        CoTaskMemFree(Some(format.cast()));
        let (rate, channels, bits) = (f.nSamplesPerSec, f.nChannels, f.wBitsPerSample);
        Ok(format!("{rate} Hz, {channels} ch, {bits} bit"))
    }
}

fn list() -> Result<()> {
    let en = enumerator()?;
    for (flow, title) in [(eCapture, "Capture"), (eRender, "Playback")] {
        let default = find(&en, flow, "default")
            .map(|e| e.name)
            .unwrap_or_default();
        println!("{title} devices:");
        for e in endpoints(&en, flow)? {
            let format = mix_format(&e.device).unwrap_or_else(|err| format!("format error: {err}"));
            let mark = if e.name == default { " (default)" } else { "" };
            println!("  {}{mark}  [{format}]", e.name);
        }
    }
    Ok(())
}

fn float_mono() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: 3,
        nChannels: 1,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * 4,
        nBlockAlign: 4,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

fn open_client(device: &IMMDevice, loopback: bool) -> Result<IAudioClient> {
    let mut flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    if loopback {
        flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
    }
    unsafe {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            BUFFER_HNS,
            0,
            &float_mono(),
            None,
        )?;
        Ok(client)
    }
}

fn now_hns(freq: i64) -> u64 {
    let mut counter = 0;
    unsafe { QueryPerformanceCounter(&raw mut counter).ok() };
    u64::try_from(i128::from(counter) * 10_000_000 / i128::from(freq)).unwrap_or(0)
}

struct Packet {
    pos: u64,
    qpc: u64,
    frames: u32,
    flags: u32,
    read: u64,
}

struct Capture {
    label: &'static str,
    name: String,
    client: IAudioClient,
    reader: IAudioCaptureClient,
    recording: bool,
    first: Option<u64>,
    samples: Vec<f32>,
    packets: Vec<Packet>,
    position_gaps: u32,
}

impl Capture {
    fn open(en: &IMMDeviceEnumerator, spec: &str, label: &'static str) -> Result<Self> {
        let (flow, loopback, pattern) = if let Some(p) = spec.strip_prefix("cap:") {
            (eCapture, false, p)
        } else if let Some(p) = spec.strip_prefix("loop:") {
            (eRender, true, p)
        } else {
            bail!("{spec:?} must start with cap: or loop:");
        };
        let ep = find(en, flow, pattern)?;
        let client = open_client(&ep.device, loopback)
            .with_context(|| format!("opening {:?} for capture", ep.name))?;
        let reader = unsafe { client.GetService()? };
        Ok(Self {
            label,
            name: if loopback {
                format!("loopback of {}", ep.name)
            } else {
                ep.name
            },
            client,
            reader,
            recording: false,
            first: None,
            samples: Vec::new(),
            packets: Vec::new(),
            position_gaps: 0,
        })
    }

    fn drain(&mut self, freq: i64) -> Result<()> {
        loop {
            if unsafe { self.reader.GetNextPacketSize()? } == 0 {
                return Ok(());
            }
            let (mut data, mut frames, mut flags, mut pos, mut qpc) = (ptr::null_mut(), 0, 0, 0, 0);
            unsafe {
                self.reader.GetBuffer(
                    &raw mut data,
                    &raw mut frames,
                    &raw mut flags,
                    Some(&raw mut pos),
                    Some(&raw mut qpc),
                )?;
            }
            if self.recording {
                let first = *self.first.get_or_insert(pos);
                let at = usize::try_from(pos.saturating_sub(first))?;
                if at > self.samples.len() {
                    self.position_gaps += 1;
                }
                self.samples.resize(at, 0.0);
                let n = frames as usize;
                if flags & SILENT != 0 || data.is_null() {
                    self.samples.resize(at + n, 0.0);
                } else {
                    // WASAPI buffers are aligned to the frame size, 4 bytes here.
                    #[allow(clippy::cast_ptr_alignment)]
                    let packet = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), n) };
                    self.samples.extend_from_slice(packet);
                }
                self.packets.push(Packet {
                    pos,
                    qpc,
                    frames,
                    flags,
                    read: now_hns(freq),
                });
            }
            unsafe { self.reader.ReleaseBuffer(frames)? };
        }
    }

    /// Fits device timestamps against position. Returns the first sample's time (100 ns
    /// units), the clock error (ppm), the worst deviation from the fit (ms), and whether
    /// read times stood in for missing device timestamps.
    fn timing(&self) -> Result<(f64, f64, f64, bool)> {
        let first = self
            .first
            .ok_or_else(|| anyhow!("{}: no audio arrived", self.label))?;
        let mut points: Vec<(f64, f64)> = self
            .packets
            .iter()
            .filter(|p| p.flags & TIMESTAMP_ERROR == 0)
            .map(|p| ((p.pos - first) as f64, p.qpc as f64))
            .collect();
        let from_read = points.len() < 10;
        if from_read {
            points = self
                .packets
                .iter()
                .map(|p| ((p.pos + u64::from(p.frames) - first) as f64, p.read as f64))
                .collect();
        }
        if points.len() < 10 {
            bail!("{}: only {} packets", self.label, points.len());
        }
        let n = points.len() as f64;
        let (mx, my) = points
            .iter()
            .fold((0.0, 0.0), |(a, b), (x, y)| (a + x / n, b + y / n));
        let (sxy, sxx) = points.iter().fold((0.0, 0.0), |(a, b), (x, y)| {
            (a + (x - mx) * (y - my), b + (x - mx) * (x - mx))
        });
        let slope = sxy / sxx;
        let start = my - slope * mx;
        let worst = points
            .iter()
            .map(|(x, y)| (y - (start + slope * x)).abs())
            .fold(0.0, f64::max);
        Ok((
            start,
            (slope / HNS_PER_FRAME - 1.0) * 1e6,
            worst / 1e4,
            from_read,
        ))
    }
}

struct Player {
    client: IAudioClient,
    render: IAudioRenderClient,
    size: u32,
    wav: Vec<f32>,
    at: usize,
}

impl Player {
    fn open(en: &IMMDeviceEnumerator, path: &PathBuf, pattern: &str) -> Result<(Self, String)> {
        let wav = load_wav(path)?;
        let ep = find(en, eRender, pattern)?;
        let client = open_client(&ep.device, false)
            .with_context(|| format!("opening {:?} for playback", ep.name))?;
        let (size, render) = unsafe { (client.GetBufferSize()?, client.GetService()?) };
        let mut player = Self {
            client,
            render,
            size,
            wav,
            at: 0,
        };
        player.fill()?;
        Ok((player, ep.name))
    }

    fn fill(&mut self) -> Result<()> {
        let free = self.size - unsafe { self.client.GetCurrentPadding()? };
        if free == 0 {
            return Ok(());
        }
        unsafe {
            #[allow(clippy::cast_ptr_alignment)]
            let buf = self.render.GetBuffer(free)?.cast::<f32>();
            for s in std::slice::from_raw_parts_mut(buf, free as usize) {
                *s = self.wav[self.at];
                self.at = (self.at + 1) % self.wav.len();
            }
            self.render.ReleaseBuffer(free, 0)?;
        }
        Ok(())
    }
}

fn load_wav(path: &PathBuf) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path).with_context(|| format!("{}", path.display()))?;
    let spec = reader.spec();
    if spec.sample_rate != RATE {
        bail!(
            "{} is {} Hz, expected {RATE}",
            path.display(),
            spec.sample_rate
        );
    }
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / f64::from(1u32 << (spec.bits_per_sample - 1));
            reader
                .samples::<i32>()
                .map(|s| s.map(|s| (f64::from(s) * scale) as f32))
                .collect::<Result<_, _>>()?
        }
    };
    let channels = usize::from(spec.channels);
    let mono: Vec<f32> = samples
        .chunks_exact(channels)
        .map(|c| c.iter().sum::<f32>() / channels as f32)
        .collect();
    if mono.is_empty() {
        bail!("{} is empty", path.display());
    }
    Ok(mono)
}

fn measure(o: &Options) -> Result<()> {
    let en = enumerator()?;
    let mut freq = 0;
    unsafe { QueryPerformanceFrequency(&raw mut freq)? };
    let mut streams = [
        Capture::open(&en, &o.reference, "ref")?,
        Capture::open(&en, &o.out, "out")?,
    ];
    let mut player = match &o.play {
        Some(path) => Some(Player::open(&en, path, &o.play_dev)?),
        None => None,
    };
    let mut summary = String::new();
    writeln!(summary, "ref: {}", streams[0].name)?;
    writeln!(summary, "out: {}", streams[1].name)?;
    if let (Some((_, dev)), Some(path)) = (&player, &o.play) {
        writeln!(summary, "playing {} to {dev}", path.display())?;
    }
    print!("{summary}");
    let printed = summary.len();
    println!("warming up {} s, then recording {} s", o.warmup, o.seconds);

    unsafe {
        if let Some((p, _)) = &player {
            p.client.Start()?;
        }
        for s in &streams {
            s.client.Start()?;
        }
    }
    let start = Instant::now();
    let warmup = Duration::from_secs(o.warmup.into());
    let end = warmup + Duration::from_secs(o.seconds.into());
    while start.elapsed() < end {
        thread::sleep(Duration::from_millis(5));
        if let Some((p, _)) = &mut player {
            p.fill()?;
        }
        let recording = start.elapsed() >= warmup;
        for s in &mut streams {
            s.recording = recording;
            s.drain(freq)?;
        }
    }
    unsafe {
        for s in &streams {
            s.client.Stop()?;
        }
        if let Some((p, _)) = &player {
            p.client.Stop()?;
        }
    }
    report(o, &streams, &mut summary)?;
    print!("{}", &summary[printed..]);
    fs::write(o.dir.join(format!("{}.txt", o.name)), &summary)?;
    Ok(())
}

fn report(o: &Options, streams: &[Capture; 2], summary: &mut String) -> Result<()> {
    fs::create_dir_all(&o.dir)?;
    write_packets(&o.dir.join(format!("{}.csv", o.name)), streams)?;
    let [reference, out] = streams;
    let (t_ref, ppm_ref, jitter_ref, read_ref) = reference.timing()?;
    let (t_out, ppm_out, jitter_out, read_out) = out.timing()?;
    for (s, ppm, jitter, from_read) in [
        (reference, ppm_ref, jitter_ref, read_ref),
        (out, ppm_out, jitter_out, read_out),
    ] {
        if from_read {
            writeln!(
                summary,
                "note: {} gave no usable timestamps, so it is timed by when packets were read",
                s.label
            )?;
        }
        let flagged = |bit| s.packets.iter().filter(|p| p.flags & bit != 0).count();
        writeln!(
            summary,
            "{}: {:.1} s, clock {ppm:+.0} ppm, timestamp jitter {jitter:.2} ms, \
             discontinuities {}, position gaps {}, timestamp errors {}",
            s.label,
            s.samples.len() as f64 / f64::from(RATE),
            flagged(DISCONTINUITY),
            s.position_gaps,
            flagged(TIMESTAMP_ERROR),
        )?;
    }

    let common = t_ref.max(t_out);
    let skip = |t: f64| ((common - t) / HNS_PER_FRAME).round() as usize;
    let a = &reference.samples[skip(t_ref).min(reference.samples.len())..];
    let b = &out.samples[skip(t_out).min(out.samples.len())..];
    let len = a.len().min(b.len());
    let (a, b) = (&a[..len], &b[..len]);
    if len < RATE as usize * 2 {
        bail!("only {len} aligned samples\n{summary}");
    }
    let mut file = BufWriter::new(fs::File::create(o.dir.join(format!("{}.f32", o.name)))?);
    for (l, r) in a.iter().zip(b) {
        file.write_all(&l.to_le_bytes())?;
        file.write_all(&r.to_le_bytes())?;
    }
    file.flush()?;
    analyze(a, b, summary)
}

fn analyze_file(path: &str) -> Result<()> {
    let bytes = fs::read(path).with_context(|| path.to_owned())?;
    let (a, b): (Vec<f32>, Vec<f32>) = bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|f| {
            let (l, r) = f.split_at(4);
            (
                f32::from_le_bytes(l.try_into().unwrap_or_default()),
                f32::from_le_bytes(r.try_into().unwrap_or_default()),
            )
        })
        .unzip();
    let mut summary = String::new();
    analyze(&a, &b, &mut summary)?;
    print!("{summary}");
    Ok(())
}

fn analyze(a: &[f32], b: &[f32], summary: &mut String) -> Result<()> {
    let (lag, corr) = delay(a, b);
    let polarity = if corr < 0.0 {
        ", inverted polarity"
    } else {
        ""
    };
    writeln!(
        summary,
        "delay {:.1} ms (correlation {corr:.3}{polarity})",
        lag as f64 / 48.0
    )?;
    let (env_lag, env_corr) = envelope_delay(a, b);
    writeln!(
        summary,
        "envelope delay {env_lag} ms (correlation {env_corr:.3})"
    )?;
    let chunk = RATE as usize * 5;
    let per_chunk: Vec<String> = a
        .chunks_exact(chunk)
        .zip(b.chunks_exact(chunk))
        .map(|(x, y)| format!("{:.1}", delay(x, y).0 as f64 / 48.0))
        .collect();
    writeln!(summary, "delay per 5 s: {} ms", per_chunk.join(" "))?;
    let gaps = silent_gaps(&b[usize::try_from(lag.max(0))?..]);
    writeln!(summary, "silent gaps over 2 ms in out: {gaps}")?;
    let envelope_ok = env_corr >= 0.5;
    if corr.abs() >= 0.2 {
        if envelope_ok && (lag as f64 / 48.0 - env_lag as f64).abs() > 5.0 {
            writeln!(
                summary,
                "WARNING: the waveform and envelope delays disagree"
            )?;
        }
    } else if envelope_ok {
        writeln!(
            summary,
            "note: the waveform doesn't match (generated output), so the envelope delay is the result"
        )?;
    } else {
        writeln!(
            summary,
            "WARNING: low correlation, the delay is not trustworthy"
        )?;
    }
    if lag < -96 {
        writeln!(
            summary,
            "WARNING: out is over 2 ms ahead of ref, the alignment is wrong"
        )?;
    }
    Ok(())
}

fn write_packets(path: &PathBuf, streams: &[Capture]) -> Result<()> {
    let mut file = BufWriter::new(fs::File::create(path)?);
    writeln!(file, "stream,pos,qpc_hns,frames,flags,read_hns")?;
    for s in streams {
        for p in &s.packets {
            writeln!(
                file,
                "{},{},{},{},{},{}",
                s.label, p.pos, p.qpc, p.frames, p.flags, p.read
            )?;
        }
    }
    Ok(file.flush()?)
}

/// Lag of `b` behind `a` in samples, searched within one second either way, and the
/// normalized correlation at that lag. The sign of the correlation shows polarity.
fn delay(a: &[f32], b: &[f32]) -> (i64, f64) {
    let m = a.len().min(b.len()).min(1 << 21);
    let wide = |x: &[f32]| x[..m].iter().map(|&v| f64::from(v)).collect::<Vec<_>>();
    xcorr(&wide(a), &wide(b), RATE as usize)
}

/// Lag in ms between the 1 ms log-loudness envelopes. It still works when the effect
/// regenerates the waveform instead of filtering it, as Studio Voice does.
fn envelope_delay(a: &[f32], b: &[f32]) -> (i64, f64) {
    let envelope = |x: &[f32]| {
        let e: Vec<f64> = x
            .as_chunks::<48>()
            .0
            .iter()
            .map(|c| {
                let power = c.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / 48.0;
                (power.sqrt() + 1e-6).ln()
            })
            .collect();
        let mean = e.iter().sum::<f64>() / e.len().max(1) as f64;
        e.into_iter().map(|v| v - mean).collect::<Vec<_>>()
    };
    let (lag, corr) = xcorr(&envelope(a), &envelope(b), 1000);
    if lag < 0 || corr < 0.0 {
        return (lag, 0.0);
    }
    (lag, corr)
}

fn xcorr(a: &[f64], b: &[f64], reach: usize) -> (i64, f64) {
    let m = a.len().min(b.len());
    if m < 2 {
        return (0, 0.0);
    }
    let n = (2 * m).next_power_of_two();
    let mut planner = FftPlanner::<f64>::new();
    let forward = planner.plan_fft_forward(n);
    let spectrum = |x: &[f64]| {
        let mut buf: Vec<Complex<f64>> = x[..m].iter().map(|&v| Complex::new(v, 0.0)).collect();
        buf.resize(n, Complex::default());
        forward.process(&mut buf);
        buf
    };
    let (fa, fb) = (spectrum(a), spectrum(b));
    let mut xc: Vec<Complex<f64>> = fb.iter().zip(&fa).map(|(y, x)| y * x.conj()).collect();
    planner.plan_fft_inverse(n).process(&mut xc);
    let reach = reach.min(m - 1);
    let (lag, peak) = (0..=reach)
        .map(|k| (k as i64, xc[k].re))
        .chain((1..=reach).map(|k| (-(k as i64), xc[n - k].re)))
        .max_by(|x, y| x.1.abs().total_cmp(&y.1.abs()))
        .unwrap_or((0, 0.0));
    let energy = |x: &[f64]| x[..m].iter().map(|v| v * v).sum::<f64>();
    (
        lag,
        peak / n as f64 / (energy(a) * energy(b)).sqrt().max(1e-30),
    )
}

fn silent_gaps(x: &[f32]) -> usize {
    let mut gaps = 0;
    let mut run = 0;
    for v in x.iter().chain([1.0].iter()) {
        if v.abs() < 1e-7 {
            run += 1;
        } else {
            if run > 96 {
                gaps += 1;
            }
            run = 0;
        }
    }
    gaps
}
