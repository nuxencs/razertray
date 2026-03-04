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
pub fn is_enabled() -> Result<bool> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey_with_flags(RUN_KEY_PATH, KEY_READ)
        .context("failed to open Run key")?;
    let value: Result<String, _> = key.get_value(RUN_VALUE_NAME);
    Ok(value.is_ok())
}

#[cfg(not(target_os = "windows"))]
pub fn is_enabled() -> Result<bool> {
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
        key.set_value(RUN_VALUE_NAME, &exe_path.display().to_string())
            .context("failed writing Run key value")?;
    } else {
        let _ = key.delete_value(RUN_VALUE_NAME);
    }

    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn set_enabled(_exe_path: &Path, _enabled: bool) -> Result<()> {
    Ok(())
}
