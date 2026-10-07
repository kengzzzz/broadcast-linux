use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};
use broadcast_linux::paths::Paths;
use broadcast_linux::progress::{Cancelled, Progress};
use broadcast_linux::setup::{self, Ui};
use eframe::egui;

pub enum Outcome {
    Done,
    Cancelled,
    Failed(String),
}

/// `setup` running on its own thread; the window reads its progress and answers the licence.
pub struct SetupTask {
    shared: Arc<Mutex<Shared>>,
    cancel: Arc<AtomicBool>,
    answer: Sender<bool>,
}

#[derive(Default)]
struct Shared {
    step: String,
    progress: Option<(u64, u64)>,
    eula: Option<String>,
    outcome: Option<Outcome>,
}

impl SetupTask {
    pub fn start(ctx: egui::Context) -> Self {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let (answer, answers) = mpsc::channel();
        let ui = GuiUi {
            shared: Arc::clone(&shared),
            cancel: Arc::clone(&cancel),
            answers: Mutex::new(answers),
            ctx: ctx.clone(),
        };
        thread::spawn(move || {
            let result = Paths::new().and_then(|paths| {
                let opts = setup::Options {
                    keep_installer: false,
                };
                setup::run(&paths, &opts, &ui)
            });
            let outcome = match result {
                Ok(()) => Outcome::Done,
                Err(e) if e.chain().any(<dyn std::error::Error>::is::<Cancelled>) => {
                    Outcome::Cancelled
                }
                Err(e) => Outcome::Failed(format!("{e:#}")),
            };
            ui.shared.lock().unwrap().outcome = Some(outcome);
            ctx.request_repaint();
        });
        Self {
            shared,
            cancel,
            answer,
        }
    }

    pub fn step(&self) -> String {
        self.shared.lock().unwrap().step.clone()
    }

    /// Bytes done and total for the current step, if it reports them.
    pub fn progress(&self) -> Option<(u64, u64)> {
        self.shared.lock().unwrap().progress
    }

    /// The licence text while setup waits for an answer.
    pub fn eula(&self) -> Option<String> {
        self.shared.lock().unwrap().eula.clone()
    }

    pub fn answer_eula(&self, accepted: bool) {
        self.shared.lock().unwrap().eula = None;
        let _ = self.answer.send(accepted);
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn take_outcome(&self) -> Option<Outcome> {
        self.shared.lock().unwrap().outcome.take()
    }
}

struct GuiUi {
    shared: Arc<Mutex<Shared>>,
    cancel: Arc<AtomicBool>,
    answers: Mutex<Receiver<bool>>,
    ctx: egui::Context,
}

impl Progress for GuiUi {
    fn step(&self, message: &str) {
        let mut shared = self.shared.lock().unwrap();
        message.clone_into(&mut shared.step);
        shared.progress = None;
        self.ctx.request_repaint();
    }

    fn advance(&self, done: u64, total: u64) {
        let mut shared = self.shared.lock().unwrap();
        let before = shared.progress.map_or(0, |(d, t)| d * 1000 / t.max(1));
        shared.progress = Some((done, total));
        if done * 1000 / total.max(1) != before {
            self.ctx.request_repaint();
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

impl Ui for GuiUi {
    fn accept_eula(&self, eula: &Path) -> Result<bool> {
        let text = std::fs::read_to_string(eula)?;
        self.shared.lock().unwrap().eula = Some(text);
        self.ctx.request_repaint();
        let answers = self.answers.lock().unwrap();
        loop {
            match answers.recv_timeout(Duration::from_millis(200)) {
                Ok(accepted) => return Ok(accepted),
                Err(RecvTimeoutError::Timeout) if self.cancelled() => {
                    return Err(Cancelled.into());
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => bail!("the window closed"),
            }
        }
    }
}
