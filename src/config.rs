use crate::APP_ID;
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AppConfig {
    pub poll_interval_seconds: u64,
    pub low_battery_threshold: u8,
    pub low_battery_cooldown_minutes: u64,
    pub selected_device_id: String,
    pub autostart: bool,
    pub log_level: String,
    /// Tray display style: "icon" (battery glyph) or "text" (percentage number).
    #[serde(default = "default_view_mode")]
    pub view_mode: String,
}

fn default_view_mode() -> String {
    "icon".to_string()
}

impl AppConfig {
    /// True when the tray should render the percentage as text instead of the
    /// battery icon.
    pub fn text_mode(&self) -> bool {
        self.view_mode.eq_ignore_ascii_case("text")
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            poll_interval_seconds: 60,
            low_battery_threshold: 15,
            low_battery_cooldown_minutes: 120,
            selected_device_id: String::new(),
            autostart: false,
            log_level: "info".to_string(),
            view_mode: default_view_mode(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PidCache {
    pub transaction_ids: BTreeMap<String, u8>,
}

impl PidCache {
    pub fn get(&self, pid: u16) -> Option<u8> {
        let key = format!("{:04X}", pid);
        self.transaction_ids.get(&key).copied()
    }

    pub fn set(&mut self, pid: u16, transaction_id: u8) {
        let key = format!("{:04X}", pid);
        self.transaction_ids.insert(key, transaction_id);
    }
}

pub fn app_data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join(APP_ID);
        }
    }

    if let Some(mut dir) = dirs::config_dir() {
        dir.push(APP_ID);
        return dir;
    }

    PathBuf::from(".").join(APP_ID)
}

pub fn config_path() -> PathBuf {
    app_data_dir().join("config.toml")
}

pub fn pid_cache_path() -> PathBuf {
    app_data_dir().join("pid_cache.toml")
}

pub fn log_path() -> PathBuf {
    app_data_dir().join(format!("{APP_ID}.log"))
}

fn write_atomic(path: &Path, raw: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("missing parent directory for {}", file_label(path)))?;
    fs::create_dir_all(parent).context("failed creating the app data directory")?;
    let file_name = path
        .file_name()
        .with_context(|| format!("missing file name for {}", file_label(path)))?
        .to_string_lossy();

    let tmp_path = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
    {
        let mut tmp = fs::File::create(&tmp_path)
            .with_context(|| format!("failed creating {}", file_label(&tmp_path)))?;
        tmp.write_all(raw)
            .with_context(|| format!("failed writing {}", file_label(&tmp_path)))?;
        tmp.sync_all()
            .with_context(|| format!("failed syncing {}", file_label(&tmp_path)))?;
    }

    #[cfg(target_os = "windows")]
    if path.exists() {
        fs::remove_file(path).with_context(|| format!("failed replacing {}", file_label(path)))?;
    }

    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err).with_context(|| format!("failed replacing {}", file_label(path)));
    }

    Ok(())
}

/// File name only, so errors and logs do not leak the user's home directory.
fn file_label(path: &Path) -> std::path::Display<'_> {
    Path::new(path.file_name().unwrap_or(path.as_os_str())).display()
}

/// A loaded settings file, plus the problem that forced a fallback to defaults.
#[derive(Debug)]
pub struct Loaded<T> {
    pub value: T,
    /// Set when the file existed but could not be used. The caller logs it.
    pub problem: Option<anyhow::Error>,
}

/// Loads `path`, or creates it with defaults when it does not exist.
///
/// A file that cannot be read or parsed is moved to `<name>.invalid` and
/// replaced with defaults. Later saves therefore never overwrite the user's
/// broken file, and the user can fix and restore it.
fn load_or_create<T>(path: &Path) -> Loaded<T>
where
    T: Default + DeserializeOwned + Serialize,
{
    let error = match fs::read_to_string(path) {
        Ok(raw) => match toml::from_str(&raw) {
            Ok(value) => {
                return Loaded {
                    value,
                    problem: None,
                };
            }
            Err(err) => {
                anyhow::Error::new(err).context(format!("failed parsing {}", file_label(path)))
            }
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let value = T::default();
            let problem = save(path, &value).err();
            return Loaded { value, problem };
        }
        Err(err) => anyhow::Error::new(err).context(format!("failed reading {}", file_label(path))),
    };

    let mut backup = path.as_os_str().to_owned();
    backup.push(".invalid");
    let backup = PathBuf::from(backup);
    let value = T::default();
    let problem = match fs::rename(path, &backup) {
        Ok(()) => {
            let error = error.context(format!(
                "using defaults; moved the old file to {}",
                file_label(&backup)
            ));
            match save(path, &value) {
                Ok(()) => error,
                Err(save_err) => {
                    error.context(format!("also failed writing defaults: {save_err:#}"))
                }
            }
        }
        Err(rename_err) => error.context(format!(
            "using defaults; could not move the file aside: {rename_err}"
        )),
    };

    Loaded {
        value,
        problem: Some(problem),
    }
}

fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let raw = toml::to_string_pretty(value)
        .with_context(|| format!("failed serializing {}", file_label(path)))?;
    write_atomic(path, raw.as_bytes())
}

pub fn load_config() -> Loaded<AppConfig> {
    load_or_create(&config_path())
}

pub fn save_config(cfg: &AppConfig) -> Result<()> {
    save(&config_path(), cfg)
}

pub fn load_pid_cache() -> Loaded<PidCache> {
    load_or_create(&pid_cache_path())
}

pub fn save_pid_cache(cache: &PidCache) -> Result<()> {
    save(&pid_cache_path(), cache)
}

#[cfg(test)]
mod tests {
    use super::{AppConfig, PidCache, load_or_create};
    use std::fs;

    #[test]
    fn pid_cache_get_set_roundtrip() {
        let mut cache = PidCache::default();
        assert_eq!(cache.get(0x1532), None);
        cache.set(0x1532, 0x3F);
        assert_eq!(cache.get(0x1532), Some(0x3F));
    }

    #[test]
    fn config_toml_roundtrip() {
        let cfg = AppConfig::default();
        let raw = toml::to_string_pretty(&cfg).expect("serialize default config");
        let parsed: AppConfig = toml::from_str(&raw).expect("parse config");

        assert_eq!(parsed.poll_interval_seconds, cfg.poll_interval_seconds);
        assert_eq!(parsed.low_battery_threshold, cfg.low_battery_threshold);
        assert_eq!(
            parsed.low_battery_cooldown_minutes,
            cfg.low_battery_cooldown_minutes
        );
        assert_eq!(parsed.selected_device_id, cfg.selected_device_id);
        assert_eq!(parsed.autostart, cfg.autostart);
        assert_eq!(parsed.log_level, cfg.log_level);
        assert_eq!(parsed.view_mode, cfg.view_mode);
    }

    /// Settings block from the v0.2.0 README; existing user files look like this.
    const V0_2_0_CONFIG: &str = r#"
poll_interval_seconds = 60        # how often to check the battery (minimum 5)
low_battery_threshold = 15        # warn at or below this percentage
low_battery_cooldown_minutes = 120  # minimum gap between repeat warnings
selected_device_id = ""           # which device to watch (set from the menu)
autostart = false                 # start with Windows (toggle from the tray menu)
log_level = "info"                # detail level for the log file
"#;

    #[test]
    fn v0_2_0_config_still_parses() {
        let parsed: AppConfig = toml::from_str(V0_2_0_CONFIG).expect("parse v0.2.0 config");
        assert!(!parsed.text_mode());

        let text_mode: AppConfig =
            toml::from_str(&format!("{V0_2_0_CONFIG}view_mode = \"Text\"\n")).expect("parse");
        assert!(text_mode.text_mode());
    }

    #[test]
    fn missing_file_is_created_with_defaults() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");

        let loaded = load_or_create::<AppConfig>(&path);

        assert!(loaded.problem.is_none());
        let on_disk: AppConfig =
            toml::from_str(&fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(
            on_disk.poll_interval_seconds,
            loaded.value.poll_interval_seconds
        );
    }

    #[test]
    fn invalid_file_is_moved_aside_and_replaced_with_defaults() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        let broken = "poll_interval_seconds = 30\nlow_battery_threshold = \"oops\"\n";
        fs::write(&path, broken).expect("write");

        let loaded = load_or_create::<AppConfig>(&path);

        let problem = format!("{:#}", loaded.problem.expect("problem reported"));
        assert!(problem.contains("config.toml.invalid"), "{problem}");
        assert!(
            !problem.contains(&*dir.path().to_string_lossy()),
            "{problem}"
        );
        assert_eq!(loaded.value.poll_interval_seconds, 60);
        // The user's file survives, and later saves go to a fresh default file.
        assert_eq!(
            fs::read_to_string(dir.path().join("config.toml.invalid")).expect("backup"),
            broken
        );
        assert!(
            fs::read_to_string(&path)
                .expect("defaults")
                .contains("poll_interval_seconds = 60")
        );
    }
}
