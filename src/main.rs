#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use anyhow::Result;

/// Re-attach stdout/stderr to the launching terminal's console (if any) so the
/// CLI mode (`--once`) keeps printing to the terminal despite the "windows"
/// subsystem. No-op when launched without a parent console (e.g. from Explorer
/// or autostart).
#[cfg(windows)]
fn attach_parent_console() {
    // kernel32 is always linked on Windows; declare the one call we need.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AttachConsole(dw_process_id: u32) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF; // (DWORD)-1
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() >= 2 && args[1] == "--once" {
        #[cfg(windows)]
        attach_parent_console();

        return razertray::app::run_once();
    }

    razertray::app::run_tray()
}
