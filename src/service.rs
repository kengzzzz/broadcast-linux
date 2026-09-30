use std::cell::RefCell;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use pipewire as pw;
use pw::loop_::Signal;

use crate::camera::{self, Camera};
use crate::config::Config;
use crate::mic::Mic;
use crate::paths::Paths;

#[derive(Clone, Copy)]
pub enum Event {
    LinkAdded { id: u32, output_node: u32 },
    LinkRemoved(u32),
    IdleTimeout(u64),
    UnloadTimeout(u64),
    MicWorkerReady(u64),
    MicWorkerExited(u64),
    MicRestart,
    Reload,
    Quit,
}

pub type EventSender = pw::channel::Sender<Event>;

struct State {
    paths: Paths,
    config: Config,
    mic: Option<Mic>,
    camera: Option<Camera>,
    events: EventSender,
    mainloop: pw::main_loop::MainLoopRc,
    /// Bumped whenever a pending idle timeout should be ignored.
    idle_token: u64,
}

pub fn run() -> Result<()> {
    let paths = Paths::new()?;
    let config = Config::load(&paths.config)?;

    block_handled_signals();
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let (events, receiver) = pw::channel::channel::<Event>();

    let registry = core.get_registry_rc()?;
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let events = events.clone();
            move |global| {
                if global.type_ != pw::types::ObjectType::Link {
                    return;
                }
                let output_node = global
                    .props
                    .and_then(|p| p.get("link.output.node"))
                    .and_then(|v| v.parse().ok());
                if let Some(output_node) = output_node {
                    let _ = events.send(Event::LinkAdded {
                        id: global.id,
                        output_node,
                    });
                }
            }
        })
        .global_remove({
            let events = events.clone();
            move |id| {
                let _ = events.send(Event::LinkRemoved(id));
            }
        })
        .register();

    let signal = |signal, event: fn() -> Event| {
        let events = events.clone();
        mainloop.loop_().add_signal_local(signal, move || {
            let _ = events.send(event());
        })
    };
    let _hup = signal(Signal::HUP, || Event::Reload);
    let _term = signal(Signal::TERM, || Event::Quit);
    let _int = signal(Signal::INT, || Event::Quit);

    let mic = if config.mic.enabled {
        Some(Mic::new(&core, config.mic.clone(), events.clone())?)
    } else {
        None
    };
    let camera = if config.camera.enabled {
        let idle = Duration::from_secs(config.service.idle_timeout_seconds);
        match Camera::start(Paths::new()?, config.camera.clone(), idle) {
            Ok(camera) => Some(camera),
            Err(e) => {
                eprintln!("camera: disabled: {e:#}");
                None
            }
        }
    } else {
        None
    };
    let status = |on: bool, device: &str| {
        format!("; {device} {}", if on { "available" } else { "disabled" })
    };
    eprintln!(
        "broadcast-linux running (config {}){}{}",
        paths.config.display(),
        status(mic.is_some(), "mic"),
        status(camera.is_some(), "camera"),
    );
    notify_ready();

    let state = RefCell::new(State {
        paths,
        config,
        mic,
        camera,
        events: events.clone(),
        mainloop: mainloop.clone(),
        idle_token: 0,
    });
    let _receiver = receiver.attach(mainloop.loop_(), move |event| {
        state.borrow_mut().handle(event);
    });
    mainloop.run();
    Ok(())
}

/// Tells systemd (Type=notify) that the devices exist. Ordered before WirePlumber,
/// this makes the loopback camera already look like a camera when it is probed.
fn notify_ready() {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};

    let Some(target) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let target = target.to_string_lossy().into_owned();
    let addr = match target.strip_prefix('@') {
        Some(name) => SocketAddr::from_abstract_name(name.as_bytes()),
        None => SocketAddr::from_pathname(&target),
    };
    let sent = addr.and_then(|addr| UnixDatagram::unbound()?.send_to_addr(b"READY=1", &addr));
    if let Err(e) = sent {
        eprintln!("could not notify systemd: {e}");
    }
}

