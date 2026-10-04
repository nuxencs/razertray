//! Settings in `config.toml` and the transaction-ID cache in `pid_cache.toml`.

use crate::APP_ID;
use crate::model::DeviceKey;
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

/// Shortest poll interval. Each poll sends HID requests to every Razer
/// device, so a smaller value only adds USB traffic and wakes the mouse.
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// User settings. The field names are the `config.toml` keys documented in the README.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub(crate) struct AppConfig {
    pub(crate) poll_interval_seconds: u64,
    pub(crate) low_battery_threshold: u8,
    pub(crate) low_battery_cooldown_minutes: u64,
    /// Stored as `""` when no device is selected, as in v0.2.0.
    #[serde(rename = "selected_device_id", with = "empty_is_none")]
    pub(crate) selected_device: Option<DeviceKey>,
    pub(crate) autostart: bool,
    /// A `tracing` filter directive, such as `info` or `razertray=debug`.
    pub(crate) log_level: String,
    #[serde(default)]
    pub(crate) view_mode: ViewMode,
}

impl AppConfig {
    /// Loads `config.toml`, creating it with defaults when it does not exist.
    pub(crate) fn load() -> Loaded<Self> {
        load_or_create(&config_path())
    }

    pub(crate) fn save(&self) -> Result<()> {
        save(&config_path(), self)
    }

    /// The poll interval, raised to at least [`MIN_POLL_INTERVAL`].
    pub(crate) fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_interval_seconds).max(MIN_POLL_INTERVAL)
    }

    pub(crate) fn low_battery_cooldown(&self) -> Duration {
        Duration::from_secs(self.low_battery_cooldown_minutes.saturating_mul(60))
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            poll_interval_seconds: 60,
            low_battery_threshold: 15,
            low_battery_cooldown_minutes: 120,
            selected_device: None,
            autostart: false,
            log_level: "info".to_owned(),
            view_mode: ViewMode::default(),
        }
    }
}

/// How the tray icon shows the battery level.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub(crate) enum ViewMode {
    /// A battery glyph that fills up and changes color.
    #[default]
    Icon,
    /// The percentage as digits.
    Text,
}

impl ViewMode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Icon => "icon",
            Self::Text => "text",
        }
    }
}

impl fmt::Display for ViewMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Case-insensitive, because v0.2.0 accepted any case.
impl FromStr for ViewMode {
    type Err = ParseViewModeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("icon") {
            Ok(Self::Icon)
        } else if s.eq_ignore_ascii_case("text") {
            Ok(Self::Text)
        } else {
            Err(ParseViewModeError(s.to_owned()))
        }
    }
}

impl<'de> Deserialize<'de> for ViewMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl Serialize for ViewMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// A `view_mode` value other than `icon` or `text`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ParseViewModeError(String);

impl fmt::Display for ParseViewModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid view mode {:?}, expected \"icon\" or \"text\"",
            self.0
        )
    }
}

impl std::error::Error for ParseViewModeError {}

/// Serde adapter: `None` is stored as an empty string.
mod empty_is_none {
    use crate::model::DeviceKey;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        key: &Option<DeviceKey>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(key.as_ref().map_or("", DeviceKey::as_str))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<DeviceKey>, D::Error> {
        let key = Option::<DeviceKey>::deserialize(deserializer)?;
        Ok(key.filter(|key| !key.as_str().is_empty()))
    }
}

/// Transaction IDs found by probing devices that are not in the device map.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub(crate) struct PidCache {
    /// Keyed by product ID as 4 hex digits; TOML keys must be strings.
    transaction_ids: BTreeMap<String, u8>,
}

impl PidCache {
    /// Loads `pid_cache.toml`, creating it when it does not exist.
    pub(crate) fn load() -> Loaded<Self> {
        load_or_create(&pid_cache_path())
    }

    pub(crate) fn save(&self) -> Result<()> {
        save(&pid_cache_path(), self)
    }

    pub(crate) fn get(&self, pid: u16) -> Option<u8> {
        self.transaction_ids.get(&format!("{pid:04X}")).copied()
    }

    /// Stores the ID for `pid` and returns the previous one, as `HashMap::insert` does.
    pub(crate) fn insert(&mut self, pid: u16, transaction_id: u8) -> Option<u8> {
        self.transaction_ids
            .insert(format!("{pid:04X}"), transaction_id)
    }
}

