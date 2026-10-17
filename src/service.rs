use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use pipewire as pw;
use pw::loop_::Signal;

use crate::audio::{AudioDevice, Kind, Settings};
use crate::camera::{self, Camera};
use crate::config::{self, Config};
use crate::gpu;
use crate::paths::Paths;
use crate::status;

#[derive(Clone)]
pub struct LinkEnd {
    pub node: u32,
    pub app: String,
}

#[derive(Clone)]
pub enum Event {
    /// App names are looked up when the link appears, since its nodes may be gone by
    /// the time the event is handled.
    LinkAdded {
        id: u32,
        output: LinkEnd,
        input: LinkEnd,
    },
    LinkRemoved(u32),
    IdleTimeout(Kind, u64),
    UnloadTimeout(Kind, u64),
    WorkerReady(Kind, u64),
    WorkerExited(Kind, u64),
    Restart(Kind),
    Reload,
    StatusChanged,
    Quit,
}

pub type EventSender = pw::channel::Sender<Event>;

/// How often status goes out while only loads change.
const LOAD_TICK: Duration = Duration::from_secs(1);

struct State {
    paths: Paths,
    config: Config,
    /// The config the service started with; restart-only fields keep these values.
    started: Config,
    status: Option<status::Server>,
    reloads: u64,
    reload_error: Option<String>,
    speaker_error: Option<String>,
    camera_error: Option<String>,
    /// From the config the service started with, since a change needs a restart.
    gpu: gpu::Lazy,
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
    let _registry_listener = watch_links(&registry, &events);

    let signal = |signal, event: fn() -> Event| {
        let events = events.clone();
        mainloop.loop_().add_signal_local(signal, move || {
            let _ = events.send(event());
        })
    };
    let _hup = signal(Signal::HUP, || Event::Reload);
    let _term = signal(Signal::TERM, || Event::Quit);
    let _int = signal(Signal::INT, || Event::Quit);

