use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::paths::Paths;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Generation {
    Turing,
    Ampere,
    Ada,
    Blackwell,
}

impl Generation {
    pub const ALL: [Self; 4] = [Self::Turing, Self::Ampere, Self::Ada, Self::Blackwell];

    pub fn from_compute_capability(major: u32, minor: u32) -> Option<Self> {
        match (major, minor) {
            (7, 5) => Some(Self::Turing),
            (8, 0 | 6 | 7) => Some(Self::Ampere),
            (8, 9) => Some(Self::Ada),
            (10 | 12, _) => Some(Self::Blackwell),
            _ => None,
        }
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Turing => "Turing",
            Self::Ampere => "Ampere",
            Self::Ada => "Ada",
            Self::Blackwell => "Blackwell",
        })
    }
}

pub const NO_NGX: &str = "Super Resolution needs the NVIDIA driver's _nvngx.dll, which the \
     driver installs in nvidia/wine beside libGLX_nvidia.so.0, and this driver has none";

/// The driver's Windows `_nvngx.dll` for Proton, found like Proton does: in `nvidia/wine`
/// beside `libGLX_nvidia.so.0`.
pub fn ngx_dir() -> Option<PathBuf> {
    let listed = ["ldconfig", "/sbin/ldconfig", "/usr/sbin/ldconfig"]
        .iter()
        .find_map(|ldconfig| Command::new(ldconfig).arg("-p").output().ok())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    let from_ldconfig = glx_dirs(&listed)
        .into_iter()
        .map(|dir| dir.join("nvidia/wine"));
    let usual = [
        "/usr/lib/nvidia/wine",
        "/usr/lib64/nvidia/wine",
        "/usr/lib/x86_64-linux-gnu/nvidia/wine",
    ]
    .map(PathBuf::from);
    from_ldconfig
        .chain(usual)
        .find(|dir| dir.join("_nvngx.dll").is_file() && dir.join("nvngx.dll").is_file())
}

