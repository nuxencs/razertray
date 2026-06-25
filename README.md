# razertray

A tiny Windows tray app that shows your wireless Razer mouse's battery level
and warns you before it dies — no Razer Synapse required.

## What it does

razertray sits quietly next to your clock in the Windows system tray. It shows
a small battery icon for your wireless Razer mouse, checks the charge level
about once a minute, and pops up a notification when the battery gets low so you
can plug in before it runs out. If you have more than one Razer device, you can
pick which one the icon tracks.

## Features

- Live battery level for your wireless Razer mouse, right in the system tray
- Color-coded battery icon: green when healthy, orange when getting low, red
  when nearly empty, and blue while charging
- Hover the icon for a tooltip with the device name, exact percentage, and
  whether it's charging
- Automatic low-battery alerts so you get a heads-up before the mouse dies
- Pick which Razer device to watch when several are connected
- "Refresh now" for an instant reading instead of waiting for the next check
- Optionally starts with Windows so it's always running
- Lightweight and quiet — just a tray icon, no heavy background software
- Knows a wide range of Razer mice by name, and still shows a reading (under a
  generic name) for newer ones it hasn't learned yet

## Install & run

1. Download `razertray.exe` from the [latest release](../../releases/latest) —
   there's no installer.
2. Run it. It starts in the system tray with no main window and keeps working
   in the background.
3. By default it sets itself to start automatically when you log in to Windows.
   You can turn this off any time from the tray menu.

Quick check from a terminal (works without opening the tray):

```text
razertray.exe --once
```

This prints the battery level of every detected Razer device once and exits —
handy for confirming your mouse is picked up.

## In the tray

Right-click the icon for the menu:

- **Status line** — the watched device, its battery percentage, and `(charging)`
  if it's plugged in (or "No supported Razer devices" if none are found)
- **Select Device** — lists every Razer device found, each with its name,
  battery percentage and charge state, with a checkmark on the one being tracked
- **Refresh now** — check the battery immediately
- **Start at login** — toggle starting automatically with Windows
- **Exit** — close the app and remove the icon

The icon itself is a small battery that fills up and changes color:
**blue** while charging, **red** at 15% or below, **orange** up to 35%, and
**green** above that. A device with no reading yet shows a faded mark.

## Low-battery alerts

When the watched mouse drops to or below the low-battery level (15% by default)
and isn't charging, Windows shows a notification — for example
`Razer Viper Ultimate: 12%`, with "Battery low / Plug in charger soon". To avoid
nagging, it only repeats an alert for the same device every couple of hours, and
alerts stop once you plug in. Each device is tracked separately.

## Supported devices

razertray looks at every Razer-branded device you have connected and shows the
ones that report a battery level — in practice that's **wireless mice**, which is
what it's built and tested for. It ships with a built-in list of around 65 known
Razer mice and wireless receivers (Mamba, Viper, DeathAdder, Naga, Basilisk,
Cobra, Pro Click, Orochi, Lancehead and more), shown by their proper product
names.

That list is generated from the community [OpenRazer](https://openrazer.github.io/)
project's device database and refreshed in new releases, so newer mice get added
over time. A device that isn't on the list — a newer mouse, or another Razer
device that reports its battery the same way (some wireless keyboards do) — still
gets a reading where possible, shown under its system-reported name. Gear that
doesn't report a battery this way is simply ignored.

## Settings

Settings live in a plain text file at
`%APPDATA%\razertray\config.toml`, created on first run. The device you pick and
the "Start at login" choice are saved here automatically.

```toml
poll_interval_seconds = 60        # how often to check the battery (minimum 5)
low_battery_threshold = 15        # warn at or below this percentage
low_battery_cooldown_minutes = 120  # minimum gap between repeat warnings
selected_device_id = ""           # which device to watch (set from the menu)
autostart = false                 # start with Windows (toggle from the tray menu)
log_level = "info"                # detail level for the log file
```

Other files in the same `%APPDATA%\razertray\` folder:

- `pid_cache.toml` — remembers how to talk to devices it had to probe
- `razertray.log` (plus `.1`, `.2`) — a rolling log for troubleshooting; rotates
  at about 1 MiB and keeps three files

## Limitations

- **Windows only** for the tray app and notifications. (`--once` runs elsewhere
  for a quick reading, but the tray mode does not.)
- **Curated and tested for Razer wireless mice.** It polls every connected Razer
  device for a battery level, so some other battery-reporting gear (e.g. certain
  wireless keyboards) may also appear under its system-reported name — but only
  mice are recognized by name and verified; non-battery devices are ignored.
- The mouse (or its wireless dongle) must be connected and awake; a sleeping or
  powered-off mouse may show no reading.
- Battery and charging readings come straight from the device and can
  occasionally be unavailable or off by a percent, especially for newer mice not
  yet in the built-in list (some devices don't report charging status at all).
- There's no settings window — advanced options are changed by editing
  `config.toml`.

---

## For developers

Build on Windows (MSVC toolchain + Visual Studio Build Tools):

```bash
cargo build --release --target x86_64-pc-windows-msvc
# -> target/x86_64-pc-windows-msvc/release/razertray.exe
```

The device support map (`src/device_map.rs`) is generated from a local OpenRazer
checkout:

```bash
tools/extract_openrazer_map.py ~/dev/openrazer src/device_map.rs
```

It reads battery info over HID feature reports using the Razer protocol,
mirroring OpenRazer's mouse driver (`razermouse_driver.c`): per-device
transaction IDs, the battery/charge commands, and which devices report charging
status.

CI keeps the device list in sync with OpenRazer and cuts releases automatically
— see the workflows in [`.github/workflows/`](.github/workflows/).

## License

razertray is licensed under the [GNU General Public License v2.0 or later](LICENSE)
(`GPL-2.0-or-later`).

The device support map (`src/device_map.rs`) and the Razer HID protocol code are
derived from [OpenRazer](https://github.com/openrazer/openrazer), which is itself
licensed under GPL-2.0-or-later. razertray matches that license out of respect for
the OpenRazer project's reverse-engineering work, which this app builds on.