    let server = start_status(&events)?;
    let gpu = gpu::Lazy::new(config.service.gpu.clone());
    let (audio, speaker_error) = start_audio(&config, &gpu, &core, &events)?;
    let (camera, camera_error) = if config.camera.enabled {
        start_camera(&config, gpu.clone(), &events)?
    } else {
        (None, None)
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

    let state = Rc::new(RefCell::new(State {
        paths,
        started: config.clone(),
        config,
        status: server,
        reloads: 0,
        reload_error: None,
        speaker_error,
        camera_error,
        gpu,
        audio,
        camera,
        events: events.clone(),
        mainloop: mainloop.clone(),
    }));
    state.borrow_mut().publish();
    let attached = receiver.attach(mainloop.loop_(), {
        let state = Rc::clone(&state);
        move |event| {
            let mut state = state.borrow_mut();
            state.handle(event);
            state.publish();
        }
    });
    // Loads change without an event. Audio and camera threads must not send one: the channel
    // stays locked while a slow client is written to, which would stall them.
    let ticker = mainloop.loop_().add_timer({
        let state = Rc::clone(&state);
        move |_| state.borrow_mut().publish()
    });
    ticker
        .update_timer(Some(LOAD_TICK), Some(LOAD_TICK))
        .into_result()?;
    mainloop.run();
    // Dropping the state joins the camera thread. The channel stays locked while the
    // callback runs, so joining there would deadlock with a camera status notification.
    drop(ticker);
    drop(attached);
    drop(state);
    Ok(())
}

/// Sends link events with the app at each end.
fn watch_links(
    registry: &pw::registry::RegistryRc,
    events: &EventSender,
) -> pw::registry::Listener {
    let apps = Rc::new(RefCell::new(HashMap::<u32, String>::new()));
    registry
        .add_listener_local()
        .global({
            let (events, apps) = (events.clone(), apps.clone());
            move |global| {
                let prop = |key| global.props?.get(key);
                match global.type_ {
                    pw::types::ObjectType::Node => {
                        if let Some(app) = prop("application.name").or_else(|| prop("node.name")) {
                            apps.borrow_mut().insert(global.id, app.to_owned());
                        }
                    }
                    pw::types::ObjectType::Link => {
                        let node = |key| prop(key)?.parse().ok();
                        let (Some(output), Some(input)) =
                            (node("link.output.node"), node("link.input.node"))
                        else {
                            return;
                        };
                        let apps = apps.borrow();
                        let end = |node| LinkEnd {
                            node,
                            app: apps
                                .get(&node)
                                .cloned()
                                .unwrap_or_else(|| format!("node {node}")),
                        };
                        let _ = events.send(Event::LinkAdded {
                            id: global.id,
                            output: end(output),
                            input: end(input),
                        });
                    }
                    _ => {}
                }
            }
        })
        .global_remove({
            let events = events.clone();
            move |id| {
                apps.borrow_mut().remove(&id);
                let _ = events.send(Event::LinkRemoved(id));
            }
        })
        .register()
}

/// The mic is required; a speaker failure is returned instead.
fn start_audio(
    config: &Config,
    gpu: &gpu::Lazy,
    core: &pw::core::CoreRc,
    events: &EventSender,
) -> Result<(Vec<AudioDevice>, Option<String>)> {
    let mut audio = Vec::new();
    let mut speaker_error = None;
    if config.mic.enabled {
        let settings = Settings::mic(config, gpu);
        audio.push(AudioDevice::new(Kind::Mic, core, settings, events.clone())?);
    }
    if config.speaker.enabled {
        let settings = Settings::speaker(config, gpu);
        match AudioDevice::new(Kind::Speaker, core, settings, events.clone()) {
            Ok(speaker) => audio.push(speaker),
            Err(e) => {
                eprintln!("speaker: disabled: {e:#}");
                speaker_error = Some(format!("{e:#}"));
            }
        }
    }
    Ok((audio, speaker_error))
}

/// Fails if another service is running: two would create duplicate devices and fight
/// over the virtual camera. Any other socket problem only disables the status socket.
fn start_status(events: &EventSender) -> Result<Option<status::Server>> {
    let events = Mutex::new(events.clone());
    let reload = move || {
        let _ = events.lock().unwrap().send(Event::Reload);
    };
    match status::Server::start(reload) {
        Ok(server) => Ok(Some(server)),
        Err(e) if e.is::<status::AlreadyRunning>() => Err(e),
        Err(e) => {
            eprintln!("status socket: disabled: {e:#}");
            Ok(None)
        }
    }
}

/// A camera failure is returned, not fatal.
fn start_camera(
    config: &Config,
    gpu: gpu::Lazy,
    events: &EventSender,
) -> Result<(Option<Camera>, Option<String>)> {
    let idle = Duration::from_secs(config.service.idle_timeout_seconds);
    let events = Mutex::new(events.clone());
    let notify = Box::new(move || {
        let _ = events.lock().unwrap().send(Event::StatusChanged);
    });
    Ok(
        match Camera::start(Paths::new()?, config.camera.clone(), idle, gpu, notify) {
            Ok(camera) => (Some(camera), None),
            Err(e) => {
                eprintln!("camera: disabled: {e:#}");
                (None, Some(format!("{e:#}")))
            }
        },
    )
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
            Event::LinkAdded { id, output, input } => {
                // A link from the mic into the speaker is a reader of both.
                let kinds: Vec<Kind> = self
                    .audio
                    .iter_mut()
                    .filter_map(|d| d.link_added(id, &output, &input).then(|| d.kind()))
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
            Event::StatusChanged => {}
            Event::Quit => {
                self.status = None;
                for d in &mut self.audio {
                    d.stop();
                }
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
        let apps = d.reader_apps();
        if apps.is_empty() {
            eprintln!("{}: 0 app(s) reading", kind.label());
        } else {
            eprintln!(
                "{}: {} app(s) reading ({})",
                kind.label(),
                d.readers(),
                apps.join(", ")
            );
        }
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
                self.reloads += 1;
                self.reload_error = Some(format!("{e:#}"));
                return;
            }
        };
        self.reloads += 1;
        self.reload_error = None;
        eprintln!("reload: {}", self.paths.config.display());
        for d in &mut self.audio {
            let settings = match d.kind() {
                Kind::Mic => Settings::mic(&config, &self.gpu),
                Kind::Speaker => Settings::speaker(&config, &self.gpu),
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

    fn publish(&mut self) {
        let Some(server) = &mut self.status else {
            return;
        };
        let started = &self.started;
        let unavailable = |error: &Option<String>| status::Device {
            state: status::State::Unavailable,
            error: error.clone(),
            ..status::Device::default()
        };
        let audio = |kind, enabled: bool, error: &Option<String>| {
            if !enabled {
                return status::Device::default();
            }
            self.audio
                .iter()
                .find(|d| d.kind() == kind)
                .map_or_else(|| unavailable(error), AudioDevice::status)
        };
        let camera = if started.camera.enabled {
            self.camera
                .as_ref()
                .map_or_else(|| unavailable(&self.camera_error), Camera::status)
        } else {
            status::Device::default()
        };
        server.publish(&status::Status {
            protocol: status::PROTOCOL,
            version: env!("CARGO_PKG_VERSION").into(),
            reloads: self.reloads,
            reload_error: self.reload_error.clone(),
            restart_pending: config::needs_restart(started, &self.config),
            gpu: self.gpu.found().map(|chosen| status::Gpu {
                uuid: chosen.gpu.uuid.clone(),
                name: chosen.gpu.name.clone(),
                fallback: chosen.fallback.clone(),
            }),
            mic: audio(Kind::Mic, started.mic.enabled, &None),
            speaker: audio(Kind::Speaker, started.speaker.enabled, &self.speaker_error),
            camera,
        });
    }

    fn send_after(&self, delay: Duration, event: Event) {
        let events = self.events.clone();
        thread::spawn(move || {
            thread::sleep(delay);
            let _ = events.send(event);
        });
    }
}
