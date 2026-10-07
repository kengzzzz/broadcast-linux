use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const APP: &str = "broadcast-linux";

pub struct Paths {
    pub data: PathBuf,
    pub config: PathBuf,
}

impl Paths {
    pub fn new() -> Result<Self> {
        let home = || -> Result<PathBuf> {
            Ok(PathBuf::from(
                env::var_os("HOME").context("HOME is not set")?,
            ))
        };
        let xdg = |var: &str, fallback: &str| -> Result<PathBuf> {
            match env::var_os(var).filter(|v| !v.is_empty()) {
                Some(dir) => Ok(PathBuf::from(dir)),
                None => Ok(home()?.join(fallback)),
            }
        };
        Ok(Self {
            data: xdg("XDG_DATA_HOME", ".local/share")?.join(APP),
            config: xdg("XDG_CONFIG_HOME", ".config")?
                .join(APP)
                .join("config.toml"),
        })
    }

    pub fn downloads(&self) -> PathBuf {
        self.data.join("downloads")
    }

    pub fn nvidia(&self) -> PathBuf {
        self.data.join("nvidia")
    }

    pub fn prefix(&self) -> PathBuf {
        self.data.join("prefix")
    }
}

/// `$BROADCAST_LINUX_LIBDIR`, else `../lib/broadcast-linux` beside the binary's `bin/`.
pub fn lib_dir() -> PathBuf {
    if let Some(dir) = env::var_os("BROADCAST_LINUX_LIBDIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.join("lib").join(APP)))
        .unwrap_or_else(|| PathBuf::from("/usr/lib").join(APP))
}

/// `path` with the home directory shortened to `~`, for messages.
pub fn tilde(path: &Path) -> String {
    let home = env::var_os("HOME").map(PathBuf::from);
    match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) if !rest.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

/// `share/broadcast-linux` beside `lib/broadcast-linux`: the module config files.
pub fn share_dir() -> PathBuf {
    lib_dir()
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| PathBuf::from("/usr/share"), Path::to_path_buf)
        .join("share")
        .join(APP)
}

pub fn relay_dir() -> PathBuf {
    lib_dir().join("wine")
}

pub fn workers_dir() -> PathBuf {
    lib_dir().join("workers")
}
