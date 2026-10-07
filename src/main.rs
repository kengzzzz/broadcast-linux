use anyhow::Result;
use broadcast_linux::paths::Paths;
use broadcast_linux::progress::Terminal;
use broadcast_linux::{doctor, service, setup};
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
        /// Ignored; kept so existing scripts still work
        #[arg(long, hide = true)]
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
            allow_unverified: _,
            keep_installer,
        } => setup::run(
            &Paths::new()?,
            &setup::Options { keep_installer },
            &setup::Terminal {
                progress: Terminal::new(),
                preaccepted: accept_eula,
            },
        ),
        Commands::Run => service::run(),
        Commands::Doctor => doctor::run(),
    }
}
