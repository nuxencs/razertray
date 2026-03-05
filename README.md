# razertray

Rust Windows tray app that polls battery percentage for connected Razer devices and shows status in the system tray.

## Features

- Single-process tray app
- Battery + charging polling via HID feature reports (Razer protocol)
- Device support map generated from local OpenRazer source
- Unknown PID fallback probe (`0x1F -> 0x3F -> 0xFF`) with cache
- Low-battery toasts (cooldown + dedup)
- Autostart toggle via `HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run`
- Windows notification details: [`docs/notifications.md`](docs/notifications.md)

## Config

Config file: `%APPDATA%\\razertray\\config.toml`

```toml
poll_interval_seconds = 60
low_battery_threshold = 15
low_battery_cooldown_minutes = 120
selected_device_id = ""
autostart = true
log_level = "info"
```

PID cache file: `%APPDATA%\\razertray\\pid_cache.toml`

Log files:
- `%APPDATA%\\razertray\\razertray.log` (current)
- `%APPDATA%\\razertray\\razertray.log.1` (previous)
- `%APPDATA%\\razertray\\razertray.log.2` (older)

Rotation policy: when `razertray.log` reaches ~1 MiB, it rotates and keeps 3 files total.

## CLI

- `razertray.exe` - tray mode
- `razertray.exe --once` - single poll run to stdout

## Build

On Windows (MSVC toolchain + Visual Studio Build Tools installed):

```bash
cargo build --release --target x86_64-pc-windows-msvc
```

Output:

```text
target/x86_64-pc-windows-msvc/release/razertray.exe
```

## Regenerate Device Map

```bash
tools/extract_openrazer_map.py ~/dev/openrazer src/device_map.rs
```

## CI

### Tag Release

- Workflow: `.github/workflows/release-on-tag.yml`
- Trigger: push tag matching `v*.*.*`
- Guard: tag must equal `v` + package version in `Cargo.toml`
- Output assets:
  - `razertray.exe`
  - `SHA256SUMS.txt`

### Device-ID Sync

- Workflow: `.github/workflows/update-device-map.yml`
- Trigger:
  - Weekly schedule: `0 8 * * 1` (08:00 UTC)
  - Manual dispatch
- Source: `openrazer/openrazer` `master` branch
- Behavior:
  - Regenerate `src/device_map.rs`
  - If no map diff: exit cleanly, no PR
  - If map diff: bump patch version in `Cargo.toml` and root package in `Cargo.lock`
  - Run `cargo test --all-targets`
  - Push/update branch `ci/device-map-sync`
  - Create/update PR

Berlin local-time note: cron is evaluated in UTC, so this runs around 09:00 CET in winter and 10:00 CEST in summer.

### Release On Device-ID PR Merge

- Workflow: `.github/workflows/release-device-map-merge.yml`
- Trigger: merged PR from `ci/device-map-sync` into default branch
- Behavior:
  - Read package version and compute tag `vX.Y.Z`
  - Fail if tag or release already exists
  - Create/push tag
  - Build and publish release assets (`razertray.exe`, `SHA256SUMS.txt`)
