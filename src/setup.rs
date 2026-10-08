use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::gpu::{self, Generation};
use crate::nvidia::Installation;
use crate::paths::Paths;
use crate::progress::{self, Progress};
use crate::{download, prefix, sevenzip};

pub struct Installer {
    pub generation: Generation,
    pub build: &'static str,
    pub url: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

pub const BUILD: &str = "2.2.1.58338310";

pub const INSTALLERS: &[Installer] = &[
    Installer {
        generation: Generation::Blackwell,
        build: BUILD,
        url: "https://international.download.nvidia.com/Windows/broadcast/2.2.1/NVIDIA_Broadcast_Offline_Blackwell_v2.2.1.58338310.exe",
        size: 2_225_036_120,
        sha256: "caed3a89a0e3a680468801e818e6559b2b0358d6af217dd4996f37b2d5cb3878",
    },
    Installer {
        generation: Generation::Ada,
        build: BUILD,
        url: "https://international.download.nvidia.com/Windows/broadcast/2.2.1/NVIDIA_Broadcast_Offline_Ada_v2.2.1.58338310.exe",
        size: 2_356_726_072,
        sha256: "fd9dec23257a792e35f81b0b2785c78e5f92243149bc26ef91a15ed692eb525b",
    },
    Installer {
        generation: Generation::Ampere,
        build: BUILD,
        url: "https://international.download.nvidia.com/Windows/broadcast/2.2.1/NVIDIA_Broadcast_Offline_Ampere_v2.2.1.58338310.exe",
        size: 2_199_322_600,
        sha256: "7a272d0510841257bf27919b6c61c7aad49b3b6ffdf15d03b0392f3ee366095b",
    },
    Installer {
        generation: Generation::Turing,
        build: BUILD,
        url: "https://international.download.nvidia.com/Windows/broadcast/2.2.1/NVIDIA_Broadcast_Offline_Turing_v2.2.1.58338310.exe",
        size: 2_156_554_584,
        sha256: "b08c29ec5fe4a1e429453c8227249d94ec152147c2603a7fc9d6d8b617e2ce4b",
    },
];

pub struct Options {
    pub keep_installer: bool,
}

pub trait Ui: Progress {
    fn accept_eula(&self, eula: &Path) -> Result<bool>;
}

/// Room for the extracted files (3.3 GB for Blackwell) and the Wine prefix (0.7 GB).
const UNPACKED_SPACE: u64 = 5 << 30;

pub fn run(paths: &Paths, opts: &Options, ui: &dyn Ui) -> Result<()> {
    let gpu = gpu::detect()?;
    let installer = INSTALLERS
        .iter()
        .find(|i| i.generation == gpu.generation)
        .with_context(|| format!("no NVIDIA Broadcast installer known for {}", gpu.generation))?;
    ui.step(&format!("GPU: {} ({})", gpu.name, gpu.generation));

    let out = paths.nvidia().join(installer.build);
    match files_problem(paths, gpu.generation) {
        None => ui.step(&format!(
            "NVIDIA Broadcast {} files already installed",
            installer.build
        )),
        Some(problem) => {
            ui.step(&problem);
            let file = installer.url.rsplit('/').next().unwrap_or("installer.exe");
            let exe = paths.downloads().join(file);
            ensure_space(paths, &exe, installer.size)?;
            download::fetch(installer.url, &exe, installer.size, installer.sha256, ui)?;
            let accepted = out.join(".complete").exists();
            if out.exists() {
                fs::remove_dir_all(&out)?;
            }
            if !accepted {
                accept_eula(&exe, &out, ui)?;
            }
            extract(&exe, &out, ui)?;
            fs::write(out.join(".complete"), format!("{}\n", gpu.generation))?;
            if !opts.keep_installer {
                fs::remove_file(&exe)?;
            }
        }
    }
    for old in remove_other_builds(&paths.nvidia())? {
        ui.step(&format!(
            "Removed old NVIDIA Broadcast files in {}",
            old.display()
        ));
    }

    ui.check()?;
    prefix::create(
        &paths.prefix(),
        &paths.downloads(),
        &crate::paths::relay_dir(),
        ui,
    )?;
    ui.step(&format!("Setup complete: {}", paths.data.display()));
    Ok(())
}

fn remove_other_builds(nvidia: &Path) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    for entry in fs::read_dir(nvidia)? {
        let path = entry?.path();
        if path.is_dir()
            && path.file_name() != Some(BUILD.as_ref())
            && crate::nvidia::build_version(&path).is_some()
        {
            fs::remove_dir_all(&path)
                .with_context(|| format!("removing old NVIDIA files in {}", path.display()))?;
            removed.push(path);
        }
    }
    Ok(removed)
}

