use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::progress::{self, Progress};

pub fn fetch(
    url: &str,
    dest: &Path,
    size: u64,
    sha256: &str,
    progress: &dyn Progress,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let partial = dest.with_extension("part");
    if !dest.exists() {
        download(url, &partial, size, progress)?;
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
    progress.step(&format!("Verifying {}", dest.display()));
    let digest = sha256_file(dest, progress)?;
    if digest != sha256 {
        fs::remove_file(dest)?;
        bail!(
            "sha256 mismatch for {}: got {digest}, expected {sha256}; deleted",
            dest.display()
        );
    }
    Ok(())
}

fn download(url: &str, partial: &Path, size: u64, progress: &dyn Progress) -> Result<()> {
    let start = fs::metadata(partial).map_or(0, |m| m.len());
    if start >= size {
        return Ok(());
    }
    progress.step(&format!("Downloading {url}"));
    let mut request = ureq::get(url);
    if start > 0 {
        progress.step(&format!("Resuming at {} MB", start >> 20));
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
    progress::copy(
        &mut response.body_mut().as_reader(),
        &mut out,
        &mut done,
        size,
        progress,
    )
}

fn sha256_file(path: &Path, progress: &dyn Progress) -> Result<String> {
    let mut file = File::open(path)?;
    let total = file.metadata()?.len();
    let mut hasher = Sha256::new();
    let mut done = 0;
    progress::copy(
        &mut file,
        &mut HashWriter(&mut hasher),
        &mut done,
        total,
        progress,
    )?;
    Ok(hex::encode(hasher.finalize()))
}

struct HashWriter<'a>(&'a mut Sha256);

impl io::Write for HashWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