fn data_dir() -> PathBuf {
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

fn config_path() -> PathBuf {
    data_dir().join("config.toml")
}

fn pid_cache_path() -> PathBuf {
    data_dir().join("pid_cache.toml")
}

pub(crate) fn log_path() -> PathBuf {
    data_dir().join(format!("{APP_ID}.log"))
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
    write_synced(&tmp_path, raw)?;

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

fn write_synced(path: &Path, raw: &[u8]) -> Result<()> {
    let mut file =
        fs::File::create(path).with_context(|| format!("failed creating {}", file_label(path)))?;
    file.write_all(raw)
        .with_context(|| format!("failed writing {}", file_label(path)))?;
    file.sync_all()
        .with_context(|| format!("failed syncing {}", file_label(path)))
}

/// File name only, so errors and logs do not leak the user's home directory.
pub(crate) fn file_label(path: &Path) -> std::path::Display<'_> {
    Path::new(path.file_name().unwrap_or(path.as_os_str())).display()
}

/// A loaded settings file, plus the problem that forced a fallback to defaults.
#[derive(Debug)]
pub(crate) struct Loaded<T> {
    pub(crate) value: T,
    /// Set when the file existed but could not be used. The caller logs it.
    pub(crate) problem: Option<anyhow::Error>,
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

#[cfg(test)]
mod tests {
    use super::{AppConfig, PidCache, ViewMode, load_or_create};
    use crate::model::DeviceKey;
    use std::fs;
    use std::time::Duration;

    /// Settings block from the v0.2.0 README; existing user files look like this.
    const V0_2_0_CONFIG: &str = r#"
poll_interval_seconds = 60        # how often to check the battery (minimum 5)
low_battery_threshold = 15        # warn at or below this percentage
low_battery_cooldown_minutes = 120  # minimum gap between repeat warnings
selected_device_id = ""           # which device to watch (set from the menu)
autostart = false                 # start with Windows (toggle from the tray menu)
log_level = "info"                # detail level for the log file
"#;

    fn parse(raw: &str) -> AppConfig {
        toml::from_str(raw).expect("parse config")
    }

    #[test]
    fn v0_2_0_config_parses_to_defaults() {
        assert_eq!(parse(V0_2_0_CONFIG), AppConfig::default());
    }

    #[test]
    fn view_mode_is_case_insensitive_like_v0_2_0() {
        let cfg = parse(&format!("{V0_2_0_CONFIG}view_mode = \"Text\"\n"));
        assert_eq!(cfg.view_mode, ViewMode::Text);

        let invalid = toml::from_str::<AppConfig>(&format!("{V0_2_0_CONFIG}view_mode = \"big\"\n"))
            .expect_err("unknown view mode");
        assert!(
            invalid.to_string().contains("invalid view mode"),
            "{invalid}"
        );
    }

    #[test]
    fn selected_device_roundtrips_and_empty_means_none() {
        let mut cfg = AppConfig::default();
        let raw = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(raw.contains("selected_device_id = \"\""), "{raw}");
        assert_eq!(parse(&raw), cfg);

        cfg.selected_device = Some(DeviceKey::new(0x00B6, Some("XYZ")));
        cfg.view_mode = ViewMode::Text;
        let raw = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(raw.contains("selected_device_id = \"00B6:XYZ\""), "{raw}");
        assert_eq!(parse(&raw), cfg);
    }

    #[test]
    fn poll_interval_has_a_floor() {
        let interval = |poll_interval_seconds| {
            AppConfig {
                poll_interval_seconds,
                ..AppConfig::default()
            }
            .poll_interval()
        };
        assert_eq!(interval(1), Duration::from_secs(5));
        assert_eq!(interval(90), Duration::from_secs(90));
    }

    #[test]
    fn pid_cache_insert_returns_previous_id() {
        let mut cache = PidCache::default();
        assert_eq!(cache.insert(0x00B6, 0x1F), None);
        assert_eq!(cache.insert(0x00B6, 0x3F), Some(0x1F));
        assert_eq!(cache.get(0x00B6), Some(0x3F));
        assert_eq!(cache.get(0x00B7), None);
    }

    #[test]
    fn pid_cache_file_format_is_stable() {
        let mut cache = PidCache::default();
        cache.insert(0x00B6, 0x1F);
        let raw = toml::to_string_pretty(&cache).expect("serialize");
        assert_eq!(raw, "[transaction_ids]\n00B6 = 31\n");
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
        assert_eq!(loaded.value, AppConfig::default());
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
