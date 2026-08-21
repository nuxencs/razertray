# razertray

razertray is a focused Windows tray utility for Razer wireless-device battery
status. It reads HID feature reports directly. Razer Synapse is not required.

## Features

- Battery percentage and charge state in the Windows system tray
- Battery-icon and percentage-text display modes
- Explicit checking, current, stale, unavailable, and no-device states
- Stable preferred-device selection with automatic display fallback
- Low-battery notifications for the displayed device or all devices
- Discharge-based time-remaining forecast after enough observations
- Tray settings for display mode, alert scope, threshold, check interval, and
  Windows startup
- Manual refresh and direct access to the app data folder
- Automatic recovery from an invalid configuration file
- HID interface fallback, transaction-ID probing, bounded retries, and cache
  invalidation
- Human-readable, JSON, and diagnostic command-line output

## Install and run

1. Download `razertray.exe` from the [latest release](../../releases/latest).
2. Run the executable.
3. Right-click the tray icon to view status or change settings.

The app does not enable Windows startup by default. Use **Preferences > Start
at login** if you want it.

Only one tray instance runs for each Windows user session.

## Tray states

The icon and status line distinguish these states:

- **Checking**: a poll is active. Manual refresh is disabled.
- **Current**: the displayed reading came from the latest successful poll.
- **Stale**: the last reading is retained, but the device is not currently
  readable. The status includes the age of the reading.
- **Unavailable**: polling failed and no prior reading can be shown.
- **No device**: polling succeeded but found no battery-capable Razer device.

If the preferred device is temporarily unavailable, razertray shows another
readable device. It does not change the saved preference. The preferred device
returns automatically when it becomes readable again.

## Preferences

Open the **Preferences** submenu to change:

- percentage text or battery icon
- alerts for the displayed device or all devices
- low-battery threshold
- check interval
- Windows startup

Changes are saved immediately. If a configuration write fails, the setting is
restored in the menu and Windows shows an error notification.

Advanced settings remain available in
`%APPDATA%\razertray\config.toml`:

```toml
poll_interval_seconds = 60
low_battery_threshold = 15
low_battery_cooldown_minutes = 120
selected_device_id = ""
log_level = "info"
view_mode = "icon"              # "icon" or "text"
alert_scope = "selected"        # "selected" or "all"
welcome_shown = true
```

Unsafe numeric values are clamped to supported limits. If the file cannot be
parsed, razertray preserves it as `config.invalid.<time>.<pid>.toml` and creates
a valid default file.

Other files in the app data folder:

- `pid_cache.toml`: working transaction IDs found during device probing
- `razertray.log`, `.1`, and `.2`: bounded diagnostic logs

## Battery forecast

The tooltip and status line can show an estimate such as `~9 h left`. The
forecast needs at least 30 minutes of uninterrupted discharge data and a
meaningful battery drop. Charging, a long observation gap, or an upward battery
jump starts a new sample window. No estimate is shown until the data is useful.

The forecast is an estimate, not a battery-health measurement. Device firmware
controls the precision of the source reading.

## Command line

```text
razertray.exe --once
razertray.exe --json
razertray.exe --diagnose
```

- `--once` prints one readable snapshot.
- `--json` prints the complete structured poll result.
- `--diagnose` also prints the device key, raw battery value, cached transaction
  ID, and typed error scope and kind.

Exit codes:

- `0`: one or more devices were read and no polling error occurred
- `1`: no readable battery device was found
- `2`: a partial or fatal failure occurred, or an argument was invalid

## Low-battery alerts

The default threshold is 15 percent. Alerts stop while the device reports that
it is charging. Each device has its own cooldown. A time-remaining estimate is
included when one is available. See [notification details](docs/notifications.md).

## Supported devices

razertray polls Razer HID devices that expose the battery protocol used by
OpenRazer. It is designed and tested for wireless mice. Some compatible Razer
keyboards can also appear. Known products use names and protocol details from
the community [OpenRazer](https://openrazer.github.io/) device database.

Unknown Razer products are probed with a bounded set of interfaces and
transaction IDs. Unsupported devices are excluded from readable-device entries
and retained as typed diagnostics in JSON, diagnostic output, and tray status.

## Limitations

- The tray app and notifications require Windows.
- Sleeping, powered-off, or disconnected hardware can produce a stale reading.
- Some devices do not report charge state.
- Forecast history is held in memory and restarts with the app.
- Multiple identical devices without serial numbers can be difficult to
  distinguish because the HID metadata does not always expose a stable identity.

## Development

The pinned Rust toolchain and Windows target are defined in
`rust-toolchain.toml`.

```bash
cargo fmt --check
cargo clippy --all-targets --target x86_64-pc-windows-msvc -- -D warnings
cargo test --all-targets
cargo build --release --target x86_64-pc-windows-msvc
```

Regenerate the device map from a local OpenRazer checkout:

```bash
tools/extract_openrazer_map.py ~/dev/openrazer src/device_map.rs
```

The main design seams are documented in [docs/architecture.md](docs/architecture.md).

## License

razertray uses the [GNU General Public License v2.0 or later](LICENSE). The
device map and Razer HID protocol work derive from GPL-licensed OpenRazer work.
