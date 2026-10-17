use std::cell::Cell;
use std::fmt;
use std::io::{self, IsTerminal, Read, Write};

use anyhow::Result;

/// The error a long step returns after `Progress::cancelled`; test with `err.is::<Cancelled>()`.
#[derive(Debug)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

pub trait Progress {
    /// Starts a new step; any `advance` after it refers to this step.
    fn step(&self, message: &str);
    fn advance(&self, done: u64, total: u64);
    fn cancelled(&self) -> bool {
        false
    }

    fn check(&self) -> Result<()> {
        if self.cancelled() {
            return Err(Cancelled.into());
        }
        Ok(())
    }
}

/// `done` carries over between calls, so several copies can report one total.
pub fn copy(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    done: &mut u64,
    total: u64,
    progress: &dyn Progress,
) -> Result<()> {
    let mut buf = vec![0u8; 1 << 20];
    loop {
        progress.check()?;
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        writer.write_all(&buf[..n])?;
        *done += n as u64;
        progress.advance(*done, total);
    }
}

/// Prints steps and percentages to stderr: every 1% on a terminal, every 10% in logs.
pub struct Terminal {
    interactive: bool,
    last: Cell<Option<u64>>,
    timed: bool,
}

impl Terminal {
    /// Shows `advance` amounts as megabytes.
    pub fn new() -> Self {
        Self {
            interactive: io::stderr().is_terminal(),
            last: Cell::new(None),
            timed: false,
        }
    }

    /// Shows `advance` amounts, which are milliseconds of media, as minutes and seconds.
    pub fn timed() -> Self {
        let mut terminal = Self::new();
        terminal.timed = true;
        terminal
    }

    /// Ends the percentage line before other output.
    pub fn end_line(&self) {
        if self.last.take().is_some() && self.interactive {
            eprintln!();
        }
    }
}

impl Default for Terminal {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.end_line();
    }
}

impl Progress for Terminal {
    fn step(&self, message: &str) {
        self.end_line();
        eprintln!("{message}");
    }

    fn advance(&self, done: u64, total: u64) {
        let percent = done * 100 / total.max(1);
        let every = if self.interactive { 1 } else { 10 };
        let last = self.last.get();
        let soon = done < total && last.is_some_and(|last| percent < last + every);
        if soon || last == Some(percent) {
            return;
        }
        self.last.set(Some(percent));
        let end = if self.interactive { "\r" } else { "\n" };
        let amount = if self.timed {
            format!("{} / {}", clock(done), clock(total))
        } else {
            format!("{} / {} MB", done >> 20, total >> 20)
        };
        eprint!("  {percent:3}%  {amount}{end}");
    }
}

fn clock(ms: u64) -> String {
    let seconds = ms / 1000;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}