fn ensure_space(paths: &Paths, exe: &Path, size: u64) -> Result<()> {
    fs::create_dir_all(&paths.data)?;
    let downloaded = [exe.to_path_buf(), exe.with_extension("part")]
        .iter()
        .filter_map(|p| fs::metadata(p).ok())
        .map(|m| m.len())
        .max()
        .unwrap_or(0);
    let needed = size.saturating_sub(downloaded) + UNPACKED_SPACE;
    let free = free_space(&paths.data)?;
    if free < needed {
        bail!(
            "setup needs {} GB free in {}, but only {} GB is available",
            needed.div_ceil(1 << 30),
            paths.data.display(),
            free >> 30
        );
    }
    Ok(())
}

fn free_space(dir: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
    // SAFETY: `path` is NUL-terminated and `stat` is a valid out-pointer for the call.
    let stat = unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(path.as_ptr(), &raw mut stat) != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("checking free space in {}", dir.display()));
        }
        stat
    };
    Ok(stat.f_bavail * stat.f_frsize)
}

/// Why this GPU needs `setup` to (re)install NVIDIA's files, or `None` if they are current.
pub fn files_problem(paths: &Paths, generation: Generation) -> Option<String> {
    let out = paths.nvidia().join(BUILD);
    let Ok(installed) = fs::read_to_string(out.join(".complete")) else {
        return Some(format!(
            "NVIDIA Broadcast {BUILD} is not installed; run `broadcast-linux setup`"
        ));
    };
    if installed.trim() != generation.to_string() {
        return Some(format!(
            "the installed files are for {}, but this GPU is {generation}; run `broadcast-linux setup`",
            installed.trim()
        ));
    }
    let Ok(manifest) = fs::read_to_string(out.join(MANIFEST)) else {
        // Installed before the manifest existed: check everything a worker can load.
        // Reinstalling, if an asset is missing, writes the manifest.
        return Installation::find(paths)
            .and_then(|install| install.verify())
            .err()
            .map(|e| format!("{e:#}; run `broadcast-linux setup`"));
    };
    manifest.lines().find_map(|line| {
        let (size, file) = line.split_once('\t')?;
        let intact = fs::metadata(out.join(file)).is_ok_and(|m| size == m.len().to_string());
        (!intact).then(|| {
            format!("NVIDIA file {file} is missing or damaged; run `broadcast-linux setup`")
        })
    })
}

/// Why `setup` needs to run, or `None` when the files and Wine prefix are ready.
pub fn needed(paths: &Paths, generation: Generation) -> Option<String> {
    files_problem(paths, generation).or_else(|| prefix::problem(&paths.prefix()))
}

fn accept_eula(exe: &Path, out: &Path, ui: &dyn Ui) -> Result<()> {
    let eula = out.join("EULA.txt");
    ui.step("Reading NVIDIA's licence from the installer");
    sevenzip::extract_file(exe, "EULA.txt", &eula)?;
    if !ui.accept_eula(&eula)? {
        bail!("licence not accepted");
    }
    Ok(())
}

/// Shows the licence in `$PAGER` and asks on the terminal, unless `--accept-eula` was given.
pub struct Terminal {
    pub progress: progress::Terminal,
    pub preaccepted: bool,
}

impl Progress for Terminal {
    fn step(&self, message: &str) {
        self.progress.step(message);
    }

    fn advance(&self, done: u64, total: u64) {
        self.progress.advance(done, total);
    }
}

