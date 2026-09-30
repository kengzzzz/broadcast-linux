use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

pub fn fetch(url: &str, dest: &Path, size: u64, sha256: Option<&str>) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let partial = dest.with_extension("part");
    if !dest.exists() {
        download(url, &partial, size)?;
        fs::rename(&partial, dest)?;
    }

    let actual = fs::metadata(dest)?.len();
    if actual != size {
        fs::remove_file(dest)?;
        bail!(
            "{} has {actual} bytes, expected {size}; deleted, run setup again",
            dest.display()
        );
    }
    if let Some(expected) = sha256 {
        eprintln!("Verifying {}", dest.display());
        let digest = sha256_file(dest)?;
        if digest != expected {
            fs::remove_file(dest)?;
            bail!(
                "sha256 mismatch for {}: got {digest}, expected {expected}; deleted",
                dest.display()
            );
        }
    }
    Ok(())
}

fn download(url: &str, partial: &Path, size: u64) -> Result<()> {
    let start = fs::metadata(partial).map_or(0, |m| m.len());
    if start >= size {
        return Ok(());
    }
    eprintln!("Downloading {url}");
    let mut request = ureq::get(url);
    if start > 0 {
        eprintln!("Resuming at {} MB", start >> 20);
        request = request.header("Range", format!("bytes={start}-"));
    }
    let mut response = request
        .call()
        .with_context(|| format!("requesting {url}"))?;
    let resumed = response.status() == 206;
    let mut out = OpenOptions::new()
        .create(true)
        .append(resumed)
        .write(true)
        .truncate(!resumed)
        .open(partial)?;
    let mut done = if resumed { start } else { 0 };

    let mut body = response.body_mut().as_reader();
    let mut buf = vec![0u8; 1 << 20];
    let interactive = io::stderr().is_terminal();
    let step = if interactive { 1 } else { 10 };
    let mut last_report = done * 100 / size.max(1);
    loop {
        let n = body.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        done += n as u64;
        let percent = done * 100 / size.max(1);
        if percent >= last_report + step {
            last_report = percent;
            let end = if interactive { "\r" } else { "\n" };
            eprint!("  {percent:3}%  {} / {} MB{end}", done >> 20, size >> 20);
        }
    }
    if interactive {
        eprintln!();
    }
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}
