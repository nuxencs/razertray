use crate::APP_ID;
use crate::model::PollError;
use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewMode {
    #[default]
    Icon,
    Text,
}

impl<'de> Deserialize<'de> for ViewMode {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.eq_ignore_ascii_case("icon") {
            Ok(Self::Icon)
        } else if value.eq_ignore_ascii_case("text") {
            Ok(Self::Text)
        } else {
            Err(serde::de::Error::unknown_variant(&value, &["icon", "text"]))
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertScope {
    #[default]
    Selected,
    All,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct AppConfig {
    pub poll_interval_seconds: u64,
    pub low_battery_threshold: u8,
    pub low_battery_cooldown_minutes: u64,
    pub selected_device_id: String,
    pub log_level: String,
    pub view_mode: ViewMode,
    pub alert_scope: AlertScope,
    pub welcome_shown: bool,
}

impl AppConfig {
    /// True when the tray should render the percentage as text instead of the
    /// battery icon.
    pub fn text_mode(&self) -> bool {
        self.view_mode == ViewMode::Text
    }

    pub fn validate(&mut self) {
        self.poll_interval_seconds = self.poll_interval_seconds.clamp(5, 3_600);
        self.low_battery_threshold = self.low_battery_threshold.clamp(1, 100);
        self.low_battery_cooldown_minutes = self.low_battery_cooldown_minutes.clamp(1, 10_080);
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            poll_interval_seconds: 60,
            low_battery_threshold: 15,
            low_battery_cooldown_minutes: 120,
            selected_device_id: String::new(),
            log_level: "info".to_string(),
            view_mode: ViewMode::default(),
            alert_scope: AlertScope::default(),
            welcome_shown: false,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PidCache {
    pub transaction_ids: BTreeMap<String, u8>,
}

#[derive(Clone, Debug)]
pub struct PidCacheLoad {
    pub cache: PidCache,
    pub diagnostic: Option<PollError>,
}

impl PidCache {
    pub fn get(&self, pid: u16) -> Option<u8> {
        let key = format!("{:04X}", pid);
        self.transaction_ids.get(&key).copied()
    }

    pub fn set(&mut self, pid: u16, transaction_id: u8) -> bool {
        let key = format!("{:04X}", pid);
        self.transaction_ids.insert(key, transaction_id) != Some(transaction_id)
    }

    pub fn remove(&mut self, pid: u16) -> bool {
        let key = format!("{:04X}", pid);
        self.transaction_ids.remove(&key).is_some()
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

#[derive(Clone, Debug)]
pub struct ConfigLoad {
    pub config: AppConfig,
    pub recovery: Option<ConfigRecovery>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigRecovery {
    InvalidFileReset {
        backup_path: PathBuf,
        parse_error: String,
    },
    ValuesAdjusted,
}

impl ConfigRecovery {
    pub fn title(&self) -> &'static str {
        match self {
            Self::InvalidFileReset { .. } => "Configuration was reset",
            Self::ValuesAdjusted => "Configuration was adjusted",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::InvalidFileReset {
                backup_path,
                parse_error,
            } => format!(
                "The configuration was invalid and was reset. The old file is {}. Parse error: {parse_error}",
                backup_path.display()
            ),
            Self::ValuesAdjusted => {
                "Unsafe configuration values were adjusted to supported limits.".to_string()
            }
        }
    }
}

fn write_atomic(path: &Path, raw: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("missing parent directory for {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed creating {}", parent.display()))?;
    let file_name = path
        .file_name()
        .with_context(|| format!("missing file name for {}", path.display()))?
        .to_string_lossy();

    let tmp_path = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
    {
        let mut tmp = fs::File::create(&tmp_path)
            .with_context(|| format!("failed creating {}", tmp_path.display()))?;
        tmp.write_all(raw)
            .with_context(|| format!("failed writing {}", tmp_path.display()))?;
        tmp.sync_all()
            .with_context(|| format!("failed syncing {}", tmp_path.display()))?;
    }

    #[cfg(target_os = "windows")]
    let backup_path = replacement_backup_path(path)?;

    #[cfg(target_os = "windows")]
    if path.exists() {
        if backup_path.exists() {
            fs::remove_file(&backup_path)
                .with_context(|| format!("failed removing {}", backup_path.display()))?;
        }
        fs::rename(path, &backup_path).with_context(|| {
            format!(
                "failed preparing replacement of {} with backup {}",
                path.display(),
                backup_path.display()
            )
        })?;
    }

    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        #[cfg(target_os = "windows")]
        if backup_path.exists() {
            let _ = fs::rename(&backup_path, path);
        }
        return Err(err).with_context(|| {
            format!(
                "failed renaming {} to {}",
                tmp_path.display(),
                path.display()
            )
        });
    }

    #[cfg(target_os = "windows")]
    if backup_path.exists() {
        let _ = fs::remove_file(backup_path);
    }

    Ok(())
}

fn replacement_backup_path(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .with_context(|| format!("missing file name for {}", path.display()))?
        .to_string_lossy();
    Ok(path.with_file_name(format!(".{file_name}.replace-backup")))
}

fn restore_interrupted_replacement(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let backup = replacement_backup_path(path)?;
    if backup.exists() {
        fs::rename(&backup, path).with_context(|| {
            format!(
                "failed restoring interrupted replacement from {} to {}",
                backup.display(),
                path.display()
            )
        })?;
    }
    Ok(())
}

fn quarantine_invalid_file(path: &Path) -> Result<PathBuf> {
    let stem = path
        .file_stem()
        .with_context(|| format!("missing file stem for {}", path.display()))?
        .to_string_lossy();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let backup = path.with_file_name(format!(
        "{stem}.invalid.{timestamp}.{}.toml",
        std::process::id()
    ));
    fs::rename(path, &backup).with_context(|| {
        format!(
            "failed moving invalid file {} to {}",
            path.display(),
            backup.display()
        )
    })?;
    Ok(backup)
}

pub fn load_or_create_config() -> Result<ConfigLoad> {
    let path = config_path();
    load_or_create_config_at(&path)
}

fn load_or_create_config_at(path: &Path) -> Result<ConfigLoad> {
    #[cfg(target_os = "windows")]
    restore_interrupted_replacement(path)?;
    if !path.exists() {
        let default_cfg = AppConfig::default();
        save_config_at(path, &default_cfg)?;
        return Ok(ConfigLoad {
            config: default_cfg,
            recovery: None,
        });
    }

    let raw =
        fs::read_to_string(path).with_context(|| format!("failed reading {}", path.display()))?;
    let mut parsed: AppConfig = match toml::from_str(&raw) {
        Ok(config) => config,
        Err(err) => {
            let backup = quarantine_invalid_file(path)?;
            let config = AppConfig::default();
            save_config_at(path, &config)?;
            return Ok(ConfigLoad {
                config,
                recovery: Some(ConfigRecovery::InvalidFileReset {
                    backup_path: backup,
                    parse_error: err.to_string(),
                }),
            });
        }
    };
    let before = parsed.clone();
    parsed.validate();
    let recovery = if parsed != before {
        save_config_at(path, &parsed)?;
        Some(ConfigRecovery::ValuesAdjusted)
    } else {
        None
    };
    Ok(ConfigLoad {
        config: parsed,
        recovery,
    })
}

pub fn save_config(cfg: &AppConfig) -> Result<()> {
    let path = config_path();
    save_config_at(&path, cfg)
}

fn save_config_at(path: &Path, cfg: &AppConfig) -> Result<()> {
    let raw = toml::to_string_pretty(cfg).context("failed serializing config")?;
    write_atomic(path, raw.as_bytes())?;
    Ok(())
}

pub fn load_or_create_pid_cache() -> Result<PidCache> {
    let path = pid_cache_path();
    load_or_create_pid_cache_at(&path)
}

pub fn load_pid_cache_for_polling() -> PidCacheLoad {
    let path = pid_cache_path();
    load_pid_cache_for_polling_at(&path)
}

fn load_pid_cache_for_polling_at(path: &Path) -> PidCacheLoad {
    match load_or_create_pid_cache_at(path) {
        Ok(cache) => PidCacheLoad {
            cache,
            diagnostic: None,
        },
        Err(error) => {
            let message = format!("PID cache unavailable: {error:#}");
            PidCacheLoad {
                cache: PidCache::default(),
                diagnostic: Some(PollError::subsystem_component("PID cache", message)),
            }
        }
    }
}

fn load_or_create_pid_cache_at(path: &Path) -> Result<PidCache> {
    #[cfg(target_os = "windows")]
    restore_interrupted_replacement(path)?;
    if !path.exists() {
        let cache = PidCache::default();
        save_pid_cache_at(path, &cache)?;
        return Ok(cache);
    }

    let raw =
        fs::read_to_string(path).with_context(|| format!("failed reading {}", path.display()))?;
    match toml::from_str(&raw) {
        Ok(parsed) => Ok(parsed),
        Err(err) => {
            let backup = quarantine_invalid_file(path)?;
            let cache = PidCache::default();
            save_pid_cache_at(path, &cache)?;
            tracing::warn!(
                "invalid PID cache was reset and preserved at {}: {err}",
                backup.display()
            );
            Ok(cache)
        }
    }
}

pub fn save_pid_cache(cache: &PidCache) -> Result<()> {
    let path = pid_cache_path();
    save_pid_cache_at(&path, cache)
}

fn save_pid_cache_at(path: &Path, cache: &PidCache) -> Result<()> {
    let raw = toml::to_string_pretty(cache).context("failed serializing pid cache")?;
    write_atomic(path, raw.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        AppConfig, ConfigRecovery, PidCache, load_or_create_config_at, load_or_create_pid_cache_at,
        load_pid_cache_for_polling_at, replacement_backup_path, restore_interrupted_replacement,
    };
    use crate::model::{PollErrorKind, PollErrorScope};
    use std::fs;
    use std::path::Path;

    #[test]
    fn pid_cache_get_set_roundtrip() {
        let mut cache = PidCache::default();
        assert_eq!(cache.get(0x1532), None);
        assert!(cache.set(0x1532, 0x3F));
        assert!(!cache.set(0x1532, 0x3F));
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
        assert_eq!(parsed.log_level, cfg.log_level);
        assert_eq!(parsed.view_mode, cfg.view_mode);
        assert_eq!(parsed.alert_scope, cfg.alert_scope);
        assert_eq!(parsed.welcome_shown, cfg.welcome_shown);
    }

    #[test]
    fn missing_new_fields_use_defaults() {
        let parsed: AppConfig = toml::from_str(
            r#"
poll_interval_seconds = 60
low_battery_threshold = 15
low_battery_cooldown_minutes = 120
selected_device_id = ""
autostart = false
log_level = "info"
"#,
        )
        .expect("parse old config");

        assert_eq!(parsed.view_mode, super::ViewMode::Icon);
        assert_eq!(parsed.alert_scope, super::AlertScope::Selected);
        assert!(!parsed.welcome_shown);
    }

    #[test]
    fn legacy_view_mode_case_preserves_other_configuration() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let path = temp.path().join("config.toml");
        fs::write(
            &path,
            r#"
poll_interval_seconds = 300
low_battery_threshold = 25
low_battery_cooldown_minutes = 240
selected_device_id = "preferred-mouse"
log_level = "debug"
view_mode = "TeXt"
alert_scope = "all"
welcome_shown = true
"#,
        )
        .expect("write legacy config");

        let loaded = load_or_create_config_at(&path).expect("load legacy config");

        assert_eq!(loaded.config.view_mode, super::ViewMode::Text);
        assert_eq!(loaded.config.poll_interval_seconds, 300);
        assert_eq!(loaded.config.low_battery_threshold, 25);
        assert_eq!(loaded.config.low_battery_cooldown_minutes, 240);
        assert_eq!(loaded.config.selected_device_id, "preferred-mouse");
        assert_eq!(loaded.config.log_level, "debug");
        assert_eq!(loaded.config.alert_scope, super::AlertScope::All);
        assert!(loaded.config.welcome_shown);
        assert!(loaded.recovery.is_none());
        let serialized = toml::Value::try_from(&loaded.config).expect("serialize loaded config");
        assert_eq!(serialized["view_mode"].as_str(), Some("text"));
    }

    #[test]
    fn config_validation_clamps_unsafe_values() {
        let mut cfg = AppConfig {
            poll_interval_seconds: 0,
            low_battery_threshold: 200,
            low_battery_cooldown_minutes: 0,
            ..AppConfig::default()
        };
        cfg.validate();
        assert_eq!(cfg.poll_interval_seconds, 5);
        assert_eq!(cfg.low_battery_threshold, 100);
        assert_eq!(cfg.low_battery_cooldown_minutes, 1);
    }

    #[test]
    fn replacement_backup_name_is_shared_by_all_persisted_files() {
        assert_eq!(
            replacement_backup_path(Path::new("config.toml")).unwrap(),
            Path::new(".config.toml.replace-backup")
        );
        assert_eq!(
            replacement_backup_path(Path::new("pid_cache.toml")).unwrap(),
            Path::new(".pid_cache.toml.replace-backup")
        );
    }

    #[test]
    fn config_recovery_resets_invalid_file() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let path = temp.path().join("config.toml");
        fs::write(&path, "not = [valid").expect("write invalid config");

        let loaded = load_or_create_config_at(&path).expect("recover invalid config");

        assert_eq!(loaded.config, AppConfig::default());
        let recovery = loaded.recovery.as_ref().expect("config recovery");
        assert_eq!(recovery.title(), "Configuration was reset");
        match recovery {
            ConfigRecovery::InvalidFileReset {
                backup_path,
                parse_error,
            } => {
                assert!(backup_path.exists());
                assert!(!parse_error.is_empty());
            }
            ConfigRecovery::ValuesAdjusted => panic!("expected invalid-file reset"),
        }
        let parsed: AppConfig =
            toml::from_str(&fs::read_to_string(&path).expect("read replacement config"))
                .expect("replacement config is valid");
        assert_eq!(parsed, AppConfig::default());
        let backups: Vec<_> = fs::read_dir(temp.path())
            .expect("list temporary directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("config.invalid.")
            })
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            fs::read_to_string(backups[0].path()).expect("read preserved config"),
            "not = [valid"
        );
    }

    #[test]
    fn config_recovery_adjusts_values_without_reset() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let path = temp.path().join("config.toml");
        let config = AppConfig {
            poll_interval_seconds: 1,
            low_battery_threshold: 25,
            low_battery_cooldown_minutes: 240,
            selected_device_id: "preferred-mouse".to_string(),
            log_level: "debug".to_string(),
            view_mode: super::ViewMode::Text,
            alert_scope: super::AlertScope::All,
            welcome_shown: true,
        };
        fs::write(
            &path,
            toml::to_string_pretty(&config).expect("serialize config"),
        )
        .expect("write config");

        let loaded = load_or_create_config_at(&path).expect("adjust config");

        assert_eq!(loaded.recovery, Some(ConfigRecovery::ValuesAdjusted));
        assert_eq!(
            loaded.recovery.as_ref().map(ConfigRecovery::title),
            Some("Configuration was adjusted")
        );
        assert_eq!(
            loaded.recovery.as_ref().map(ConfigRecovery::message),
            Some("Unsafe configuration values were adjusted to supported limits.".to_string())
        );
        assert_eq!(loaded.config.poll_interval_seconds, 5);
        assert_eq!(loaded.config.low_battery_threshold, 25);
        assert_eq!(loaded.config.low_battery_cooldown_minutes, 240);
        assert_eq!(loaded.config.selected_device_id, "preferred-mouse");
        assert_eq!(loaded.config.log_level, "debug");
        assert_eq!(loaded.config.view_mode, super::ViewMode::Text);
        assert_eq!(loaded.config.alert_scope, super::AlertScope::All);
        assert!(loaded.config.welcome_shown);
        let persisted: AppConfig =
            toml::from_str(&fs::read_to_string(&path).expect("read adjusted config"))
                .expect("parse adjusted config");
        assert_eq!(persisted, loaded.config);
    }

