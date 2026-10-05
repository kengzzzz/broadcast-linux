mod audio;
mod camera;
mod config;
mod doctor;
mod download;
mod gpu;
mod nvidia;
mod paths;
mod prefix;
mod service;
mod setup;
mod sevenzip;
mod v4l2;
mod webcam;
mod worker;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    version,
    about = "NVIDIA Broadcast effects as a virtual mic and camera on Linux"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Download NVIDIA Broadcast for this GPU and prepare the Wine prefix
    Setup {
        /// Accept NVIDIA's licence without the interactive prompt
        #[arg(long)]
        accept_eula: bool,
        /// Allow installers whose checksum is not pinned yet (size check only)
        #[arg(long)]
        allow_unverified: bool,
        /// Keep the downloaded installer after extraction
        #[arg(long)]
        keep_installer: bool,
    },
    /// Run the service: provide the virtual devices and start effects on demand
    Run,
    /// Check the setup and print what needs fixing
    Doctor,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Commands::Setup {
            accept_eula,
            allow_unverified,
            keep_installer,
        } => setup::run(&setup::Options {
            accept_eula,
            allow_unverified,
            keep_installer,
        }),
        Commands::Run => service::run(),
        Commands::Doctor => doctor::run(),
    }
}
