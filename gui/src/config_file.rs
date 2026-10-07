use std::fmt;
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use broadcast_linux::config::Config;
use toml_edit::{DocumentMut, InlineTable, Item, Table, TableLike, Value};

const TEMPLATE: &str = include_str!("../../packaging/config.toml");

/// The config file as last read or written, edited in place so comments survive.
pub struct ConfigFile {
    pub path: PathBuf,
    pub saved: Config,
    doc: DocumentMut,
    /// The file's text when last read or written; `None` while it does not exist.
    disk: Option<String>,
}

/// Returned by `write` when someone else changed the file since it was read.
#[derive(Debug)]
pub struct ChangedOnDisk;

impl fmt::Display for ChangedOnDisk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the config file changed on disk")
    }
}

impl std::error::Error for ChangedOnDisk {}

impl ConfigFile {
    /// A missing file starts from the documented template.
    pub fn load(path: &Path) -> Result<Self> {
        let disk = read_optional(path)?;
        let text = disk.as_deref().unwrap_or(TEMPLATE);
        let saved = Config::parse(text).with_context(|| format!("loading {}", path.display()))?;
        Ok(Self {
            path: path.to_owned(),
            saved,
            doc: text.parse()?,
            disk,
        })
    }

    /// Starts over from the template; the next `write` replaces whatever is on disk.
    pub fn defaults(path: &Path) -> Result<Self> {
        Ok(Self {
            path: path.to_owned(),
            saved: Config::default(),
            doc: TEMPLATE.parse()?,
            disk: read_optional(path)?,
        })
    }

    pub fn render(&self, draft: &Config) -> Result<String> {
        let old = toml::Value::try_from(&self.saved)?;
        let new = toml::Value::try_from(draft)?;
        let (Some(old), Some(new)) = (old.as_table(), new.as_table()) else {
            anyhow::bail!("the config did not serialize to a table");
        };
        let mut doc = self.doc.clone();
        apply(doc.as_table_mut(), old, new, true);
        let text = doc.to_string();
        let written = Config::parse(&text)?;
        ensure!(
            written == *draft,
            "the edited config reads back differently; not saving"
        );
        Ok(text)
    }

    /// Fails with `ChangedOnDisk` unless `overwrite`, if the file changed since it was read.
    pub fn write(&mut self, draft: &Config, overwrite: bool) -> Result<()> {
        if !overwrite && read_optional(&self.path)? != self.disk {
            return Err(ChangedOnDisk.into());
        }
        let text = self.render(draft)?;
        write_atomic(&self.path, &text)?;
        self.doc = text.parse()?;
        self.saved = draft.clone();
        self.disk = Some(text);
        Ok(())
    }
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Changes only what differs between `old` and `new`; new nested tables are inline.
fn apply(table: &mut dyn TableLike, old: &toml::Table, new: &toml::Table, top: bool) {
    for (key, value) in new {
        let before = old.get(key);
        if before == Some(value) {
            continue;
        }
        if let toml::Value::Table(child) = value {
            let empty = toml::Table::new();
            let mut before = before.and_then(toml::Value::as_table).unwrap_or(&empty);
            if table.get(key).and_then(Item::as_table_like).is_none() {
                let item = if top {
                    Item::Table(Table::new())
                } else {
                    Item::Value(Value::InlineTable(InlineTable::new()))
                };
                table.insert(key, item);
                // Fields missing from a nested table take that table's own defaults,
                // which can differ from its parent's (noise_removal defaults to enabled),
                // so a new nested table gets every field.
                if !top {
                    before = &empty;
                }
            }
            if let Some(inner) = table.get_mut(key).and_then(Item::as_table_like_mut) {
                apply(inner, before, child, false);
            }
        } else {
            let mut edited = edit_value(value);
            // Assigning through get_mut keeps the key, and with it the comments above.
            match table.get_mut(key) {
                Some(item) => {
                    if let Some(current) = item.as_value() {
                        *edited.decor_mut() = current.decor().clone();
                    }
                    *item = Item::Value(edited);
                }
                None => {
                    table.insert(key, Item::Value(edited));
                }
            }
        }
    }
    for key in old.keys().filter(|k| !new.contains_key(*k)) {
        keep_comments(table, key);
        table.remove(key);
    }
}

/// Moves the comments above `key` onto the key after it, so they outlive its removal.
fn keep_comments(table: &mut dyn TableLike, key: &str) {
    let Some(comments) = table
        .key(key)
        .and_then(|k| k.leaf_decor().prefix())
        .and_then(|p| p.as_str())
        .filter(|p| p.contains('#'))
        .map(str::to_owned)
    else {
        return;
    };
    let keys: Vec<String> = table.iter().map(|(k, _)| k.to_owned()).collect();
    let Some(next) = keys.iter().skip_while(|k| *k != key).nth(1) else {
        return;
    };
    if let Some(mut next) = table.key_mut(next) {
        let decor = next.leaf_decor_mut();
        let own = decor
            .prefix()
            .and_then(|p| p.as_str())
            .unwrap_or_default()
            .to_owned();
        decor.set_prefix(comments + &own);
    }
}

fn edit_value(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(s) => s.as_str().into(),
        toml::Value::Integer(i) => (*i).into(),
        // Every config float is an f32; writing it widened would give 0.699999988079071.
        #[allow(clippy::cast_possible_truncation)]
        toml::Value::Float(f) => (*f as f32).to_string().parse::<f64>().unwrap_or(*f).into(),
        toml::Value::Boolean(b) => (*b).into(),
        other => other
            .to_string()
            .parse()
            .unwrap_or_else(|_| other.to_string().into()),
    }
}

