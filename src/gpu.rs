use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Generation {
    Turing,
    Ampere,
    Ada,
    Blackwell,
}

impl Generation {
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

/// Where the driver keeps the Windows `_nvngx.dll` it ships for Proton, found the way Proton
/// does: beside `libGLX_nvidia.so.0`, in `nvidia/wine`.
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

pub struct Gpu {
    pub name: String,
    pub generation: Generation,
}

pub fn detect() -> Result<Gpu> {
    let output = Command::new("nvidia-smi")
        .args(["--query-gpu=name,compute_cap", "--format=csv,noheader"])
        .output()
        .context("running nvidia-smi (is the NVIDIA driver installed?)")?;
    if !output.status.success() {
        bail!(
            "nvidia-smi failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8(output.stdout)?;
    let line = stdout
        .lines()
        .next()
        .context("nvidia-smi reported no GPU")?;
    let (name, cap) = line
        .rsplit_once(',')
        .context("unexpected nvidia-smi output")?;
    let (major, minor) = cap
        .trim()
        .split_once('.')
        .context("unexpected compute capability")?;
    let (major, minor): (u32, u32) = (major.parse()?, minor.parse()?);
    let generation = Generation::from_compute_capability(major, minor)
        .with_context(|| format!("{} (compute capability {major}.{minor}) is not an RTX GPU supported by NVIDIA Broadcast", name.trim()))?;
    Ok(Gpu {
        name: name.trim().to_owned(),
        generation,
    })
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
