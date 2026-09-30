use std::fmt;
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
}
