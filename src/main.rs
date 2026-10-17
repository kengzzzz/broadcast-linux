use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail, ensure};
use broadcast_linux::config::{FrameGenerationMode, LightPreset, MicProfile, SuperResolutionMode};
use broadcast_linux::paths::Paths;
use broadcast_linux::process::{self, Effects};
use broadcast_linux::progress::{Cancelled, Progress, Terminal};
use broadcast_linux::{doctor, graph, service, setup};
use clap::{Args, Parser, Subcommand};

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
    /// List microphones and outputs with the node names the config uses
    Devices,
    /// Run effects over video and audio files (needs ffmpeg)
    Process(ProcessArgs),
}

// Options with an optional value take it only after `=`, so they never take a file name.
#[derive(Args)]
#[allow(clippy::struct_excessive_bools)]
struct ProcessArgs {
    /// Video or audio files
    #[arg(required = true, value_name = "FILE")]
    files: Vec<PathBuf>,
    /// Folder for the new files [default: beside each file]
    #[arg(short, long, value_name = "DIR")]
    output: Option<PathBuf>,

    /// Remove background noise, at a strength from 0 to 1
    #[arg(long, help_heading = "Audio effects", value_name = "STRENGTH", num_args = 0..=1,
        require_equals = true, default_missing_value = "1", value_parser = strength)]
    noise_removal: Option<f32>,
    /// Remove room echo, at a strength from 0 to 1
    #[arg(long, help_heading = "Audio effects", value_name = "STRENGTH", num_args = 0..=1,
        require_equals = true, default_missing_value = "1", value_parser = strength)]
    room_echo_removal: Option<f32>,
    /// Make the voice sound like a studio mic, with a mic profile
    #[arg(long, help_heading = "Audio effects", value_name = "PROFILE", num_args = 0..=1,
        require_equals = true, default_missing_value = "default",
        conflicts_with_all = ["noise_removal", "room_echo_removal"])]
    studio_voice: Option<MicProfile>,

    /// Remove grain in low light
    #[arg(long, help_heading = "Video effects")]
    video_noise_removal: bool,
    /// Blur the background, at a strength from 0 to 1
    #[arg(long, help_heading = "Video effects", value_name = "STRENGTH", num_args = 0..=1,
        require_equals = true, default_missing_value = "0.5", value_parser = strength,
        conflicts_with_all = ["background", "remove_background"])]
    blur: Option<f32>,
    /// Replace the background with an image
    #[arg(
        long,
        help_heading = "Video effects",
        value_name = "IMAGE",
        conflicts_with = "remove_background"
    )]
    background: Option<String>,
    /// Make the background black
    #[arg(long, help_heading = "Video effects")]
    remove_background: bool,
    /// Light the face like a studio light, at a strength from 0 to 1
    #[arg(long, help_heading = "Video effects", value_name = "STRENGTH", num_args = 0..=1,
        require_equals = true, default_missing_value = "1", value_parser = strength)]
    studio_light: Option<f32>,
    /// Studio Light's color [default: neutral]
    #[arg(long, help_heading = "Video effects", value_name = "PRESET")]
    light: Option<LightPreset>,
    /// Point the eyes at the camera
    #[arg(long, help_heading = "Video effects")]
    eye_contact: bool,
    /// Zoom in and follow the face
    #[arg(long, help_heading = "Video effects")]
    auto_frame: bool,
    /// Upscale to this height with RTX Video Super Resolution
    #[arg(long, help_heading = "Video effects", value_name = "HEIGHT",
        value_parser = ["1080", "1440", "2160"])]
    super_resolution: Option<String>,
    /// Super Resolution's mode [default: quality]
    #[arg(long, help_heading = "Video effects", value_name = "MODE")]
    super_resolution_mode: Option<SuperResolutionMode>,
    /// Make 2, 3 or 4 frames from each one with RTX Video Frame Generation
    #[arg(long, help_heading = "Video effects", value_name = "MULTIPLIER", num_args = 0..=1,
        require_equals = true, default_missing_value = "2",
        value_parser = clap::value_parser!(u32).range(2..=4))]
    frame_generation: Option<u32>,
    /// Frame Generation's mode [default: balanced]
    #[arg(long, help_heading = "Video effects", value_name = "MODE")]
    frame_generation_mode: Option<FrameGenerationMode>,
}