impl Ui for Terminal {
    fn accept_eula(&self, eula: &Path) -> Result<bool> {
        if self.preaccepted {
            self.step(&format!(
                "NVIDIA licence accepted via --accept-eula ({})",
                eula.display()
            ));
            return Ok(true);
        }
        if !io::stdin().is_terminal() {
            bail!(
                "NVIDIA's licence must be accepted: read {} and rerun with --accept-eula",
                eula.display()
            );
        }
        let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".into());
        if Command::new(&pager).arg(eula).status().is_err() {
            io::copy(&mut fs::File::open(eula)?, &mut io::stdout())?;
        }
        eprint!("Do you accept NVIDIA's licence above? [y/N] ");
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().lock().read_line(&mut answer)?;
        Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
    }
}

const MODELS_PREFIX: &str = "NvMaxineModels/NvModels/";
const STUDIO_LIGHT_PREFIX: &str = "NvMaxineClient/nv/vkl_";
pub const STUDIO_LIGHT_DIR: &str = "studio_light";

/// Lists every extracted file with its size, so a later check can spot missing or
/// damaged files whatever the GPU's installer contained.
const MANIFEST: &str = ".files";

fn extract(exe: &Path, out: &Path, progress: &dyn Progress) -> Result<()> {
    progress.step("Extracting NVIDIA Broadcast effect libraries and models");
    let mut files = Vec::new();
    let count = sevenzip::extract(exe, out, progress, |name| {
        let target = if let Some(rest) = name.strip_prefix(MODELS_PREFIX) {
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
        };
        files.extend(target.clone());
        target
    })?;
    if !out.join("models/nvbcast").is_dir() {
        bail!("the installer did not contain the expected NvMaxineModels/NvModels/nvbcast folder");
    }
    if !out.join(STUDIO_LIGHT_DIR).is_dir() {
        bail!(
            "the installer did not contain the Studio Light presets ({STUDIO_LIGHT_PREFIX}*.hdr)"
        );
    }
    let mut manifest = String::new();
    for file in &files {
        let meta = fs::metadata(out.join(file))?;
        if meta.is_file() {
            let _ = writeln!(manifest, "{}\t{file}", meta.len());
        }
    }
    fs::write(out.join(MANIFEST), manifest)?;
    progress.step(&format!("Extracted {count} files to {}", out.display()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_finds_missing_and_damaged_files() {
        let dir =
            std::env::temp_dir().join(format!("broadcast-linux-setup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let paths = Paths {
            data: dir.clone(),
            config: dir.join("config.toml"),
        };
        let out = paths.nvidia().join(BUILD);
        fs::create_dir_all(out.join("models/nvbcast")).unwrap();
        fs::write(out.join(".complete"), "Ada\n").unwrap();
        fs::write(out.join("models/nvbcast/a.dll"), "abc").unwrap();
        fs::write(out.join(MANIFEST), "3\tmodels/nvbcast/a.dll\n").unwrap();
        assert_eq!(files_problem(&paths, Generation::Ada), None);

        fs::write(out.join("models/nvbcast/a.dll"), "ab").unwrap();
        assert!(files_problem(&paths, Generation::Ada).is_some_and(|p| p.contains("a.dll")));
        fs::remove_file(out.join("models/nvbcast/a.dll")).unwrap();
        assert!(files_problem(&paths, Generation::Ada).is_some_and(|p| p.contains("a.dll")));

        fs::remove_file(out.join(MANIFEST)).unwrap();
        assert!(
            files_problem(&paths, Generation::Ada).is_some(),
            "no manifest, no runtime"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn removes_only_other_builds() {
        let dir =
            std::env::temp_dir().join(format!("broadcast-linux-builds-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for name in [BUILD, "2.1.0.1", "2.10.0.1", "notes"] {
            fs::create_dir_all(dir.join(name)).unwrap();
        }
        fs::write(dir.join("1.0.0.1"), "").unwrap();
        let mut removed = remove_other_builds(&dir).unwrap();
        removed.sort();
        assert_eq!(removed, [dir.join("2.1.0.1"), dir.join("2.10.0.1")]);
        assert!(dir.join(BUILD).is_dir() && dir.join("notes").is_dir());
        assert!(dir.join("1.0.0.1").is_file());
        fs::remove_dir_all(&dir).unwrap();
    }
}
