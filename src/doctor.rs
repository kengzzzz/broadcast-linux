use std::fmt::Write;
use std::fs;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::audio::{self, Kind};
use crate::camera::expand_home;
use crate::config::{CameraConfig, Config};
use crate::nvidia::Installation;
use crate::paths::{self, Paths};
use crate::{gpu, prefix, setup, v4l2, webcam, worker};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
    Skip,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "FAIL",
            Self::Skip => "skip",
        }
    }
}

pub struct Item {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    /// Shell commands that fix this, in order.
    pub fix: Vec<String>,
}

pub struct Report {
    pub version: String,
    pub items: Vec<Item>,
}

impl Report {
    pub fn failed(&self) -> usize {
        self.items
            .iter()
            .filter(|i| i.status == Status::Fail)
            .count()
    }

    pub fn get(&self, name: &str) -> Option<&Item> {
        self.items.iter().find(|i| i.name == name)
    }
}

struct Problem {
    status: Status,
    detail: String,
    fix: Vec<String>,
}

impl Problem {
    fn warn(detail: impl Into<String>, fix: &[&str]) -> Self {
        Self {
            status: Status::Warn,
            detail: detail.into(),
            fix: fix.iter().map(|c| (*c).to_owned()).collect(),
        }
    }

    fn fail(detail: impl Into<String>) -> Self {
        Self {
            status: Status::Fail,
            detail: detail.into(),
            fix: Vec::new(),
        }
    }
}

impl From<anyhow::Error> for Problem {
    fn from(e: anyhow::Error) -> Self {
        Self::fail(format!("{e:#}"))
    }
}

impl From<v4l2::Hint> for Problem {
    fn from(hint: v4l2::Hint) -> Self {
        Self {
            status: Status::Fail,
            detail: hint.text,
            fix: hint.commands,
        }
    }
}

type Check = Result<String, Problem>;

pub fn run() -> Result<()> {
    let report = check(&Paths::new()?);
    println!("{}", report.version);
    for item in &report.items {
        let indent = format!("\n{:22}", "");
        let mut detail = item.detail.trim_end().replace('\n', &indent);
        for command in &item.fix {
            let _ = write!(detail, "{indent}$ {command}");
        }
        println!("{:<5} {:<15} {detail}", item.status.label(), item.name);
    }
    let failed = report.failed();
    if failed > 0 {
        bail!("{failed} check(s) failed");
    }
    Ok(())
}

pub fn check(paths: &Paths) -> Report {
    let kernel = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let version = format!(
        "broadcast-linux {} (kernel {})",
        env!("CARGO_PKG_VERSION"),
        kernel.trim()
    );
    let mut items = Vec::new();
    let mut report = |name: &'static str, check: Check| {
        let item = match check {
            Ok(detail) => Item {
                name,
                status: Status::Ok,
                detail,
                fix: Vec::new(),
            },
            Err(p) => Item {
                name,
                status: p.status,
                detail: p.detail,
                fix: p.fix,
            },
        };
        items.push(item);
    };
    let skip = |why: &str| {
        Err(Problem {
            status: Status::Skip,
            detail: why.into(),
            fix: Vec::new(),
        })
    };

    report("gpu", gpu_check());
    report("wine", wine());
    report("nvidia files", nvidia(paths));
    report("wine prefix", prefix(paths));
    report("workers", workers());
    report("display", display());
    report("pipewire", pipewire());
    let service = service();
    let running = service.is_ok();
    report("service", service);

    let config = match Config::load(&paths.config) {
        Ok(config) => {
            let source = if paths.config.exists() {
                paths::tilde(&paths.config)
            } else {
                "defaults (no config file)".into()
            };
            report("config", Ok(source));
            config
        }
        Err(e) => {
            report("config", Err(e.into()));
            for name in ["mic", "speaker", "virtual camera", "webcam"] {
                report(name, skip("config did not load"));
            }
            return Report { version, items };
        }
    };

    for (kind, enabled, target) in [
        (Kind::Mic, config.mic.enabled, &config.mic.input),
        (
            Kind::Speaker,
            config.speaker.enabled,
            &config.speaker.output,
        ),
    ] {
        let name = kind.label();
        if enabled {
            report(name, audio_device(kind, target, running));
        } else {
            report(name, skip("disabled in config"));
        }
    }
    if config.camera.enabled {
        report("virtual camera", loopback(&config.camera));
        report("webcam", webcam(&config.camera));
    } else {
        report("virtual camera", skip("disabled in config"));
        report("webcam", skip("disabled in config"));
    }
    Report { version, items }
}

