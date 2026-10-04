//! "Start at login" through the current user's `Run` registry key.

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
pub(super) fn is_enabled() -> Result<bool> {
    use std::io::ErrorKind;
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = match hkcu.open_subkey_with_flags(RUN_KEY_PATH, KEY_READ) {
        Ok(key) => key,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).context("failed to open Run key"),
    };
    match key.get_raw_value(RUN_VALUE_NAME) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).context("failed reading Run key value"),
    }
}

#[cfg(not(target_os = "windows"))]
pub(super) fn is_enabled() -> Result<bool> {
    anyhow::bail!("autostart is only supported on Windows")
}

/// Starts `exe_path` at login.
#[cfg(target_os = "windows")]
pub(super) fn enable(exe_path: &Path) -> Result<()> {
    use std::ffi::OsString;

    // Quoted: Windows splits an unquoted Run command at the first space, so a
    // path such as `C:\Program Files\...` would not start.
    let mut command = OsString::from("\"");
    command.push(exe_path);
    command.push("\"");
    run_key()?
        .set_value(RUN_VALUE_NAME, &command)
        .context("failed writing Run key value")
}

/// Stops starting at login. Succeeds when it was not enabled.
#[cfg(target_os = "windows")]
pub(super) fn disable() -> Result<()> {
    match run_key()?.delete_value(RUN_VALUE_NAME) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).context("failed deleting Run key value"),
    }
}

#[cfg(target_os = "windows")]
fn run_key() -> Result<winreg::RegKey> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(RUN_KEY_PATH)
        .context("failed to create/open Run key")?;
    Ok(key)
}

#[cfg(not(target_os = "windows"))]
pub(super) fn enable(_exe_path: &Path) -> Result<()> {
    anyhow::bail!("autostart is only supported on Windows")
}

#[cfg(not(target_os = "windows"))]
pub(super) fn disable() -> Result<()> {
    anyhow::bail!("autostart is only supported on Windows")
}
