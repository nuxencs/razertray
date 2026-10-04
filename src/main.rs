#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use anyhow::Result;

/// Re-attach stdout/stderr to the launching terminal's console (if any) so the
/// CLI mode (`--once`) keeps printing to the terminal despite the "windows"
/// subsystem. No-op when launched without a parent console (e.g. from Explorer
/// or autostart).
#[cfg(windows)]
fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};

    // SAFETY: AttachConsole takes a plain process ID and no pointers. Failure
    // (no parent console) only returns FALSE, which needs no handling here.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() >= 2 && args[1] == "--once" {
        #[cfg(windows)]
        attach_parent_console();

        return razertray::run_once();
    }

    razertray::run_tray()
}
