// Shared by the service and GUI binaries; not a published API.
#![allow(
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]

pub mod audio;
pub mod camera;
pub mod config;
pub mod doctor;
pub mod download;
pub mod frames;
pub mod gpu;
pub mod graph;
pub mod mjpeg;
pub mod nvidia;
pub mod paths;
pub mod prefix;
pub mod progress;
pub mod service;
pub mod setup;
pub mod sevenzip;
pub mod status;
pub mod v4l2;
pub mod webcam;
pub mod worker;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
