use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Component, Path};

use anyhow::{Context, Result, bail};

const SIGNATURE: &[u8] = b"7z\xbc\xaf\x27\x1c";

/// Reads the 7z archive after the Windows stub in NVIDIA's self-extracting installers.
struct Embedded<R> {
    inner: R,
    base: u64,
}

impl<R: Read> Read for Embedded<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<R: Seek> Seek for Embedded<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let pos = match pos {
            SeekFrom::Start(p) => SeekFrom::Start(p + self.base),
            other => other,
        };
        Ok(self.inner.seek(pos)? - self.base)
    }
}

fn archive_offset(path: &Path) -> Result<u64> {
    let mut head = vec![0u8; 8 << 20];
    let n = File::open(path)?.read(&mut head)?;
    head[..n]
        .windows(SIGNATURE.len())
        .position(|w| w == SIGNATURE)
        .map(|p| p as u64)
        .with_context(|| format!("{} does not contain a 7z archive", path.display()))
}

pub fn extract(
    installer: &Path,
    out: &Path,
    mut select: impl FnMut(&str) -> Option<String>,
) -> Result<usize> {
    let base = archive_offset(installer)?;
    let mut file = BufReader::new(File::open(installer)?);
    file.seek(SeekFrom::Start(base))?;
    let mut reader = sevenz_rust2::ArchiveReader::new(
        Embedded { inner: file, base },
        sevenz_rust2::Password::empty(),
    )
    .with_context(|| format!("reading the archive in {}", installer.display()))?;

    let mut written = 0;
    reader.for_each_entries(|entry, data| {
        let name = entry.name().replace('\\', "/");
        let target = match select(&name) {
            Some(target) if !entry.is_directory() => target,
            _ => {
                io::copy(data, &mut io::sink())?;
                return Ok(true);
            }
        };
        if let Err(e) = safe_relative(&target) {
            return Err(io::Error::other(e.to_string()).into());
        }
        let dest = out.join(&target);
        fs::create_dir_all(dest.parent().unwrap_or(out))?;
        io::copy(data, &mut File::create(&dest)?)?;
        written += 1;
        Ok(true)
    })?;
    Ok(written)
}

pub fn safe_relative(path: &str) -> Result<&str> {
    let p = Path::new(path);
    if p.components().any(|c| !matches!(c, Component::Normal(_))) {
        bail!("refusing unsafe archive path {path}");
    }
    Ok(path)
}