fn output(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program} (is it installed?)"))?;
    if !out.status.success() {
        bail!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn gpu_check() -> Check {
    let gpu = gpu::detect()?;
    let driver = output(
        "nvidia-smi",
        &["--query-gpu=driver_version", "--format=csv,noheader"],
    )?;
    let driver = driver.lines().next().unwrap_or_default();
    Ok(format!(
        "{} ({}), driver {driver}",
        gpu.name, gpu.generation
    ))
}

fn wine() -> Check {
    let version = output("wine", &["--version"])?;
    match wine_major(&version) {
        Some(major) if major >= 11 => Ok(version),
        _ => Err(Problem::fail(format!(
            "{version}; Wine 11 or newer is required"
        ))),
    }
}

fn wine_major(version: &str) -> Option<u32> {
    version
        .strip_prefix("wine-")?
        .split('.')
        .next()?
        .parse()
        .ok()
}

fn nvidia(paths: &Paths) -> Check {
    if let Some(problem) = setup::files_problem(paths, gpu::detect()?.generation) {
        return Err(Problem::fail(problem));
    }
    let install = Installation::find(paths)?;
    Ok(paths::tilde(&install.runtime))
}

fn prefix(paths: &Paths) -> Check {
    let prefix = paths.prefix();
    match prefix::problem(&prefix) {
        Some(problem) => Err(Problem::fail(problem)),
        None => Ok(paths::tilde(&prefix)),
    }
}

fn workers() -> Check {
    let files = [
        paths::workers_dir().join("afx_stream.exe.so"),
        paths::workers_dir().join("camera_stream.exe.so"),
        paths::relay_dir().join("x86_64-unix/nvcuda.dll.so"),
    ];
    if let Some(missing) = files.iter().find(|f| !f.exists()) {
        return Err(Problem::fail(format!(
            "{} not found (set BROADCAST_LINUX_LIBDIR)",
            paths::tilde(missing)
        )));
    }
    Ok(paths::tilde(&paths::lib_dir()))
}

fn display() -> Check {
    let vars = worker::manager_display();
    if !worker::has_display(&vars) {
        return Err(Problem::warn(
            "the user manager has no DISPLAY or WAYLAND_DISPLAY; effects load once a desktop session exports one",
            &[],
        ));
    }
    let names: Vec<String> = vars.iter().map(|(k, v)| format!("{k}={v}")).collect();
    Ok(names.join(" "))
}

fn pipewire() -> Check {
    let info = output("pactl", &["info"])?;
    let server = info
        .lines()
        .find_map(|l| l.strip_prefix("Server Name: "))
        .unwrap_or("unknown server")
        .to_owned();
    if output("systemctl", &["--user", "is-active", "wireplumber"]).is_err() {
        return Err(Problem::warn(
            format!("{server}; WirePlumber is not running"),
            &["systemctl --user enable --now wireplumber"],
        ));
    }
    Ok(format!("{server}, WirePlumber running"))
}

fn service() -> Check {
    output("systemctl", &["--user", "is-active", "broadcast-linux"])
        .map(|_| "running".into())
        .map_err(|_| {
            Problem::warn(
                "not running",
                &["systemctl --user enable --now broadcast-linux"],
            )
        })
}

fn audio_device(kind: Kind, target: &str, running: bool) -> Check {
    let name = audio::resolve_target(kind, target)?;
    let (list, class) = match kind {
        Kind::Mic => ("sources", "source"),
        Kind::Speaker => ("sinks", "sink"),
    };
    let devices = output("pactl", &["list", "short", list])?;
    if !devices
        .lines()
        .any(|l| l.split('\t').nth(1) == Some(name.as_str()))
    {
        return Err(Problem::fail(format!(
            "{name} not found; see `pactl list short {list}`"
        )));
    }
    let mut muted = vec![name.as_str()];
    if running {
        muted.push(kind.node_name());
    }
    muted.retain(|node| {
        output("pactl", &[&format!("get-{class}-mute"), node]).is_ok_and(|o| is_muted(&o))
    });
    if let Some(node) = muted.first() {
        return Err(Problem::warn(
            format!("{node} is muted"),
            &[&format!("pactl set-{class}-mute {node} 0")],
        ));
    }
    Ok(name)
}

fn is_muted(pactl: &str) -> bool {
    pactl.trim() == "Mute: yes"
}

fn loopback(camera: &CameraConfig) -> Check {
    let label = v4l2::check(&camera.device)?;
    Ok(format!("{} \"{label}\"", camera.device))
}

fn webcam(camera: &CameraConfig) -> Check {
    if let Some(background) = &camera.background
        && !expand_home(background).exists()
    {
        return Err(Problem::fail(format!(
            "background image {background} does not exist"
        )));
    }
    let format = webcam::choose(
        &camera.input,
        camera.input_format,
        camera.width,
        camera.height,
    )?;
    Ok(format!(
        "{} {} {}x{}",
        camera.input,
        format.label(),
        camera.width,
        camera.height
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wine_versions() {
        assert_eq!(wine_major("wine-11.19"), Some(11));
        assert_eq!(wine_major("wine-11.0 (Staging)"), Some(11));
        assert_eq!(wine_major("wine-9.22"), Some(9));
        assert_eq!(wine_major("garbage"), None);
    }

    #[test]
    fn parses_mute() {
        assert!(is_muted("Mute: yes\n"));
        assert!(!is_muted("Mute: no"));
    }
}
