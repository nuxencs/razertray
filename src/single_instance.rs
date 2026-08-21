use anyhow::Result;

#[cfg(target_os = "windows")]
pub struct InstanceGuard(windows_sys::Win32::Foundation::HANDLE);

#[cfg(not(target_os = "windows"))]
pub struct InstanceGuard;

#[cfg(target_os = "windows")]
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(target_os = "windows")]
pub fn acquire() -> Result<Option<InstanceGuard>> {
    use crate::APP_ID;
    use anyhow::Context;
    use std::ptr;
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    let name: Vec<u16> = format!("Local\\{APP_ID}-single-instance")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error()).context("failed creating instance mutex");
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(handle);
        }
        return Ok(None);
    }
    Ok(Some(InstanceGuard(handle)))
}

#[cfg(not(target_os = "windows"))]
pub fn acquire() -> Result<Option<InstanceGuard>> {
    Ok(Some(InstanceGuard))
}
