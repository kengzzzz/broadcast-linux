use anyhow::Result;
use broadcast_linux::audio::Kind;
use broadcast_linux::graph::{self, AudioNode, Graph};

#[derive(Default)]
pub struct Devices {
    pub sources: Vec<AudioNode>,
    pub sinks: Vec<AudioNode>,
    pub default_source: Option<String>,
}

/// Without our own nodes; empty if PipeWire did not answer.
pub fn query() -> Devices {
    let Ok(graph) = Graph::query() else {
        return Devices::default();
    };
    let nodes = |kind: Kind| {
        graph
            .nodes(kind)
            .filter(|n| n.name != kind.node_name())
            .cloned()
            .collect()
    };
    Devices {
        sources: nodes(Kind::Mic),
        sinks: nodes(Kind::Speaker),
        default_source: graph.default_node(Kind::Mic).map(str::to_owned),
    }
}

pub fn make_default_mic() -> Result<()> {
    graph::set_default(Kind::Mic, Kind::Mic.node_name())
}
