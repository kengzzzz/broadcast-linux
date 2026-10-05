use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioEffect {
    Denoiser,
    Dereverb,
    DereverbDenoiser,
    StudioVoiceLowLatency,
}

impl AudioEffect {
    pub fn selector(self) -> &'static str {
        match self {
            Self::Denoiser => "denoiser",
            Self::Dereverb => "dereverb",
            Self::DereverbDenoiser => "dereverb_denoiser",
            Self::StudioVoiceLowLatency => "studio_voice_low_latency",
        }
    }

    pub fn model(self) -> (&'static str, &'static str) {
        match self {
            Self::Denoiser => ("nvbcast_afx_bnr_v0_9", "denoiser_48k.trtpkg"),
            Self::Dereverb => ("nvbcast_afx_rec_v0_9", "dereverb_48k.trtpkg"),
            Self::DereverbDenoiser => ("nvbcast_afx_bnrrec_v0_9", "dereverb_denoiser_48k.trtpkg"),
            Self::StudioVoiceLowLatency => (
                "nvbcast_afx_stdvoice_v0_9",
                "studio_voice_low_latency_48k.trtpkg",
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Adjustable {
    pub enabled: bool,
    pub strength: f32,
}

impl Default for Adjustable {
    fn default() -> Self {
        Self {
            enabled: true,
            strength: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Toggle {
    pub enabled: bool,
}

impl Default for Toggle {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stage {
    pub effect: AudioEffect,
    pub strength: f32,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct MicConfig {
    pub enabled: bool,
    pub input: String,
    pub noise_removal: Adjustable,
    pub room_echo_removal: Adjustable,
    pub studio_voice: Toggle,
    pub unload_after_minutes: u64,
    pub name: String,
}

impl Default for MicConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            input: "default".into(),
            noise_removal: Adjustable::default(),
            room_echo_removal: Adjustable {
                enabled: false,
                ..Adjustable::default()
            },
            studio_voice: Toggle { enabled: false },
            unload_after_minutes: 10,
            name: "NVIDIA Broadcast Mic".into(),
        }
    }
}

impl MicConfig {
    pub fn stages(&self) -> Vec<Stage> {
        stages(
            self.noise_removal,
            self.room_echo_removal,
            self.studio_voice.enabled,
        )
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SpeakerConfig {
    pub enabled: bool,
    pub output: String,
    pub noise_removal: Adjustable,
    pub room_echo_removal: Adjustable,
    pub unload_after_minutes: u64,
    pub name: String,
}

impl Default for SpeakerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            output: "default".into(),
            noise_removal: Adjustable::default(),
            room_echo_removal: Adjustable {
                enabled: false,
                ..Adjustable::default()
            },
            unload_after_minutes: 10,
            name: "NVIDIA Broadcast Speaker".into(),
        }
    }
}

impl SpeakerConfig {
    pub fn stages(&self) -> Vec<Stage> {
        stages(self.noise_removal, self.room_echo_removal, false)
    }
}

/// Studio Voice replaces noise/echo removal, as on Windows. Noise and echo together
/// use the combined model at the higher strength.
fn stages(noise: Adjustable, echo: Adjustable, studio_voice: bool) -> Vec<Stage> {
    let strength = |effect: Adjustable| effect.enabled.then_some(effect.strength);
    let stage = |effect, strength| Stage { effect, strength };
    let mut stages = Vec::new();
    if studio_voice {
        stages.push(stage(AudioEffect::StudioVoiceLowLatency, 1.0));
        return stages;
    }
    match (strength(noise), strength(echo)) {
        (Some(noise), Some(echo)) => {
            stages.push(stage(AudioEffect::DereverbDenoiser, noise.max(echo)));
        }
        (Some(noise), None) => stages.push(stage(AudioEffect::Denoiser, noise)),
        (None, Some(echo)) => stages.push(stage(AudioEffect::Dereverb, echo)),
        (None, None) => {}
    }
    stages
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LightPreset {
    Cooler,
    Cool,
    #[default]
    Neutral,
    Warm,
    Warmer,
}

impl LightPreset {
    pub fn file(self) -> &'static str {
        match self {
            Self::Cooler => "vkl_cool_02.hdr",
            Self::Cool => "vkl_cool_01.hdr",
            Self::Neutral => "vkl_mid.hdr",
            Self::Warm => "vkl_warm_01.hdr",
            Self::Warmer => "vkl_warm_02.hdr",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct StudioLight {
    pub enabled: bool,
    pub strength: f32,
    pub preset: LightPreset,
}

impl Default for StudioLight {
    fn default() -> Self {
        Self {
            enabled: true,
            strength: 1.0,
            preset: LightPreset::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct BackgroundBlur {
    pub enabled: bool,
    pub strength: f32,
}

impl Default for BackgroundBlur {
    fn default() -> Self {
        Self {
            enabled: true,
            strength: 0.5,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputFormat {
    #[default]
    Auto,
    Mjpeg,
    Yuyv,
    Nv12,
}

impl InputFormat {
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::Auto | Self::Mjpeg => "mjpeg",
            Self::Yuyv => "yuyv422",
            Self::Nv12 => "nv12",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "supported",
            Self::Mjpeg => "MJPEG",
            Self::Yuyv => "YUYV",
            Self::Nv12 => "NV12",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct CameraConfig {
    pub enabled: bool,
    pub device: String,
    pub input: String,
    pub input_format: InputFormat,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Broadcast ships only the strong denoising model, so there is no strength.
    pub video_noise_removal: Toggle,
    pub background: Option<String>,
    pub background_blur: BackgroundBlur,
    pub background_removal: Toggle,
    pub studio_light: StudioLight,
}

impl CameraConfig {
    pub fn has_effects(&self) -> bool {
        self.video_noise_removal.enabled
            || self.background.is_some()
            || self.background_blur.enabled
            || self.background_removal.enabled
            || self.studio_light.enabled
    }

    fn validate(&self) -> Result<()> {
        let backgrounds = usize::from(self.background.is_some())
            + usize::from(self.background_blur.enabled)
            + usize::from(self.background_removal.enabled);
        anyhow::ensure!(
            backgrounds <= 1,
            "use only one camera background effect: background, background_blur or background_removal"
        );
        anyhow::ensure!(
            (0.0..=1.0).contains(&self.background_blur.strength),
            "camera.background_blur.strength must be between 0.0 and 1.0"
        );
        Ok(())
    }
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            device: "/dev/video10".into(),
            input: "/dev/video0".into(),
            input_format: InputFormat::Auto,
            width: 1920,
            height: 1080,
            fps: 30,
            video_noise_removal: Toggle { enabled: false },
            background: None,
            background_blur: BackgroundBlur {
                enabled: false,
                ..BackgroundBlur::default()
            },
            background_removal: Toggle { enabled: false },
            studio_light: StudioLight {
                enabled: false,
                ..StudioLight::default()
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ServiceConfig {
    pub idle_timeout_seconds: u64,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            idle_timeout_seconds: 5,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub mic: MicConfig,
    pub speaker: SpeakerConfig,
    pub camera: CameraConfig,
    pub service: ServiceConfig,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)?;
        let config: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config
            .camera
            .validate()
            .with_context(|| format!("validating {}", path.display()))?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_partial_config() {
        let config: Config = toml::from_str(
            "[mic]\nstudio_voice = { enabled = true }\ninput = \"alsa_input.usb\"\n",
        )
        .unwrap();
        assert!(config.mic.studio_voice.enabled);
        assert!(config.mic.noise_removal.enabled);
        assert!(!config.mic.room_echo_removal.enabled);
        assert_eq!(config.mic.input, "alsa_input.usb");
        assert_eq!(config.mic.unload_after_minutes, 10);
        assert!(toml::from_str::<Config>("[mic]\nkeep_loaded = true").is_err());
        assert_eq!(config.service.idle_timeout_seconds, 5);
    }

    fn stages(toml: &str) -> Vec<(AudioEffect, f32)> {
        let config: Config = toml::from_str(toml).unwrap();
        config
            .mic
            .stages()
            .into_iter()
            .map(|s| (s.effect, s.strength))
            .collect()
    }

    #[test]
    fn maps_toggles_to_stages() {
        use AudioEffect::*;
        assert_eq!(stages(""), [(Denoiser, 1.0)]);
        assert_eq!(
            stages(
                "[mic]\nnoise_removal = { strength = 0.5 }\nroom_echo_removal = { strength = 0.75 }"
            ),
            [(DereverbDenoiser, 0.75)]
        );
        assert_eq!(
            stages(
                "[mic]\nnoise_removal = { enabled = false }\nroom_echo_removal = {}\nstudio_voice = {}"
            ),
            [(StudioVoiceLowLatency, 1.0)]
        );
        assert_eq!(
            stages("[mic]\nnoise_removal = { enabled = false }\nroom_echo_removal = {}"),
            [(Dereverb, 1.0)]
        );
        assert_eq!(stages("[mic]\nnoise_removal = { enabled = false }"), []);
    }

    #[test]
    fn parses_studio_light() {
        let config: Config =
            toml::from_str("[camera]\nstudio_light = { preset = \"warm\" }").unwrap();
        let light = config.camera.studio_light;
        assert!(light.enabled && config.camera.has_effects());
        assert_eq!(light.preset.file(), "vkl_warm_01.hdr");
        assert!(!Config::default().camera.has_effects());
        let config: Config = toml::from_str("[camera]\nvideo_noise_removal = {}").unwrap();
        assert!(config.camera.has_effects());
        assert!(toml::from_str::<Config>("[camera]\nstudio_light = { preset = \"hot\" }").is_err());
    }

    #[test]
    fn speaker_is_opt_in() {
        let config = Config::default();
        assert!(!config.speaker.enabled);
        assert_eq!(config.speaker.stages(), config.mic.stages());
        let config: Config =
            toml::from_str("[speaker]\nenabled = true\nroom_echo_removal = {}").unwrap();
        assert!(config.speaker.enabled);
        assert_eq!(
            config.speaker.stages(),
            [Stage {
                effect: AudioEffect::DereverbDenoiser,
                strength: 1.0
            }]
        );
        assert!(toml::from_str::<Config>("[speaker]\nstudio_voice = {}").is_err());
    }

    #[test]
    fn studio_voice_has_no_strength() {
        assert!(toml::from_str::<Config>("[mic]\nstudio_voice = { strength = 0.5 }").is_err());
    }

    #[test]
    fn camera_background_effects() {
        let config: Config = toml::from_str("[camera]\nbackground_blur = {}").unwrap();
        assert!(config.camera.background_blur.enabled);
        assert!((config.camera.background_blur.strength - 0.5).abs() < f32::EPSILON);
        for effect in [
            "background = \"~/Pictures/background.jpg\"",
            "background_blur = { enabled = true, strength = 0.75 }",
            "background_removal = { enabled = true }",
        ] {
            let config: Config = toml::from_str(&format!("[camera]\n{effect}")).unwrap();
            assert!(config.camera.has_effects());
            config.camera.validate().unwrap();
        }
        for settings in [
            "background = \"image.jpg\"\nbackground_blur = {}",
            "background = \"image.jpg\"\nbackground_removal = {}",
            "background_blur = {}\nbackground_removal = {}",
            "background_blur = { strength = -0.1 }",
            "background_blur = { strength = 1.1 }",
            "background_blur = { strength = nan }",
        ] {
            let config: Config = toml::from_str(&format!("[camera]\n{settings}")).unwrap();
            assert!(config.camera.validate().is_err(), "{settings}");
        }
        assert!(
            toml::from_str::<Config>("[camera]\nbackground_removal = { strength = 0.5 }").is_err()
        );
    }

    #[test]
    fn example_config_matches_defaults() {
        let config: Config = toml::from_str(include_str!("../packaging/config.toml")).unwrap();
        assert_eq!(config, Config::default());
        config.camera.validate().unwrap();
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<Config>("[mic]\nefect = \"denoiser\"\n").is_err());
    }
}
