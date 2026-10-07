use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;

use crate::download;
use crate::progress::Progress;

struct Release {
    url: &'static str,
    size: u64,
    sha256: &'static str,
    dlls: &'static [&'static str],
}

const DXVK: Release = Release {
    url: "https://github.com/doitsujin/dxvk/releases/download/v3.1.1/dxvk-3.1.1.tar.gz",
    size: 18_041_512,
    sha256: "40565b4a724aadc4433fa4e010b4b23916d9b1f1baeee64e17186db94f54e608",
    dlls: &["dxgi.dll", "d3d11.dll"],
};

const DXVK_NVAPI: Release = Release {
    url: "https://github.com/jp7677/dxvk-nvapi/releases/download/v0.9.2/dxvk-nvapi-v0.9.2.tar.gz",
    size: 4_926_359,
    sha256: "60c284223530d643c446c263f1e1a96c6de7b5ff21796219646da734d97a70d6",
    dlls: &["nvapi64.dll"],
};

pub fn create(
    prefix: &Path,
    downloads: &Path,
    relay: &Path,
    progress: &dyn Progress,
) -> Result<()> {
    let created = !prefix.join("system.reg").exists();
    if created {
        progress.step(&format!("Creating Wine prefix {}", prefix.display()));
        fs::create_dir_all(prefix)?;
        let status = Command::new("wineboot")
            .arg("-u")
            .env("WINEPREFIX", prefix)
            .env("WINEDEBUG", "-all")
            .env("WINEDLLOVERRIDES", "mscoree,mshtml=")
            .status()
            .context("running wineboot (is Wine installed?)")?;
        if !status.success() {
            bail!("wineboot failed with {status}");
        }
    }

    let system32 = prefix.join("drive_c/windows/system32");

    // Wine loads the relay's builtin nvcuda only if a placeholder exists in system32.
    let stub = relay.join("x86_64-windows/nvcuda.dll");
    if !stub.exists() || !relay.join("x86_64-unix/nvcuda.dll.so").exists() {
        bail!(
            "patched nvcuda relay not found in {} (set BROADCAST_LINUX_LIBDIR)",
            relay.display()
        );
    }
    write_if_changed(&system32.join("nvcuda.dll"), &fs::read(&stub)?)?;
    for release in [&DXVK, &DXVK_NVAPI] {
        let name = release.url.rsplit('/').next().unwrap_or("release.tar.gz");
        let archive = downloads.join(name);
        download::fetch(
            release.url,
            &archive,
            release.size,
            release.sha256,
            progress,
        )?;
        install_dlls(&archive, &system32, release.dlls)?;
    }

    // Let a new prefix's background processes settle before the first worker. In an
    // existing prefix this would also wait for the service's running workers.
    if created {
        Command::new("wineserver")
            .arg("-w")
            .env("WINEPREFIX", prefix)
            .status()
            .context("running wineserver -w")?;
    }
    Ok(())
}

/// What is missing from the prefix, or `None` when it is complete.
pub fn problem(prefix: &Path) -> Option<String> {
    let system32 = prefix.join("drive_c/windows/system32");
    let complete = prefix.join("system.reg").exists()
        && ["nvcuda.dll", "dxgi.dll", "d3d11.dll", "nvapi64.dll"]
            .iter()
            .all(|dll| system32.join(dll).exists());
    (!complete).then(|| {
        format!(
            "{} is incomplete; run `broadcast-linux setup`",
            prefix.display()
        )
    })
}

/// Leaves identical files alone: a running worker may have them mapped.
fn write_if_changed(path: &Path, data: &[u8]) -> Result<()> {
    if fs::read(path).is_ok_and(|current| current == data) {
        return Ok(());
    }
    fs::write(path, data).with_context(|| format!("writing {}", path.display()))
}

fn install_dlls(archive: &Path, system32: &Path, dlls: &[&str]) -> Result<()> {
    let mut remaining: Vec<&str> = dlls.to_vec();
    let mut tar = tar::Archive::new(GzDecoder::new(File::open(archive)?));
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let Some(file) = path.file_name().and_then(|f| f.to_str()) else {
            continue;
        };
        let in_x64 = path
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|d| d == "x64");
        if let Some(pos) = remaining.iter().position(|d| *d == file).filter(|_| in_x64) {
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            write_if_changed(&system32.join(file), &data)?;
            remaining.swap_remove(pos);
        }
    }
    if !remaining.is_empty() {
        bail!("{} is missing {}", archive.display(), remaining.join(", "));
    }
    Ok(())
}
