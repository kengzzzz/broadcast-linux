use std::fs;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::audio::{self, Kind};
use crate::camera::expand_home;
use crate::config::{CameraConfig, Config};
use crate::nvidia::Installation;
use crate::paths::{self, Paths};
use crate::{gpu, v4l2, webcam, worker};

enum Problem {
    Warn(String),
    Fail(String),
}

impl From<anyhow::Error> for Problem {
    fn from(e: anyhow::Error) -> Self {
        Self::Fail(format!("{e:#}"))
    }
}

type Check = Result<String, Problem>;

pub fn run() -> Result<()> {
    let paths = Paths::new()?;
    let kernel = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    println!(
        "broadcast-linux {} (kernel {})",
        env!("CARGO_PKG_VERSION"),
        kernel.trim()
    );
    let mut failed = 0;
    let mut report = |name: &str, check: Check| {
        let (status, detail) = match check {
            Ok(detail) => ("ok", detail),
            Err(Problem::Warn(detail)) => ("warn", detail),
            Err(Problem::Fail(detail)) => {
                failed += 1;
                ("FAIL", detail)
            }
        };
        let detail = detail.trim_end().replace('\n', &format!("\n{:22}", ""));
        println!("{status:<5} {name:<15} {detail}");
    };
    let skip = |name: &str, why: &str| println!("{:<5} {name:<15} {why}", "skip");

    report("gpu", gpu_check());
    report("wine", wine());
    report("nvidia files", nvidia(&paths));
    report("wine prefix", prefix(&paths));
    report("workers", workers());
    report("display", display());
    report("pipewire", pipewire());
    let service = service();
    let running = service.is_ok();
    report("service", service);

    let config = match Config::load(&paths.config) {
        Ok(config) => {
            let source = if paths.config.exists() {
                paths.config.display().to_string()
            } else {
                "defaults (no config file)".into()
            };
            report("config", Ok(source));
            config
        }
        Err(e) => {
            report("config", Err(e.into()));
            for name in ["mic", "speaker", "virtual camera", "webcam"] {
                skip(name, "config did not load");
            }
            return finish(failed);
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
        if enabled {
            report(kind.label(), audio_device(kind, target, running));
        } else {
            skip(kind.label(), "disabled in config");
        }
    }
    if config.camera.enabled {
        report("virtual camera", loopback(&config.camera));
        report("webcam", webcam(&config.camera));
    } else {
        skip("virtual camera", "disabled in config");
        skip("webcam", "disabled in config");
    }
    finish(failed)
}

fn finish(failed: usize) -> Result<()> {
    if failed > 0 {
        bail!("{failed} check(s) failed");
    }
    Ok(())
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
        _ => Err(Problem::Fail(format!(
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
    let install = Installation::find(paths)?;
    Ok(install.runtime.display().to_string())
}

fn prefix(paths: &Paths) -> Check {
    let prefix = paths.prefix();
    let system32 = prefix.join("drive_c/windows/system32");
    let missing: Vec<&str> = ["nvcuda.dll", "dxgi.dll", "d3d11.dll", "nvapi64.dll"]
        .into_iter()
        .filter(|dll| !system32.join(dll).exists())
        .collect();
    if !prefix.join("system.reg").exists() || !missing.is_empty() {
        return Err(Problem::Fail(format!(
            "{} is incomplete; run `broadcast-linux setup`",
            prefix.display()
        )));
    }
    Ok(prefix.display().to_string())
}

fn workers() -> Check {
    let files = [
        paths::workers_dir().join("afx_stream.exe.so"),
        paths::workers_dir().join("camera_stream.exe.so"),
        paths::relay_dir().join("x86_64-unix/nvcuda.dll.so"),
    ];
    if let Some(missing) = files.iter().find(|f| !f.exists()) {
        return Err(Problem::Fail(format!(
            "{} not found (set BROADCAST_LINUX_LIBDIR)",
            missing.display()
        )));
    }
    Ok(paths::lib_dir().display().to_string())
}

fn display() -> Check {
    let vars = worker::manager_display();
    if !worker::has_display(&vars) {
        return Err(Problem::Warn(
            "the user manager has no DISPLAY or WAYLAND_DISPLAY; effects load once a desktop session exports one".into(),
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
        return Err(Problem::Warn(format!(
            "{server}; WirePlumber is not running"
        )));
    }
    Ok(format!("{server}, WirePlumber running"))
}

fn service() -> Check {
    output("systemctl", &["--user", "is-active", "broadcast-linux"])
        .map(|_| "running".into())
        .map_err(|_| {
            Problem::Warn("not running: systemctl --user enable --now broadcast-linux".into())
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
        return Err(Problem::Fail(format!(
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
        return Err(Problem::Warn(format!(
            "{node} is muted: pactl set-{class}-mute {node} 0"
        )));
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
        return Err(Problem::Fail(format!(
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
