use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Component, Path};

use anyhow::{Context, Result, bail};

use crate::progress::{self, Progress};

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

fn open(installer: &Path) -> Result<sevenz_rust2::ArchiveReader<Embedded<BufReader<File>>>> {
    let base = archive_offset(installer)?;
    let mut file = BufReader::new(File::open(installer)?);
    file.seek(SeekFrom::Start(base))?;
    sevenz_rust2::ArchiveReader::new(
        Embedded { inner: file, base },
        sevenz_rust2::Password::empty(),
    )
    .with_context(|| format!("reading the archive in {}", installer.display()))
}

/// Decodes only the archive data up to `name`, not the whole installer.
pub fn extract_file(installer: &Path, name: &str, dest: &Path) -> Result<()> {
    let data = open(installer)?
        .read_file(name)
        .with_context(|| format!("{} has no {name}", installer.display()))?;
    fs::create_dir_all(dest.parent().unwrap_or(Path::new(".")))?;
    fs::write(dest, data)?;
    Ok(())
}

pub fn extract(
    installer: &Path,
    out: &Path,
    progress: &dyn Progress,
    mut select: impl FnMut(&str) -> Option<String>,
) -> Result<usize> {
    let mut reader = open(installer)?;
    let total = reader
        .archive()
        .files
        .iter()
        .map(sevenz_rust2::ArchiveEntry::size)
        .sum();

    let mut done = 0;
    let mut written = 0;
    // sevenz_rust2 only stops the current block when the callback returns false,
    // so failures and cancelling leave through an error and are kept here.
    let mut failure = None;
    let result = reader.for_each_entries(|entry, data| {
        let mut step = || -> Result<()> {
            let name = entry.name().replace('\\', "/");
            match select(&name).filter(|_| !entry.is_directory()) {
                Some(target) => {
                    let dest = out.join(safe_relative(&target)?);
                    fs::create_dir_all(dest.parent().unwrap_or(out))?;
                    let mut file = File::create(&dest)?;
                    progress::copy(data, &mut file, &mut done, total, progress)?;
                    written += 1;
                }
                None => progress::copy(data, &mut io::sink(), &mut done, total, progress)?,
            }
            Ok(())
        };
        match step() {
            Ok(()) => Ok(true),
            Err(e) => {
                failure = Some(e);
                Err(io::Error::other("stopped").into())
            }
        }
    });
    if let Some(e) = failure {
        return Err(e);
    }
    result?;
    Ok(written)
}

pub fn safe_relative(path: &str) -> Result<&str> {
    let p = Path::new(path);
    if p.components().any(|c| !matches!(c, Component::Normal(_))) {
        bail!("refusing unsafe archive path {path}");
    }
    Ok(path)
}
