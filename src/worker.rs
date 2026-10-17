use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::paths;

pub struct Launch {
    pub program: &'static str,
    pub prefix: PathBuf,
    /// Folder with the NVIDIA DLLs; the worker loads them from its working directory.
    pub cwd: PathBuf,
    pub args: Vec<String>,
    pub stdin: Option<Stdio>,
}

pub struct Pipes {
    pub stdin: Option<ChildStdin>,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

/// Owns the worker's process group; Wine's launcher may hand off to another process.
pub struct Worker {
    child: Child,
}

impl Worker {
    pub fn spawn(launch: Launch) -> Result<(Self, Pipes)> {
        let workers = paths::workers_dir();
        let program = workers.join(format!("{}.exe.so", launch.program));
        if !program.exists() {
            anyhow::bail!(
                "worker {} not found (set BROADCAST_LINUX_LIBDIR)",
                program.display()
            );
        }
        let dll_path = format!("{}:{}", workers.display(), paths::relay_dir().display());
        let mut child = Command::new("wine");
        child.envs(session_display());
        let mut child = child
            .arg(&program)
            .args(&launch.args)
            .current_dir(&launch.cwd)
            .env("WINEPREFIX", &launch.prefix)
            .env("WINEDLLPATH", dll_path)
            .env("WINEDLLOVERRIDES", "nvcuda=b;nvapi64=n;dxgi=n;d3d11=n")
            .env("DXVK_ENABLE_NVAPI", "1")
            .env("DXVK_LOG_LEVEL", "none")
            .env("DXVK_NVAPI_LOG_LEVEL", "none")
            .env("WINEDEBUG", "-all")
            .stdin(launch.stdin.unwrap_or_else(Stdio::piped))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .context("starting wine")?;
        let pipes = Pipes {
            stdin: child.stdin.take(),
            stdout: child.stdout.take().expect("piped stdout"),
            stderr: child.stderr.take().expect("piped stderr"),
        };
        Ok((Self { child }, pipes))
    }

    /// `None` while the worker runs.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        Ok(self.child.try_wait()?)
    }

    fn signal_group(&self, signal: libc::c_int) {
        let Ok(pgid) = libc::pid_t::try_from(self.child.id()) else {
            return;
        };
        // SAFETY: plain syscall on the process group this struct created.
        unsafe {
            libc::killpg(pgid, signal);
        }
    }
}

const DISPLAY_VARS: [&str; 3] = ["DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY"];

/// DXVK needs a real display. At boot the service starts before the desktop exports
/// one, so read it from the user manager per worker, else from our own environment.
fn session_display() -> Vec<(String, String)> {
    let vars = manager_display();
    if !has_display(&vars)
        && !["DISPLAY", "WAYLAND_DISPLAY"]
            .iter()
            .any(|name| std::env::var_os(name).is_some())
    {
        eprintln!("no DISPLAY or WAYLAND_DISPLAY in the session; NVIDIA effects need one to load");
    }
    vars
}

/// Unlike `BufRead::lines`, a line that isn't UTF-8 doesn't end the reading, so the pipe never
/// fills up and stalls the worker.
pub fn log_lines(stderr: impl Read) -> impl Iterator<Item = String> {
    let mut reader = BufReader::new(stderr);
    let mut bytes = Vec::new();
    std::iter::from_fn(move || {
        bytes.clear();
        match reader.read_until(b'\n', &mut bytes) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(
                String::from_utf8_lossy(&bytes)
                    .trim_end_matches(['\n', '\r'])
                    .to_owned(),
            ),
        }
    })
}

pub(crate) fn has_display(vars: &[(String, String)]) -> bool {
    vars.iter()
        .any(|(name, _)| name == "DISPLAY" || name == "WAYLAND_DISPLAY")
}

/// The display variables the user manager currently exports.
pub(crate) fn manager_display() -> Vec<(String, String)> {
    let manager = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    manager
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(name, _)| DISPLAY_VARS.contains(name))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.signal_group(libc::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                // The launcher may have exited while the program lives on.
                self.signal_group(libc::SIGKILL);
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.signal_group(libc::SIGKILL);
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_past_lines_that_are_not_utf8() {
        let stderr: &[u8] = b"one\r\nVid\xe9os\nthree";
        let lines: Vec<String> = log_lines(stderr).collect();
        assert_eq!(lines, ["one", "Vid\u{fffd}os", "three"]);
    }
}