/// Folders of the 64-bit `libGLX_nvidia.so.0` in `ldconfig -p` output.
fn glx_dirs(listed: &str) -> Vec<PathBuf> {
    listed
        .lines()
        .filter(|line| {
            line.trim_start().starts_with("libGLX_nvidia.so.0") && line.contains("x86-64")
        })
        .filter_map(|line| {
            Path::new(line.rsplit_once("=> ")?.1.trim())
                .parent()
                .map(Path::to_path_buf)
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gpu {
    /// `GPU-…`, as CUDA and NVML take it.
    pub uuid: String,
    pub name: String,
    pub pci_bus: String,
    pub compute_capability: String,
    /// `None` when NVIDIA Broadcast doesn't support it.
    pub generation: Option<Generation>,
}

impl Gpu {
    /// Without an all-zero domain, as `lspci` shows it.
    pub fn short_bus(&self) -> &str {
        self.pci_bus
            .split_once(':')
            .filter(|(domain, _)| domain.bytes().all(|b| b == b'0'))
            .map_or(&self.pci_bus, |(_, rest)| rest)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chosen {
    pub gpu: Gpu,
    pub generation: Generation,
    /// Why the configured GPU isn't used.
    pub fallback: Option<String>,
    /// Other NVIDIA GPUs are present.
    pub others: bool,
}

impl Chosen {
    /// Worker env for this GPU. NVIDIA's effects also find the GPU through DXGI, so with several
    /// GPUs DXVK must offer only this one.
    pub fn pin(&self) -> Vec<(&'static str, String)> {
        let mut env = vec![("CUDA_VISIBLE_DEVICES", self.gpu.uuid.clone())];
        if self.others {
            // DXVK only matches 32 hex digits, without dashes.
            let hex: String = self
                .gpu
                .uuid
                .trim_start_matches("GPU-")
                .chars()
                .filter(char::is_ascii_hexdigit)
                .collect();
            env.push(("DXVK_FILTER_DEVICE_UUID", hex.to_ascii_lowercase()));
        }
        env
    }
}

/// nvidia-smi can hang when the driver is in a bad state.
const SMI_TIMEOUT: Duration = Duration::from_secs(3);

/// Returned by `list` when nvidia-smi hangs.
#[derive(Debug)]
pub struct TimedOut;

impl fmt::Display for TimedOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "nvidia-smi did not answer within {} s",
            SMI_TIMEOUT.as_secs()
        )
    }
}

impl std::error::Error for TimedOut {}

pub fn list() -> Result<Vec<Gpu>> {
    let mut child = Command::new("nvidia-smi")
        .args([
            "--query-gpu=uuid,pci.bus_id,compute_cap,name",
            "--format=csv,noheader",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running nvidia-smi (is the NVIDIA driver installed?)")?;
    let deadline = Instant::now() + SMI_TIMEOUT;
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(TimedOut.into());
        }
        thread::sleep(Duration::from_millis(5));
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "nvidia-smi failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(parse)
        .collect()
}

/// The name goes last, since it may hold commas.
fn parse(line: &str) -> Result<Gpu> {
    let mut fields = line.splitn(4, ',').map(str::trim);
    let (Some(uuid), Some(bus), Some(cap), Some(name)) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        bail!("unexpected nvidia-smi output: {line}");
    };
    let generation = cap
        .split_once('.')
        .and_then(|(major, minor)| Some((major.parse().ok()?, minor.parse().ok()?)))
        .and_then(|(major, minor)| Generation::from_compute_capability(major, minor));
    Ok(Gpu {
        uuid: uuid.to_owned(),
        name: name.to_owned(),
        pci_bus: bus.to_owned(),
        compute_capability: cap.to_owned(),
        generation,
    })
}

/// The configured GPU by UUID, else the first supported one, with the reason in `fallback`.
pub fn choose(gpus: &[Gpu], wanted: Option<&str>) -> Result<Chosen> {
    let others = gpus.len() > 1;
    let unsupported = |gpu: &Gpu| {
        format!(
            "{} (compute capability {}) is not an RTX GPU supported by NVIDIA Broadcast",
            gpu.name, gpu.compute_capability
        )
    };
    let reason = match wanted.map(str::trim).filter(|uuid| !uuid.is_empty()) {
        None => None,
        Some(uuid) => match gpus.iter().find(|gpu| gpu.uuid.eq_ignore_ascii_case(uuid)) {
            Some(gpu) => match gpu.generation {
                Some(generation) => {
                    return Ok(Chosen {
                        gpu: gpu.clone(),
                        generation,
                        fallback: None,
                        others,
                    });
                }
                None => Some(unsupported(gpu)),
            },
            None => Some(format!("the chosen GPU {uuid} is not present")),
        },
    };
    if let Some((gpu, generation)) = gpus.iter().find_map(|gpu| Some((gpu, gpu.generation?))) {
        return Ok(Chosen {
            gpu: gpu.clone(),
            generation,
            fallback: reason.map(|reason| format!("{reason}, so the effects run on {}", gpu.name)),
            others,
        });
    }
    match gpus {
        [] => bail!("nvidia-smi reported no GPU"),
        [gpu] => bail!(unsupported(gpu)),
        _ => bail!(
            "none of the NVIDIA GPUs is supported by NVIDIA Broadcast: {}",
            gpus.iter()
                .map(|gpu| format!("{} ({})", gpu.name, gpu.compute_capability))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Looked up at the first worker start, not at service start, so login never waits on
/// nvidia-smi or wakes a sleeping laptop GPU.
#[derive(Clone, Debug, Default)]
pub struct Lazy {
    wanted: Option<String>,
    lookup: Arc<Mutex<Lookup>>,
}

#[derive(Debug, Default)]
enum Lookup {
    #[default]
    Pending,
    Found(Chosen),
    GaveUp,
}

impl Lazy {
    pub fn new(wanted: Option<String>) -> Self {
        Self {
            wanted,
            lookup: Arc::default(),
        }
    }

    /// `None` leaves the choice to CUDA and DXVK. A failure is retried at the next start, but a
    /// hang is not, since this can run on the PipeWire main loop.
    pub fn get(&self) -> Option<Chosen> {
        let mut lookup = self.lookup.lock().unwrap();
        if matches!(*lookup, Lookup::Pending) {
            match detect(self.wanted.as_deref()) {
                Ok(chosen) => {
                    if let Some(fallback) = &chosen.fallback {
                        eprintln!("gpu: {fallback}");
                    }
                    eprintln!(
                        "gpu: effects run on {} ({})",
                        chosen.gpu.name, chosen.gpu.uuid
                    );
                    *lookup = Lookup::Found(chosen);
                }
                Err(e) => {
                    eprintln!("gpu: {e:#}; the effects use CUDA's default GPU");
                    if e.is::<TimedOut>() {
                        *lookup = Lookup::GaveUp;
                    }
                }
            }
        }
        match &*lookup {
            Lookup::Found(chosen) => Some(chosen.clone()),
            _ => None,
        }
    }

    /// What `get` found so far. Never waits for a lookup in progress.
    pub fn found(&self) -> Option<Chosen> {
        match &*self.lookup.try_lock().ok()? {
            Lookup::Found(chosen) => Some(chosen.clone()),
            _ => None,
        }
    }
}

/// Compares the configured UUID only, so a lookup never counts as a settings change.
impl PartialEq for Lazy {
    fn eq(&self, other: &Self) -> bool {
        self.wanted == other.wanted
    }
}

pub fn detect(wanted: Option<&str>) -> Result<Chosen> {
    choose(&list()?, wanted)
}

/// An unreadable config picks automatically.
pub fn detect_configured(paths: &Paths) -> Result<Chosen> {
    let config = Config::load(&paths.config).ok();
    detect(config.as_ref().and_then(|c| c.service.gpu.as_deref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_compute_capabilities() {
        assert_eq!(
            Generation::from_compute_capability(12, 0),
            Some(Generation::Blackwell)
        );
        assert_eq!(
            Generation::from_compute_capability(8, 9),
            Some(Generation::Ada)
        );
        assert_eq!(
            Generation::from_compute_capability(8, 6),
            Some(Generation::Ampere)
        );
        assert_eq!(
            Generation::from_compute_capability(7, 5),
            Some(Generation::Turing)
        );
        assert_eq!(Generation::from_compute_capability(6, 1), None);
    }

    fn gpu(uuid: &str, cap: &str, name: &str) -> Gpu {
        parse(&format!("{uuid}, 00000000:01:00.0, {cap}, {name}")).unwrap()
    }

    #[test]
    fn parses_nvidia_smi_lines() {
        let gpu = parse(
            "GPU-3fdd3802-80d5-ae4a-82ba-f9faed082c27, 00000000:01:00.0, 12.0, NVIDIA GeForce RTX 5080",
        )
        .unwrap();
        assert_eq!(gpu.uuid, "GPU-3fdd3802-80d5-ae4a-82ba-f9faed082c27");
        assert_eq!(gpu.name, "NVIDIA GeForce RTX 5080");
        assert_eq!(gpu.short_bus(), "01:00.0");
        assert_eq!(gpu.generation, Some(Generation::Blackwell));
        let gpu = parse("GPU-1, 00000001:65:00.0, 7.5, NVIDIA GeForce GTX 1650, Laptop").unwrap();
        assert_eq!(gpu.name, "NVIDIA GeForce GTX 1650, Laptop");
        assert_eq!(gpu.short_bus(), "00000001:65:00.0");
        assert_eq!(gpu.generation, Some(Generation::Turing));
        assert_eq!(
            parse("GPU-2, 0:1:0.0, 6.1, GTX 1080").unwrap().generation,
            None
        );
        assert!(parse("garbage").is_err());
    }

    #[test]
    fn chooses_the_configured_gpu_or_falls_back() {
        let old = gpu("GPU-old", "6.1", "NVIDIA GeForce GTX 1080");
        let ada = gpu("GPU-ada", "8.9", "NVIDIA GeForce RTX 4090");
        let ampere = gpu("GPU-amp", "8.6", "NVIDIA GeForce RTX 3060");
        let gpus = [old.clone(), ada.clone(), ampere.clone()];

        let auto = choose(&gpus, None).unwrap();
        assert_eq!((auto.gpu.uuid.as_str(), auto.fallback), ("GPU-ada", None));
        assert!(auto.others);
        let picked = choose(&gpus, Some("gpu-AMP")).unwrap();
        assert_eq!(picked.generation, Generation::Ampere);
        assert_eq!(picked.fallback, None);

        let gone = choose(&gpus, Some("GPU-gone")).unwrap();
        assert_eq!(gone.gpu.uuid, "GPU-ada");
        assert!(gone.fallback.unwrap().contains("GPU-gone is not present"));
        let unsupported = choose(&gpus, Some("GPU-old")).unwrap();
        assert!(unsupported.fallback.unwrap().contains("GTX 1080"));

        let single = choose(std::slice::from_ref(&ampere), Some("")).unwrap();
        assert!(!single.others);
        assert!(choose(std::slice::from_ref(&old), None).is_err());
        assert!(choose(&[], None).is_err());
    }

    #[test]
    fn lazy_choices_compare_by_config() {
        let lazy = Lazy::new(Some("GPU-1".into()));
        assert_eq!(lazy, Lazy::new(Some("GPU-1".into())));
        assert_ne!(lazy, Lazy::new(None));
        assert_eq!(
            lazy.found(),
            None,
            "nothing is looked up until a worker starts"
        );
    }

    #[test]
    fn pins_dxvk_only_with_several_gpus() {
        let gpus = [
            gpu(
                "GPU-3fdd3802-80d5-ae4a-82ba-f9faed082c27",
                "12.0",
                "RTX 5080",
            ),
            gpu(
                "GPU-0000aaaa-0000-0000-0000-000000000000",
                "12.0",
                "RTX 5080",
            ),
        ];
        let env = choose(&gpus[..1], None).unwrap().pin();
        assert_eq!(env, [("CUDA_VISIBLE_DEVICES", gpus[0].uuid.clone())]);
        let env = choose(&gpus, Some(&gpus[1].uuid)).unwrap().pin();
        assert_eq!(env[0], ("CUDA_VISIBLE_DEVICES", gpus[1].uuid.clone()));
        assert_eq!(
            env[1],
            (
                "DXVK_FILTER_DEVICE_UUID",
                "0000aaaa000000000000000000000000".into()
            )
        );
    }

    #[test]
    fn finds_the_driver_library_folder() {
        let listed = "1234 libs found in cache `/etc/ld.so.cache'
\tlibGLX_nvidia.so.0 (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libGLX_nvidia.so.0
\tlibGLX_nvidia.so.0 (libc6) => /usr/lib/i386-linux-gnu/libGLX_nvidia.so.0
\tlibGLX_mesa.so.0 (libc6,x86-64) => /usr/lib/libGLX_mesa.so.0
";
        assert_eq!(
            glx_dirs(listed),
            [PathBuf::from("/usr/lib/x86_64-linux-gnu")]
        );
    }
}
