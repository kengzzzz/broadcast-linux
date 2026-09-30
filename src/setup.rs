use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::gpu::{self, Generation};
use crate::paths::Paths;
use crate::{download, prefix, sevenzip};

pub struct Installer {
    pub generation: Generation,
    pub build: &'static str,
    pub url: &'static str,
    pub size: u64,
    pub sha256: Option<&'static str>,
}

const BUILD: &str = "2.2.1.58338310";

pub const INSTALLERS: &[Installer] = &[
    Installer {
        generation: Generation::Blackwell,
        build: BUILD,
        url: "https://international.download.nvidia.com/Windows/broadcast/2.2.1/NVIDIA_Broadcast_Offline_Blackwell_v2.2.1.58338310.exe",
        size: 2_225_036_120,
        sha256: Some("caed3a89a0e3a680468801e818e6559b2b0358d6af217dd4996f37b2d5cb3878"),
    },
    Installer {
        generation: Generation::Ada,
        build: BUILD,
        url: "https://international.download.nvidia.com/Windows/broadcast/2.2.1/NVIDIA_Broadcast_Offline_Ada_v2.2.1.58338310.exe",
        size: 2_356_726_072,
        sha256: None,
    },
    Installer {
        generation: Generation::Ampere,
        build: BUILD,
        url: "https://international.download.nvidia.com/Windows/broadcast/2.2.1/NVIDIA_Broadcast_Offline_Ampere_v2.2.1.58338310.exe",
        size: 2_199_322_600,
        sha256: None,
    },
    Installer {
        generation: Generation::Turing,
        build: BUILD,
        url: "https://international.download.nvidia.com/Windows/broadcast/2.2.1/NVIDIA_Broadcast_Offline_Turing_v2.2.1.58338310.exe",
        size: 2_156_554_584,
        sha256: None,
    },
];

pub struct Options {
    pub accept_eula: bool,
    pub allow_unverified: bool,
    pub keep_installer: bool,
}

pub fn run(opts: &Options) -> Result<()> {
    let paths = Paths::new()?;
    let gpu = gpu::detect()?;
    let installer = INSTALLERS
        .iter()
        .find(|i| i.generation == gpu.generation)
        .with_context(|| format!("no NVIDIA Broadcast installer known for {}", gpu.generation))?;
    eprintln!("GPU: {} ({})", gpu.name, gpu.generation);
    if installer.sha256.is_none() && !opts.allow_unverified {
        bail!(
            "the {} installer has no pinned checksum yet; rerun with --allow-unverified to trust its size only",
            gpu.generation
        );
    }

    let out = paths.nvidia().join(installer.build);
    let complete = out.join(".complete").exists();
    if complete && out.join(STUDIO_LIGHT_DIR).is_dir() {
        eprintln!(
            "NVIDIA Broadcast {} files already installed",
            installer.build
        );
    } else {
        if complete {
            eprintln!("The Studio Light presets are missing; extracting them from the installer");
        }
        let file = installer.url.rsplit('/').next().unwrap_or("installer.exe");
        let exe = paths.downloads().join(file);
        download::fetch(installer.url, &exe, installer.size, installer.sha256)?;
        if !complete {
            accept_eula(&exe, &out, opts.accept_eula)?;
        }
        extract(&exe, &out)?;
        fs::write(out.join(".complete"), format!("{}\n", gpu.generation))?;
        if !opts.keep_installer {
            fs::remove_file(&exe)?;
        }
    }

    prefix::create(
        &paths.prefix(),
        &paths.downloads(),
        &crate::paths::relay_dir(),
    )?;
    eprintln!("Setup complete: {}", paths.data.display());
    Ok(())
}

fn accept_eula(exe: &Path, out: &Path, preaccepted: bool) -> Result<()> {
    let eula = out.join("EULA.txt");
    if !eula.exists() {
        sevenzip::extract(exe, out, |name| {
            (name == "EULA.txt").then(|| name.to_owned())
        })?;
    }
    if !eula.exists() {
        bail!(
            "the installer has no EULA.txt; refusing to continue without showing NVIDIA's licence"
        );
    }
    if preaccepted {
        eprintln!(
            "NVIDIA licence accepted via --accept-eula ({})",
            eula.display()
        );
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        bail!(
            "NVIDIA's licence must be accepted: read {} and rerun with --accept-eula",
            eula.display()
        );
    }
    let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".into());
    if Command::new(&pager).arg(&eula).status().is_err() {
        io::copy(&mut fs::File::open(&eula)?, &mut io::stdout())?;
    }
    eprint!("Do you accept NVIDIA's licence above? [y/N] ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes") {
        bail!("licence not accepted");
    }
    Ok(())
}

const MODELS_PREFIX: &str = "NvMaxineModels/NvModels/";
const STUDIO_LIGHT_PREFIX: &str = "NvMaxineClient/nv/vkl_";
pub const STUDIO_LIGHT_DIR: &str = "studio_light";

fn extract(exe: &Path, out: &Path) -> Result<()> {
    eprintln!("Extracting NVIDIA Broadcast effect libraries and models");
    let count = sevenzip::extract(exe, out, |name| {
        if let Some(rest) = name.strip_prefix(MODELS_PREFIX) {
            Some(format!("models/{rest}"))
        } else if name.starts_with(STUDIO_LIGHT_PREFIX)
            && Path::new(name)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("hdr"))
        {
            let file = name.rsplit('/').next()?;
            Some(format!("{STUDIO_LIGHT_DIR}/{file}"))
        } else if name == "EULA.txt"
            || name.ends_with("ThirdPartyLicenses.txt") && !name.contains('/')
        {
            Some(name.to_owned())
        } else {
            None
        }
    })?;
    if !out.join("models/nvbcast").is_dir() {
        bail!("the installer did not contain the expected NvMaxineModels/NvModels/nvbcast folder");
    }
    if !out.join(STUDIO_LIGHT_DIR).is_dir() {
        bail!(
            "the installer did not contain the Studio Light presets ({STUDIO_LIGHT_PREFIX}*.hdr)"
        );
    }
    eprintln!("Extracted {count} files to {}", out.display());
    Ok(())
}
