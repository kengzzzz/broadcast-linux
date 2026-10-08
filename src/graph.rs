use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use pipewire as pw;
use pw::metadata::{Metadata, MetadataListener};
use pw::node::{Node, NodeListener};
use pw::registry::GlobalObject;
use pw::spa::param::ParamType;
use pw::spa::pod::deserialize::PodDeserializer;
use pw::spa::pod::{Pod, Value};
use pw::spa::utils::dict::DictRef;
use pw::types::ObjectType;
use serde::Deserialize;

use crate::audio::Kind;

const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioNode {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub muted: bool,
    class: String,
}

impl AudioNode {
    /// Matches what pipewire-pulse lists as sources or sinks.
    pub fn is(&self, kind: Kind) -> bool {
        let class = self.class.as_str();
        match kind {
            Kind::Mic => matches!(
                class,
                "Audio/Source" | "Audio/Source/Virtual" | "Audio/Duplex"
            ),
            Kind::Speaker => matches!(class, "Audio/Sink" | "Audio/Duplex"),
        }
    }
}

#[derive(Debug, Default)]
pub struct Graph {
    pub version: String,
    pub nodes: Vec<AudioNode>,
    default_source: Option<String>,
    default_sink: Option<String>,
}

impl Graph {
    pub fn query() -> Result<Self> {
        run(None)
    }

    pub fn nodes(&self, kind: Kind) -> impl Iterator<Item = &AudioNode> {
        self.nodes.iter().filter(move |n| n.is(kind))
    }

    pub fn find(&self, kind: Kind, name: &str) -> Option<&AudioNode> {
        self.nodes(kind).find(|n| n.name == name)
    }

    pub fn default_node(&self, kind: Kind) -> Option<&str> {
        match kind {
            Kind::Mic => self.default_source.as_deref(),
            Kind::Speaker => self.default_sink.as_deref(),
        }
    }

    /// The node name for a config target, which is "default" or a node name.
    pub fn resolve(&self, kind: Kind, target: &str) -> Result<String> {
        let (key, device) = match kind {
            Kind::Mic => ("input", "microphone"),
            Kind::Speaker => ("output", "output device"),
        };
        let name = if target == "default" {
            self.default_node(kind)
                .with_context(|| format!("no default {device} is set"))?
        } else {
            target
        };
        if name == kind.node_name() {
            let label = kind.label();
            bail!("{name} is this {label} itself; set [{label}] {key} to the real {device}");
        }
        Ok(name.to_owned())
    }
}

/// Like [`Graph::resolve`], but asks PipeWire only when `target` is "default".
pub fn resolve(kind: Kind, target: &str) -> Result<String> {
    if target == "default" {
        Graph::query()?.resolve(kind, target)
    } else {
        Graph::default().resolve(kind, target)
    }
}

/// Sets the configured default the way `pactl set-default-source` does; WirePlumber
/// then switches the actual default.
pub fn set_default(kind: Kind, name: &str) -> Result<()> {
    run(Some((kind, name.to_owned()))).map(drop)
}

pub fn print_devices() -> Result<()> {
    let graph = Graph::query()?;
    for (kind, title) in [(Kind::Mic, "Microphones"), (Kind::Speaker, "Outputs")] {
        println!("{title}:");
        for node in graph.nodes(kind) {
            let mark = if graph.default_node(kind) == Some(node.name.as_str()) {
                '*'
            } else {
                ' '
            };
            println!(
                "{mark} {:>4}  {}  ({})",
                node.id, node.name, node.description
            );
        }
    }
    println!("* is the current default.");
    Ok(())
}

/// Runs on its own thread because the service calls this from its main loop's callbacks.
fn run(set_default: Option<(Kind, String)>) -> Result<Graph> {
    thread::spawn(move || query(set_default))
        .join()
        .map_err(|_| anyhow!("the PipeWire query panicked"))?
}

#[derive(Default)]
struct Bound {
    nodes: Vec<(Node, NodeListener)>,
    defaults: Option<(Metadata, MetadataListener)>,
}

fn query(set_default: Option<(Kind, String)>) -> Result<Graph> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context
        .connect_rc(None)
        .context("connecting to PipeWire (is it running?)")?;
    let registry = core.get_registry_rc()?;
    let graph = Rc::new(RefCell::new(Graph::default()));
    let bound = Rc::new(RefCell::new(Bound::default()));
    let error = Rc::new(RefCell::new(None::<anyhow::Error>));

    let _registry_listener = registry
        .add_listener_local()
        .global({
            let (registry, graph, bound) = (registry.clone(), graph.clone(), bound.clone());
            move |global| on_global(&registry, &graph, &bound, global)
        })
        .register();

    // Round 1 lists the globals. Binding them sends more requests, so round 2 waits
    // for their properties and params (and for a default change to be sent).
    let pending = Rc::new(Cell::new(core.sync(0)?));
    let round = Cell::new(1);
    let _core_listener = core
        .add_listener_local()
        .info({
            let graph = graph.clone();
            move |info| info.version().clone_into(&mut graph.borrow_mut().version)
        })
        .done({
            let (core, mainloop, graph, bound, error) = (
                core.clone(),
                mainloop.clone(),
                graph.clone(),
                bound.clone(),
                error.clone(),
            );
            let pending = pending.clone();
            move |id, seq| {
                if id != pw::core::PW_ID_CORE || seq != pending.get() {
                    return;
                }
                if round.replace(2) == 2 {
                    mainloop.quit();
                    return;
                }
                if let Some((kind, name)) = &set_default
                    && let Err(e) = send_default(&graph.borrow(), &bound.borrow(), *kind, name)
                {
                    *error.borrow_mut() = Some(e);
                    mainloop.quit();
                    return;
                }
                match core.sync(0) {
                    Ok(seq) => pending.set(seq),
                    Err(e) => {
                        *error.borrow_mut() = Some(e.into());
                        mainloop.quit();
                    }
                }
            }
        })
        .error({
            let (mainloop, error) = (mainloop.clone(), error.clone());
            move |id, _, _, message| {
                if id == pw::core::PW_ID_CORE {
                    *error.borrow_mut() = Some(anyhow!("PipeWire: {message}"));
                    mainloop.quit();
                }
            }
        })
        .register();

    let timed_out = Rc::new(Cell::new(false));
    let timer = mainloop.loop_().add_timer({
        let (mainloop, timed_out) = (mainloop.clone(), timed_out.clone());
        move |_| {
            timed_out.set(true);
            mainloop.quit();
        }
    });
    timer.update_timer(Some(TIMEOUT), None).into_result()?;
    mainloop.run();

    if let Some(e) = error.take() {
        return Err(e);
    }
    if timed_out.get() {
        bail!("PipeWire did not answer within {} s", TIMEOUT.as_secs());
    }
    Ok(graph.take())
}

