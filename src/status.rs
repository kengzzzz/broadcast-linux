use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use std::{env, fs};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Bumped when a field changes meaning; added fields keep the number.
pub const PROTOCOL: u32 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    #[default]
    Disabled,
    /// Enabled, but the device could not be created; see `error`.
    Unavailable,
    Idle,
    Loading,
    Running,
    /// No app is reading; the model stays loaded until `unload_after_minutes`.
    Paused,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Device {
    pub state: State,
    pub readers: usize,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Status {
    pub protocol: u32,
    pub version: String,
    /// Counts handled reloads, so a client can wait for the one it asked for.
    pub reloads: u64,
    pub reload_error: Option<String>,
    /// The config on disk changes settings that only apply after a restart.
    pub restart_pending: bool,
    pub mic: Device,
    pub speaker: Device,
    pub camera: Device,
}

pub fn socket_path() -> Option<PathBuf> {
    let dir = env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty())?;
    Some(PathBuf::from(dir).join("broadcast-linux.sock"))
}

/// Returned by `Server::start` when another service already answers on the socket.
#[derive(Debug)]
pub struct AlreadyRunning;

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "broadcast-linux is already running; stop it first (systemctl --user stop broadcast-linux)",
        )
    }
}

impl std::error::Error for AlreadyRunning {}

/// Sends the newest status to every connected client, one JSON object per line.
pub struct Server {
    clients: Arc<Mutex<Vec<UnixStream>>>,
    current: Arc<Mutex<String>>,
    path: PathBuf,
    last: Option<Status>,
}

impl Server {
    /// `reload` runs on a client thread whenever a client sends `reload`.
    pub fn start(reload: impl Fn() + Send + Sync + 'static) -> Result<Self> {
        let path = socket_path().context("XDG_RUNTIME_DIR is not set")?;
        if UnixStream::connect(&path).is_ok() {
            return Err(AlreadyRunning.into());
        }
        let _ = fs::remove_file(&path);
        let listener =
            UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
        let clients = Arc::new(Mutex::new(Vec::new()));
        let current = Arc::new(Mutex::new(String::new()));
        let reload = Arc::new(reload);
        thread::spawn({
            let clients = Arc::clone(&clients);
            let current = Arc::clone(&current);
            move || {
                for stream in listener.incoming().flatten() {
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
                    // Holding `current` while registering keeps the client from missing
                    // an update published in between.
                    let line = current.lock().unwrap();
                    let mut writer = &stream;
                    if writer.write_all(line.as_bytes()).is_err() {
                        continue;
                    }
                    let Ok(reader) = stream.try_clone() else {
                        continue;
                    };
                    clients.lock().unwrap().push(stream);
                    drop(line);
                    let reload = Arc::clone(&reload);
                    thread::spawn(move || {
                        for command in BufReader::new(reader).lines() {
                            match command.as_deref().map(str::trim) {
                                Ok("reload") => reload(),
                                Ok(_) => {}
                                Err(_) => break,
                            }
                        }
                    });
                }
            }
        });
        Ok(Self {
            clients,
            current,
            path,
            last: None,
        })
    }

    pub fn publish(&mut self, status: &Status) {
        if self.last.as_ref() == Some(status) {
            return;
        }
        self.last = Some(status.clone());
        let Ok(mut line) = serde_json::to_string(status) else {
            return;
        };
        line.push('\n');
        let mut current = self.current.lock().unwrap();
        self.clients
            .lock()
            .unwrap()
            .retain(|mut client| client.write_all(line.as_bytes()).is_ok());
        *current = line;
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    pub fn connect() -> Result<Self> {
        let path = socket_path().context("XDG_RUNTIME_DIR is not set")?;
        let writer = UnixStream::connect(&path)
            .with_context(|| format!("connecting to {}", path.display()))?;
        Ok(Self {
            reader: BufReader::new(writer.try_clone()?),
            writer,
        })
    }

    /// Blocks until the service sends the next status; the first is the current one.
    pub fn recv(&mut self) -> Result<Status> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(std::io::Error::from(ErrorKind::UnexpectedEof))
                .context("the service closed the status socket");
        }
        Ok(serde_json::from_str(&line)?)
    }

    /// A write-only handle, for asking to reload while another thread waits in `recv`.
    pub fn reloader(&self) -> Result<Reloader> {
        Ok(Reloader(self.writer.try_clone()?))
    }
}

pub struct Reloader(UnixStream);

impl Reloader {
    pub fn reload(&mut self) -> Result<()> {
        self.0.write_all(b"reload\n")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_and_newer_peers_parse() {
        let status: Status =
            serde_json::from_str(r#"{"protocol":1,"version":"0.4.0","future":true}"#).unwrap();
        assert_eq!(status.version, "0.4.0");
        assert_eq!(status.mic.state, State::Disabled);
        let line = serde_json::to_string(&Status::default()).unwrap();
        assert_eq!(
            serde_json::from_str::<Status>(&line).unwrap(),
            Status::default()
        );
    }
}