/// Replaces the file through a rename, writing beside a symlink's target so the link survives.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let dir = target.parent().context("config path has no parent")?;
    fs::create_dir_all(dir)?;
    let name = target.file_name().context("config path has no file name")?;
    let temp = dir.join(format!(".{}.tmp", name.to_string_lossy()));
    let mut file = File::create(&temp).with_context(|| format!("writing {}", temp.display()))?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    if let Ok(meta) = fs::metadata(&target) {
        fs::set_permissions(&temp, meta.permissions())?;
    }
    fs::rename(&temp, &target).with_context(|| format!("replacing {}", target.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use broadcast_linux::config::{InputFormat, LightPreset, ParallelDecode};

    fn temp_dir(name: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "broadcast-linux-gui-test-{name}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    type Edit = (&'static str, fn(&mut Config));

    fn edits() -> Vec<Edit> {
        vec![
            ("mic.enabled", |c| c.mic.enabled = false),
            ("mic.input", |c| c.mic.input = "alsa_input.usb".into()),
            ("mic.noise", |c| c.mic.noise_removal.strength = 0.7),
            ("mic.echo", |c| c.mic.room_echo_removal.enabled = true),
            ("mic.voice", |c| c.mic.studio_voice.enabled = true),
            ("mic.unload", |c| c.mic.unload_after_minutes = 0),
            ("mic.name", |c| c.mic.name = "Mic \"quoted\"".into()),
            ("speaker.enabled", |c| c.speaker.enabled = true),
            ("speaker.output", |c| {
                c.speaker.output = "alsa_output.x".into();
            }),
            ("speaker.noise", |c| c.speaker.noise_removal.enabled = false),
            ("camera.input", |c| c.camera.input = "/dev/video2".into()),
            ("camera.format", |c| {
                c.camera.input_format = InputFormat::Yuyv;
            }),
            ("camera.size", |c| {
                c.camera.width = 1280;
                c.camera.height = 720;
            }),
            ("camera.parallel", |c| {
                c.camera.parallel_decode = ParallelDecode::On;
            }),
            ("camera.background", |c| {
                c.camera.background = Some("~/Pictures/bg.png".into());
            }),
            ("camera.blur", |c| {
                c.camera.background_blur.enabled = true;
                c.camera.background_blur.strength = 0.35;
            }),
            ("camera.light", |c| {
                c.camera.studio_light.enabled = true;
                c.camera.studio_light.preset = LightPreset::Warmer;
            }),
            ("camera.eyes", |c| c.camera.eye_contact.enabled = true),
            ("service.idle", |c| c.service.idle_timeout_seconds = 0),
            ("camera.preset only", |c| {
                c.camera.studio_light.preset = LightPreset::Warm;
            }),
            ("speaker.strength only", |c| {
                c.speaker.noise_removal.strength = 0.25;
            }),
        ]
    }

    fn check_round_trip(start: &str) {
        let dir = temp_dir("round-trip");
        let path = dir.join("config.toml");
        for (name, edit) in edits() {
            fs::write(&path, start).unwrap();
            let mut file = ConfigFile::load(&path).unwrap();
            let mut draft = file.saved.clone();
            edit(&mut draft);
            file.write(&draft, false).unwrap();
            let text = fs::read_to_string(&path).unwrap();
            assert_eq!(Config::parse(&text).unwrap(), draft, "{name}:\n{text}");
            for line in start.lines().filter(|l| l.starts_with('#')) {
                assert!(text.contains(line), "{name} lost comment {line:?}");
            }
            assert!(!text.contains("0.69999"), "{name}:\n{text}");
        }
    }

    #[test]
    fn edits_the_template_in_place() {
        check_round_trip(TEMPLATE);
    }

    #[test]
    fn edits_section_tables_and_sparse_files() {
        check_round_trip(
            "# mine\n[mic]\ninput = \"x\"\n\n[mic.noise_removal]\nstrength = 0.5 # half\n",
        );
        check_round_trip("");
    }

    #[test]
    fn keeps_value_comments() {
        let dir = temp_dir("comments");
        let path = dir.join("config.toml");
        fs::write(&path, "[camera]\nfps = 30 # my webcam\n").unwrap();
        let mut file = ConfigFile::load(&path).unwrap();
        let mut draft = file.saved.clone();
        draft.camera.fps = 60;
        file.write(&draft, false).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[camera]\nfps = 60 # my webcam\n"
        );
    }

    #[test]
    fn switches_background_effects() {
        let dir = temp_dir("background");
        let path = dir.join("config.toml");
        fs::write(&path, "[camera]\nbackground = \"a.png\"\n").unwrap();
        let mut file = ConfigFile::load(&path).unwrap();
        let mut draft = file.saved.clone();
        draft.camera.background = None;
        draft.camera.background_removal.enabled = true;
        file.write(&draft, false).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("background ="), "{text}");
        assert!(
            Config::parse(&text)
                .unwrap()
                .camera
                .background_removal
                .enabled
        );
    }

    #[test]
    fn removing_an_option_keeps_its_comments() {
        let dir = temp_dir("removed");
        let path = dir.join("config.toml");
        let start = TEMPLATE.replace(
            "# background = \"~/Pictures/background.jpg\"",
            "background = \"~/bg.jpg\"",
        );
        fs::write(&path, &start).unwrap();
        let mut file = ConfigFile::load(&path).unwrap();
        let mut draft = file.saved.clone();
        draft.camera.background = None;
        draft.camera.background_blur.enabled = true;
        file.write(&draft, false).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("bg.jpg"), "{text}");
        assert!(
            text.contains(
                "# Use only one background effect: image, blur or removal.\n# Blur the background"
            ),
            "{text}"
        );
    }

    #[test]
    fn missing_file_starts_from_template() {
        let dir = temp_dir("missing");
        let path = dir.join("sub/config.toml");
        let mut file = ConfigFile::load(&path).unwrap();
        assert_eq!(file.saved, Config::default());
        let mut draft = file.saved.clone();
        draft.camera.auto_frame.enabled = true;
        file.write(&draft, false).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# broadcast-linux configuration"));
        assert!(text.contains("auto_frame = { enabled = true }"), "{text}");
    }

    #[test]
    fn detects_changes_on_disk() {
        let dir = temp_dir("changed");
        let path = dir.join("config.toml");
        fs::write(&path, "[mic]\n").unwrap();
        let mut file = ConfigFile::load(&path).unwrap();
        fs::write(&path, "[mic]\nenabled = false\n").unwrap();
        let draft = file.saved.clone();
        let err = file.write(&draft, false).unwrap_err();
        assert!(err.is::<ChangedOnDisk>());
        file.write(&draft, true).unwrap();
    }

    #[test]
    fn invalid_file_is_not_loaded() {
        let dir = temp_dir("invalid");
        let path = dir.join("config.toml");
        fs::write(&path, "[mic]\nbogus = 1\n").unwrap();
        assert!(ConfigFile::load(&path).is_err());
        let mut file = ConfigFile::defaults(&path).unwrap();
        file.write(&Config::default(), false).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), TEMPLATE);
    }

    #[test]
    fn writes_through_symlinks() {
        let dir = temp_dir("symlink");
        let real = dir.join("dotfiles.toml");
        let link = dir.join("config.toml");
        fs::write(&real, "[camera]\nfps = 30\n").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut file = ConfigFile::load(&link).unwrap();
        let mut draft = file.saved.clone();
        draft.camera.fps = 15;
        file.write(&draft, false).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(fs::read_to_string(&real).unwrap().contains("fps = 15"));
    }
}