impl ProcessArgs {
    fn effects(&self) -> Effects {
        let mut effects = Effects::default();
        let audio = &mut effects.audio;
        if let Some(strength) = self.noise_removal {
            audio.noise_removal.enabled = true;
            audio.noise_removal.strength = strength;
        }
        if let Some(strength) = self.room_echo_removal {
            audio.room_echo_removal.enabled = true;
            audio.room_echo_removal.strength = strength;
        }
        if let Some(profile) = self.studio_voice {
            audio.studio_voice.enabled = true;
            audio.studio_voice.profile = profile;
        }
        let video = &mut effects.video;
        video.video_noise_removal.enabled = self.video_noise_removal;
        if let Some(strength) = self.blur {
            video.background_blur.enabled = true;
            video.background_blur.strength = strength;
        }
        video.background.clone_from(&self.background);
        video.background_removal.enabled = self.remove_background;
        if let Some(strength) = self.studio_light {
            video.studio_light.enabled = true;
            video.studio_light.strength = strength;
        }
        video.studio_light.preset = self.light.unwrap_or_default();
        video.eye_contact.enabled = self.eye_contact;
        video.auto_frame.enabled = self.auto_frame;
        if let Some(height) = &self.super_resolution {
            video.super_resolution.enabled = true;
            effects.upscale_height = height.parse().unwrap_or(1080);
        }
        video.super_resolution.mode = self.super_resolution_mode.unwrap_or_default();
        if let Some(multiplier) = self.frame_generation {
            video.frame_generation.enabled = true;
            video.frame_generation.multiplier = multiplier;
        }
        video.frame_generation.mode = self.frame_generation_mode.unwrap_or_default();
        effects
    }
}

fn strength(text: &str) -> Result<f32, String> {
    let value: f32 = text
        .parse()
        .map_err(|_| format!("{text} is not a number"))?;
    if (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err("must be between 0 and 1".into())
    }
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn interrupted(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

/// Names the file in each step, and stops at Ctrl+C, so the half-made files are removed.
struct FileProgress {
    terminal: Terminal,
    file: RefCell<String>,
}

impl Progress for FileProgress {
    fn step(&self, message: &str) {
        self.terminal
            .step(&format!("{}: {message}", self.file.borrow()));
    }

    fn advance(&self, done: u64, total: u64) {
        self.terminal.advance(done, total);
    }

    fn cancelled(&self) -> bool {
        INTERRUPTED.load(Ordering::Relaxed)
    }
}

fn process_files(args: &ProcessArgs) -> Result<()> {
    if let Some(problem) = process::ffmpeg_missing() {
        bail!(problem);
    }
    let effects = args.effects();
    effects.validate()?;
    let paths = Paths::new()?;
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGINT, interrupted as *const () as libc::sighandler_t);
    }
    let progress = FileProgress {
        terminal: Terminal::timed(),
        file: RefCell::default(),
    };
    let mut failed = 0;
    for file in &args.files {
        *progress.file.borrow_mut() = file.file_name().map_or_else(
            || file.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let result = process::process(&paths, file, &effects, args.output.as_deref(), &progress);
        progress.terminal.end_line();
        match result {
            Ok(output) => println!("{}", output.display()),
            Err(e) if e.is::<Cancelled>() => bail!("cancelled"),
            Err(e) => {
                failed += 1;
                eprintln!("{}: {e:#}", file.display());
            }
        }
    }
    ensure!(failed == 0, "{failed} of {} files failed", args.files.len());
    Ok(())
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
        Commands::Devices => graph::print_devices(),
        Commands::Process(args) => process_files(&args),
    }
}
