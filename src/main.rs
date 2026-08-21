#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use razertray::app::{OnceOutput, OnceStatus};
use std::process::ExitCode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Tray,
    Once(OnceOutput),
    HidWorker,
    Help,
    Version,
}

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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = match parse_mode(&args) {
        Ok(mode) => mode,
        Err(message) => {
            attach_console_if_windows();
            eprintln!("{message}");
            print_help();
            return ExitCode::from(2);
        }
    };

    match mode {
        Mode::Help => {
            attach_console_if_windows();
            print_help();
            return ExitCode::SUCCESS;
        }
        Mode::Version => {
            attach_console_if_windows();
            println!("razertray {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Mode::Once(output) => {
            attach_console_if_windows();
            return match razertray::app::run_once(output) {
                Ok(OnceStatus::Success) => ExitCode::SUCCESS,
                Ok(OnceStatus::NoDevice) => ExitCode::from(1),
                Ok(OnceStatus::PartialFailure) => ExitCode::from(2),
                Err(err) => {
                    eprintln!("razertray: {err:#}");
                    ExitCode::from(2)
                }
            };
        }
        Mode::HidWorker => {
            return match razertray::app::run_hid_worker() {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("razertray HID worker: {error:#}");
                    ExitCode::from(2)
                }
            };
        }
        Mode::Tray => {}
    }

    match razertray::app::run_tray() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("razertray: {err:#}");
            ExitCode::from(2)
        }
    }
}

fn parse_mode(args: &[String]) -> Result<Mode, String> {
    match args {
        [] => Ok(Mode::Tray),
        [arg] if arg == "--once" => Ok(Mode::Once(OnceOutput::Human)),
        [arg] if arg == "--json" => Ok(Mode::Once(OnceOutput::Json)),
        [arg] if arg == "--diagnose" => Ok(Mode::Once(OnceOutput::Diagnose)),
        [arg] if arg == "--hid-worker" => Ok(Mode::HidWorker),
        [arg] if arg == "--help" || arg == "-h" => Ok(Mode::Help),
        [arg] if arg == "--version" || arg == "-V" => Ok(Mode::Version),
        [arg] => Err(format!("Unknown argument: {arg}")),
        _ => Err("Only one command-line option can be used at a time.".to_string()),
    }
}

fn attach_console_if_windows() {
    #[cfg(windows)]
    attach_parent_console();
}

fn print_help() {
    println!("razertray {}", env!("CARGO_PKG_VERSION"));
    println!("Show Razer battery status in the Windows system tray.");
    println!();
    println!("Usage: razertray [--once | --json | --diagnose | --help | --version]");
}

#[cfg(test)]
mod tests {
    use super::{Mode, parse_mode};
    use razertray::app::OnceOutput;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn command_line_accepts_one_mode_only() {
        assert_eq!(parse_mode(&[]).unwrap(), Mode::Tray);
        assert_eq!(
            parse_mode(&args(&["--json"])).unwrap(),
            Mode::Once(OnceOutput::Json)
        );
        assert!(parse_mode(&args(&["--json", "--once"])).is_err());
        assert!(parse_mode(&args(&["--unknown"])).is_err());
    }

    #[test]
    fn review_round_26_hidden_hid_worker_mode_is_recognized() {
        assert_eq!(
            parse_mode(&args(&["--hid-worker"])).unwrap(),
            Mode::HidWorker
        );
    }
}
