use anyhow::Result;
use std::path::Path;

#[cfg(target_os = "windows")]
use crate::APP_ID;
#[cfg(target_os = "windows")]
use anyhow::Context;

#[cfg(target_os = "windows")]
const RUN_KEY_PATH: &str = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run";
#[cfg(target_os = "windows")]
const RUN_VALUE_NAME: &str = APP_ID;

#[cfg(target_os = "windows")]
pub fn is_enabled(exe_path: &Path) -> Result<bool> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = match hkcu.open_subkey_with_flags(RUN_KEY_PATH, KEY_READ) {
        Ok(key) => key,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).context("failed to open Run key"),
    };
    let value: String = match key.get_value(RUN_VALUE_NAME) {
        Ok(value) => value,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).context("failed reading Run key value"),
    };
    Ok(command_targets_executable(&value, exe_path))
}

fn command_targets_executable(command: &str, exe_path: &Path) -> bool {
    let trimmed = command.trim();
    let target = trimmed
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(trimmed);
    target.eq_ignore_ascii_case(&exe_path.to_string_lossy())
}

#[cfg(not(target_os = "windows"))]
pub fn is_enabled(_exe_path: &Path) -> Result<bool> {
    Ok(false)
}

#[cfg(target_os = "windows")]
pub fn set_enabled(exe_path: &Path, enabled: bool) -> Result<()> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey(RUN_KEY_PATH)
        .context("failed to create/open Run key")?;

    if enabled {
        let command = format!("\"{}\"", exe_path.display());
        key.set_value(RUN_VALUE_NAME, &command)
            .context("failed writing Run key value")?;
    } else if let Err(err) = key.delete_value(RUN_VALUE_NAME)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        return Err(err).context("failed deleting Run key value");
    }

    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn set_enabled(_exe_path: &Path, _enabled: bool) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::command_targets_executable;
    use std::path::Path;

    #[test]
    fn recognizes_legacy_and_quoted_startup_commands() {
        let path = Path::new(r"C:\Program Files\razertray\razertray.exe");

        assert!(command_targets_executable(
            r"C:\Program Files\razertray\razertray.exe",
            path
        ));
        assert!(command_targets_executable(
            r#""C:\Program Files\razertray\razertray.exe""#,
            path
        ));
        assert!(!command_targets_executable(
            r#""C:\Other\razertray.exe""#,
            path
        ));
    }
}
