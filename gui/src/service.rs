use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use broadcast_linux::status::{Client, Reloader, Status};
use eframe::egui;

const UNIT: &str = "broadcast-linux";

/// Follows the service's status socket, reconnecting whenever the service restarts.
#[derive(Clone)]
pub struct Link {
    status: Arc<Mutex<Option<Status>>>,
    reloader: Arc<Mutex<Option<Reloader>>>,
    /// Counts connections, so a restart can wait for the new service.
    connections: Arc<AtomicU64>,
}

impl Link {
    pub fn start(ctx: egui::Context) -> Self {
        let link = Self {
            status: Arc::default(),
            reloader: Arc::default(),
            connections: Arc::default(),
        };
        let shared = link.clone();
        thread::spawn(move || {
            loop {
                if let Ok(mut client) = Client::connect() {
                    *shared.reloader.lock().unwrap() = client.reloader().ok();
                    shared.connections.fetch_add(1, Ordering::SeqCst);
                    while let Ok(status) = client.recv() {
                        *shared.status.lock().unwrap() = Some(status);
                        ctx.request_repaint();
                    }
                    *shared.reloader.lock().unwrap() = None;
                    *shared.status.lock().unwrap() = None;
                    ctx.request_repaint();
                }
                thread::sleep(Duration::from_millis(250));
            }
        });
        link
    }

    pub fn status(&self) -> Option<Status> {
        self.status.lock().unwrap().clone()
    }

    /// Waits for the service's answer.
    pub fn reload(&self) -> Result<()> {
        let before = self
            .status()
            .context("the service is not connected")?
            .reloads;
        self.reloader
            .lock()
            .unwrap()
            .as_mut()
            .context("the service is not connected")?
            .reload()?;
        let status = self
            .wait(Duration::from_secs(5), |s| s.reloads > before)
            .context("the service did not answer the reload")?;
        if let Some(error) = status.reload_error {
            bail!("the service kept its previous settings: {error}");
        }
        Ok(())
    }

    /// Waits until the new service reports in.
    pub fn restart(&self) -> Result<()> {
        let before = self.connections.load(Ordering::SeqCst);
        systemctl(&["restart", UNIT]).map_err(with_journal)?;
        self.wait(Duration::from_secs(10), |_| {
            self.connections.load(Ordering::SeqCst) > before
        })
        .context("the service restarted but did not report back; it may be an older version")?;
        Ok(())
    }

    fn wait(&self, timeout: Duration, ready: impl Fn(&Status) -> bool) -> Option<Status> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(status) = self.status().filter(&ready) {
                return Some(status);
            }
            thread::sleep(Duration::from_millis(50));
        }
        None
    }
}

pub fn systemctl(args: &[&str]) -> Result<()> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .context("running systemctl")?;
    if !out.status.success() {
        bail!(
            "systemctl --user {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub fn is_active() -> bool {
    systemctl(&["is-active", "--quiet", UNIT]).is_ok()
}

pub fn is_enabled() -> bool {
    systemctl(&["is-enabled", "--quiet", UNIT]).is_ok()
}

pub fn start() -> Result<()> {
    systemctl(&["enable", "--now", UNIT]).map_err(with_journal)
}

pub fn stop() -> Result<()> {
    systemctl(&["stop", UNIT])
}

pub fn journal(lines: usize) -> String {
    Command::new("journalctl")
        .args(["--user", "-u", UNIT, "-b", "-o", "cat", "--no-pager", "-n"])
        .arg(lines.to_string())
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default()
}

fn with_journal(e: anyhow::Error) -> anyhow::Error {
    let log = journal(8);
    if log.trim().is_empty() {
        return e;
    }
    anyhow::anyhow!("{e:#}\n\nRecent service log:\n{}", log.trim_end())
}
