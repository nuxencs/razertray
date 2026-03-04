#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use anyhow::Result;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() >= 2 && args[1] == "--once" {
        return razertray::app::run_once();
    }

    razertray::app::run_tray()
}
