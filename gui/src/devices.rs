use std::process::Command;

use anyhow::{Context, Result, bail};
use broadcast_linux::audio::Kind;
use serde::Deserialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioNode {
    pub name: String,
    pub description: String,
}

#[derive(Deserialize)]
struct PactlNode {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    monitor_source: String,
}

/// Without monitor sources or our own nodes.
pub fn audio_nodes(kind: Kind) -> Result<Vec<AudioNode>> {
    let list = match kind {
        Kind::Mic => "sources",
        Kind::Speaker => "sinks",
    };
    let nodes: Vec<PactlNode> = serde_json::from_str(&pactl(&["-f", "json", "list", list])?)
        .context("reading pactl's device list")?;
    Ok(nodes
        .into_iter()
        .filter(|n| n.monitor_source.is_empty() || kind == Kind::Speaker)
        .filter(|n| n.name != kind.node_name())
        .map(|n| AudioNode {
            description: if n.description.is_empty() {
                n.name.clone()
            } else {
                n.description
            },
            name: n.name,
        })
        .collect())
}

pub fn default_node(kind: Kind) -> Option<String> {
    let query = match kind {
        Kind::Mic => "get-default-source",
        Kind::Speaker => "get-default-sink",
    };
    pactl(&[query]).ok().map(|s| s.trim().to_owned())
}

pub fn make_default_mic() -> Result<()> {
    pactl(&["set-default-source", Kind::Mic.node_name()]).map(drop)
}

fn pactl(args: &[&str]) -> Result<String> {
    let out = Command::new("pactl")
        .args(args)
        .output()
        .context("running pactl (is it installed?)")?;
    if !out.status.success() {
        bail!(
            "pactl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
