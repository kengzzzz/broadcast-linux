use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::paths::Paths;

pub struct Installation {
    pub runtime: PathBuf,
    models: PathBuf,
    studio_light: PathBuf,
}

impl Installation {
    pub fn find(paths: &Paths) -> Result<Self> {
        let build = fs::read_dir(paths.nvidia())
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.join(".complete").exists())
            .max()
            .context("NVIDIA Broadcast is not installed; run `broadcast-linux setup` first")?;
        let models = build.join("models");
        let runtime = newest_files_dir(&models.join("nvbcast"))?;
        if !runtime.join("NVAudioEffects.dll").exists() {
            anyhow::bail!("{} has no NVAudioEffects.dll", runtime.display());
        }
        Ok(Self {
            runtime,
            models,
            studio_light: build.join(crate::setup::STUDIO_LIGHT_DIR),
        })
    }

    pub fn model_dir(&self, folder: &str) -> Result<PathBuf> {
        newest_files_dir(&self.models.join(folder))
    }

    pub fn studio_light(&self, file: &str) -> Result<PathBuf> {
        let path = self.studio_light.join(file);
        if !path.exists() {
            anyhow::bail!(
                "Studio Light preset {} is missing; run `broadcast-linux setup` again",
                path.display()
            );
        }
        Ok(path)
    }

    pub fn model(&self, folder: &str, file: &str) -> Result<PathBuf> {
        let path = newest_files_dir(&self.models.join(folder))?.join(file);
        if !path.exists() {
            anyhow::bail!("model {} is missing", path.display());
        }
        Ok(path)
    }
}

/// Models are laid out as `<name>/versions/<version>/files/<gpu id>/`.
fn newest_files_dir(model: &Path) -> Result<PathBuf> {
    let version = newest_child(&model.join("versions"))?;
    newest_child(&version.join("files"))
}

fn newest_child(dir: &Path) -> Result<PathBuf> {
    fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .max()
        .with_context(|| format!("{} is empty", dir.display()))
}

/// Wine maps `Z:` to the Unix root.
pub fn windows_path(path: &Path) -> String {
    format!("Z:{}", path.display().to_string().replace('/', "\\"))
}
