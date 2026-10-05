use std::cell::RefCell;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use pipewire as pw;
use pw::loop_::Signal;

use crate::audio::{AudioDevice, Kind, Settings};
use crate::camera::{self, Camera};
use crate::config::Config;
use crate::paths::Paths;

#[derive(Clone, Copy)]
pub enum Event {
    LinkAdded {
        id: u32,
        output_node: u32,
        input_node: u32,
    },
    LinkRemoved(u32),
    IdleTimeout(Kind, u64),
    UnloadTimeout(Kind, u64),
    WorkerReady(Kind, u64),
    WorkerExited(Kind, u64),
    Restart(Kind),
    Reload,
    Quit,
}

pub type EventSender = pw::channel::Sender<Event>;

struct State {
    paths: Paths,
    config: Config,
    audio: Vec<AudioDevice>,
    camera: Option<Camera>,
    events: EventSender,
    mainloop: pw::main_loop::MainLoopRc,
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
                let node = |key| global.props?.get(key)?.parse().ok();
                if let (Some(output_node), Some(input_node)) =
                    (node("link.output.node"), node("link.input.node"))
                {
                    let _ = events.send(Event::LinkAdded {
                        id: global.id,
                        output_node,
                        input_node,
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

    let mut audio = Vec::new();
    if config.mic.enabled {
        let settings = Settings::mic(&config.mic);
        audio.push(AudioDevice::new(
            Kind::Mic,
            &core,
            settings,
            events.clone(),
        )?);
    }
    if config.speaker.enabled {
        let settings = Settings::speaker(&config.speaker);
        match AudioDevice::new(Kind::Speaker, &core, settings, events.clone()) {
            Ok(speaker) => audio.push(speaker),
            Err(e) => eprintln!("speaker: disabled: {e:#}"),
        }
    }
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
    let has = |kind| audio.iter().any(|d: &AudioDevice| d.kind() == kind);
    eprintln!(
        "broadcast-linux running (config {}){}{}{}",
        paths.config.display(),
        status(has(Kind::Mic), "mic"),
        status(has(Kind::Speaker), "speaker"),
        status(camera.is_some(), "camera"),
    );
    notify_ready();

    let state = RefCell::new(State {
        paths,
        config,
        audio,
        camera,
        events: events.clone(),
        mainloop: mainloop.clone(),
    });
    let _receiver = receiver.attach(mainloop.loop_(), move |event| {
        state.borrow_mut().handle(event);
    });
    mainloop.run();
    Ok(())
}

/// Tells systemd the devices exist, so WirePlumber (ordered after) probes a live camera.
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

/// PipeWire reads signals via signalfd, so block them before any thread starts;
/// every later thread inherits the mask.
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

fn device(audio: &mut [AudioDevice], kind: Kind) -> Option<&mut AudioDevice> {
    audio.iter_mut().find(|d| d.kind() == kind)
}

impl State {
    fn handle(&mut self, event: Event) {
        match event {
            Event::LinkAdded {
                id,
                output_node,
                input_node,
            } => {
                // A link from the mic into the speaker is a reader of both.
                let kinds: Vec<Kind> = self
                    .audio
                    .iter_mut()
                    .filter_map(|d| d.link_added(id, output_node, input_node).then(|| d.kind()))
                    .collect();
                for kind in kinds {
                    self.readers_changed(kind);
                }
            }
            Event::LinkRemoved(id) => {
                let kinds: Vec<Kind> = self
                    .audio
                    .iter_mut()
                    .filter_map(|d| d.link_removed(id).then(|| d.kind()))
                    .collect();
                for kind in kinds {
                    self.readers_changed(kind);
                }
            }
            Event::IdleTimeout(kind, token) => {
                if let Some(d) = device(&mut self.audio, kind)
                    && d.idle_token() == token
                    && d.readers() == 0
                    && d.idle()
                {
                    let delay = d.unload_after();
                    self.send_after(delay, Event::UnloadTimeout(kind, token));
                }
            }
            Event::UnloadTimeout(kind, token) => {
                if let Some(d) = device(&mut self.audio, kind)
                    && d.idle_token() == token
                    && d.readers() == 0
                    && d.has_session()
                {
                    eprintln!(
                        "{}: unloading after {} min unused",
                        kind.label(),
                        d.unload_after().as_secs() / 60
                    );
                    d.stop();
                }
            }
            Event::WorkerReady(kind, session) => {
                if let Some(d) = device(&mut self.audio, kind) {
                    d.worker_ready(session);
                }
            }
            Event::WorkerExited(kind, session) => {
                if let Some(delay) =
                    device(&mut self.audio, kind).and_then(|d| d.worker_exited(session))
                {
                    self.send_after(delay, Event::Restart(kind));
                }
            }
            Event::Restart(kind) => {
                if let Some(d) = device(&mut self.audio, kind).filter(|d| d.readers() > 0) {
                    d.start(&self.paths);
                }
            }
            Event::Reload => self.reload(),
            Event::Quit => {
                for d in &mut self.audio {
                    d.stop();
                }
                // Joins the camera thread; it never sends events, so this can't deadlock.
                drop(self.camera.take());
                self.mainloop.quit();
            }
        }
    }

    fn readers_changed(&mut self, kind: Kind) {
        let timeout = Duration::from_secs(self.config.service.idle_timeout_seconds);
        let Some(d) = device(&mut self.audio, kind) else {
            return;
        };
        let token = d.bump_idle_token();
        eprintln!("{}: {} app(s) reading", kind.label(), d.readers());
        if d.readers() > 0 {
            d.start(&self.paths);
        } else if d.has_session() {
            self.send_after(timeout, Event::IdleTimeout(kind, token));
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
        for d in &mut self.audio {
            let settings = match d.kind() {
                Kind::Mic => Settings::mic(&config.mic),
                Kind::Speaker => Settings::speaker(&config.speaker),
            };
            if d.set_settings(settings) {
                d.stop();
                if d.readers() > 0 {
                    d.start(&self.paths);
                }
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