fn on_global(
    registry: &pw::registry::RegistryRc,
    graph: &Rc<RefCell<Graph>>,
    bound: &RefCell<Bound>,
    global: &GlobalObject<&DictRef>,
) {
    let Some(props) = global.props else {
        return;
    };
    match global.type_ {
        ObjectType::Node => {
            let (Some(class), Some(name)) = (props.get("media.class"), props.get("node.name"))
            else {
                return;
            };
            if !class.starts_with("Audio/") {
                return;
            }
            let description = props
                .get("node.description")
                .or_else(|| props.get("node.nick"))
                .unwrap_or(name);
            graph.borrow_mut().nodes.push(AudioNode {
                id: global.id,
                name: name.to_owned(),
                description: description.to_owned(),
                muted: false,
                class: class.to_owned(),
            });
            let Ok(node) = registry.bind::<Node, _>(global) else {
                return;
            };
            let id = global.id;
            let graph = graph.clone();
            let listener = node
                .add_listener_local()
                .param(move |_, _, _, _, pod| {
                    if pod.and_then(mute) == Some(true)
                        && let Some(n) = graph.borrow_mut().nodes.iter_mut().find(|n| n.id == id)
                    {
                        n.muted = true;
                    }
                })
                .register();
            node.enum_params(0, Some(ParamType::Props), 0, u32::MAX);
            bound.borrow_mut().nodes.push((node, listener));
        }
        ObjectType::Metadata if props.get("metadata.name") == Some("default") => {
            let Ok(metadata) = registry.bind::<Metadata, _>(global) else {
                return;
            };
            let graph = graph.clone();
            let listener = metadata
                .add_listener_local()
                .property(move |_, key, _, value| {
                    let mut graph = graph.borrow_mut();
                    let slot = match key {
                        Some("default.audio.source") => &mut graph.default_source,
                        Some("default.audio.sink") => &mut graph.default_sink,
                        _ => return 0,
                    };
                    *slot = value.and_then(node_name);
                    0
                })
                .register();
            bound.borrow_mut().defaults = Some((metadata, listener));
        }
        _ => {}
    }
}

fn send_default(graph: &Graph, bound: &Bound, kind: Kind, name: &str) -> Result<()> {
    let (metadata, _) = bound
        .defaults
        .as_ref()
        .context("PipeWire has no default-device settings (is WirePlumber running?)")?;
    if graph.find(kind, name).is_none() {
        bail!("{name} not found (is the service running?)");
    }
    let key = match kind {
        Kind::Mic => "default.configured.audio.source",
        Kind::Speaker => "default.configured.audio.sink",
    };
    let value = serde_json::json!({ "name": name }).to_string();
    metadata.set_property(0, key, Some("Spa:String:JSON"), Some(&value));
    Ok(())
}

fn node_name(json: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Value {
        name: String,
    }
    serde_json::from_str::<Value>(json).ok().map(|v| v.name)
}

fn mute(pod: &Pod) -> Option<bool> {
    let Ok((_, Value::Object(props))) = PodDeserializer::deserialize_any_from(pod.as_bytes())
    else {
        return None;
    };
    props
        .properties
        .iter()
        .find(|p| p.key == pw::spa::sys::SPA_PROP_mute)
        .and_then(|p| match p.value {
            Value::Bool(muted) => Some(muted),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_node_names() {
        assert_eq!(
            node_name(r#"{ "name": "alsa_input.usb-mic" }"#).as_deref(),
            Some("alsa_input.usb-mic")
        );
        assert_eq!(node_name("garbage"), None);
    }

    #[test]
    fn resolves_targets() {
        let graph = Graph {
            default_source: Some("real_mic".into()),
            ..Graph::default()
        };
        assert_eq!(graph.resolve(Kind::Mic, "default").unwrap(), "real_mic");
        assert_eq!(graph.resolve(Kind::Mic, "other").unwrap(), "other");
        assert!(graph.resolve(Kind::Speaker, "default").is_err());
        assert!(graph.resolve(Kind::Mic, Kind::Mic.node_name()).is_err());
    }
}