    #[test]
    fn invalid_pid_cache_is_preserved_and_replaced() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let path = temp.path().join("pid_cache.toml");
        fs::write(&path, "transaction_ids = nope").expect("write invalid cache");

        let loaded = load_or_create_pid_cache_at(&path).expect("recover invalid cache");

        assert!(loaded.transaction_ids.is_empty());
        let parsed: PidCache =
            toml::from_str(&fs::read_to_string(&path).expect("read replacement cache"))
                .expect("replacement cache is valid");
        assert!(parsed.transaction_ids.is_empty());
        assert_eq!(
            fs::read_dir(temp.path())
                .expect("list temporary directory")
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("pid_cache.invalid."))
                .count(),
            1
        );
    }

    #[test]
    fn review_round_17_optional_pid_cache_failure_is_typed() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let path = temp.path().join("pid_cache.toml");
        fs::create_dir(&path).expect("create unreadable cache path");

        let loaded = load_pid_cache_for_polling_at(&path);

        assert!(loaded.cache.transaction_ids.is_empty());
        let diagnostic = loaded.diagnostic.expect("cache diagnostic");
        assert_eq!(diagnostic.display_name, "PID cache");
        assert_eq!(diagnostic.scope, PollErrorScope::Subsystem);
        assert_eq!(diagnostic.kind, PollErrorKind::DeviceUnavailable);
    }

    #[test]
    fn interrupted_replacement_restores_backup() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let path = temp.path().join("config.toml");
        let backup = replacement_backup_path(&path).expect("resolve backup path");
        fs::write(&backup, "preserved").expect("write replacement backup");

        restore_interrupted_replacement(&path).expect("restore replacement backup");

        assert_eq!(
            fs::read_to_string(path).expect("read restored file"),
            "preserved"
        );
        assert!(!backup.exists());
    }
}
