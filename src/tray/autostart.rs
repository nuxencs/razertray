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
pub(crate) fn is_enabled() -> Result<bool> {
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
pub(crate) fn is_enabled() -> Result<bool> {
    Ok(false)
}

#[cfg(target_os = "windows")]
pub(crate) fn set_enabled(exe_path: &Path, enabled: bool) -> Result<()> {
    use std::ffi::OsString;
    use std::io::ErrorKind;
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey(RUN_KEY_PATH)
        .context("failed to create/open Run key")?;

    if enabled {
        // Quoted: Windows splits an unquoted Run command at the first space,
        // so a path such as `C:\Program Files\...` would not start.
        let mut command = OsString::from("\"");
        command.push(exe_path);
        command.push("\"");
        key.set_value(RUN_VALUE_NAME, &command)
            .context("failed writing Run key value")?;
    } else {
        match key.delete_value(RUN_VALUE_NAME) {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(err).context("failed deleting Run key value"),
        }
    }

    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn set_enabled(_exe_path: &Path, _enabled: bool) -> Result<()> {
    Ok(())
}