/// PipeWire's loop receives signals through a signalfd, which only works if no
/// thread can take the signal first. Blocking them before any thread exists makes
/// every later thread (PipeWire's and ours) inherit the block.
fn block_handled_signals() {
    // SAFETY: plain libc calls on a locally owned sigset, before other threads start.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut set);
        for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
            libc::sigaddset(&raw mut set, signal);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut());
    }
}

impl State {
    fn handle(&mut self, event: Event) {
        match event {
            Event::LinkAdded { id, output_node } => {
                if self
                    .mic
                    .as_mut()
                    .is_some_and(|m| m.link_added(id, output_node))
                {
                    self.readers_changed();
                }
            }
            Event::LinkRemoved(id) => {
                if self.mic.as_mut().is_some_and(|m| m.link_removed(id)) {
                    self.readers_changed();
                }
            }
            Event::IdleTimeout(token) => {
                if token == self.idle_token
                    && let Some(mic) = self.mic.as_mut().filter(|m| m.readers() == 0)
                    && mic.idle()
                {
                    let delay = mic.unload_after();
                    self.send_after(delay, Event::UnloadTimeout(token));
                }
            }
            Event::UnloadTimeout(token) => {
                if token == self.idle_token
                    && let Some(mic) = self.mic.as_mut().filter(|m| m.readers() == 0)
                    && mic.has_session()
                {
                    eprintln!(
                        "mic: unloading after {} min unused",
                        mic.unload_after().as_secs() / 60
                    );
                    mic.stop();
                }
            }
            Event::MicWorkerReady(session) => {
                if let Some(mic) = self.mic.as_mut() {
                    mic.worker_ready(session);
                }
            }
            Event::MicWorkerExited(session) => {
                if let Some(delay) = self.mic.as_mut().and_then(|m| m.worker_exited(session)) {
                    self.send_after(delay, Event::MicRestart);
                }
            }
            Event::MicRestart => {
                if let Some(mic) = self.mic.as_mut().filter(|m| m.readers() > 0) {
                    mic.start(&self.paths);
                }
            }
            Event::Reload => self.reload(),
            Event::Quit => {
                if let Some(mic) = self.mic.as_mut() {
                    mic.stop();
                }
                // Joins the camera thread; it never sends events, so this can't deadlock.
                drop(self.camera.take());
                self.mainloop.quit();
            }
        }
    }

    fn readers_changed(&mut self) {
        let Some(mic) = self.mic.as_mut() else { return };
        self.idle_token += 1;
        eprintln!("mic: {} app(s) reading", mic.readers());
        if mic.readers() > 0 {
            mic.start(&self.paths);
        } else if mic.has_session() {
            let timeout = Duration::from_secs(self.config.service.idle_timeout_seconds);
            self.send_after(timeout, Event::IdleTimeout(self.idle_token));
        }
    }

    fn reload(&mut self) {
        let config = match Config::load(&self.paths.config) {
            Ok(config) => config,
            Err(e) => {
                eprintln!("reload: keeping the current settings: {e:#}");
                return;
            }
        };
        eprintln!("reload: {}", self.paths.config.display());
        if let Some(mic) = self.mic.as_mut()
            && mic.set_config(config.mic.clone())
        {
            mic.stop();
            if mic.readers() > 0 {
                mic.start(&self.paths);
            }
        }
        if let Some(camera) = &self.camera {
            camera.send(camera::Control::Reload {
                config: config.camera.clone(),
                idle_timeout: Duration::from_secs(config.service.idle_timeout_seconds),
            });
        }
        self.config = config;
    }

    fn send_after(&self, delay: Duration, event: Event) {
        let events = self.events.clone();
        thread::spawn(move || {
            thread::sleep(delay);
            let _ = events.send(event);
        });
    }
}
