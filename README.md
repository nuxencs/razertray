<h1 align="center">razertray</h1>

<p align="center">Your wireless Razer mouse's battery level in the Windows tray, without Razer Synapse.</p>

## Documentation

- [Installation](docs/installation.md): download, first start, start at login,
  updates, and uninstall.
- [Using the tray](docs/using-the-tray.md): the icon colors, the menu, and how
  often razertray checks the battery.
- [Low-battery notifications](docs/notifications.md): when alerts appear and how
  to change them.
- [Configuration](docs/configuration.md): every setting in `config.toml`, and the
  other files razertray keeps.
- [Supported devices](docs/supported-devices.md): which mice razertray knows, and
  what happens with other Razer devices.
- [Troubleshooting](docs/troubleshooting.md): no device found, no notifications,
  and where to find the log.
- [Development](docs/development.md): build from source, checks, and the device
  list generator.

## Features

- **Battery in the tray**: a small battery icon fills up and changes color:
  green, orange at 35% or less, red at 15% or less, and blue while charging.
- **Percentage as text**: show the exact number as colored digits instead of the
  battery.
- **Low-battery alerts**: a Windows notification when a mouse drops to 15% or
  less, repeated at most every two hours per device.
- **Several devices**: see every connected Razer device in the menu and pick the
  one the icon tracks.
- **Known by name**: 66 Razer mice and wireless receivers, from the
  [OpenRazer](https://openrazer.github.io/) device list. Newer mice still get a
  reading under their system name.
- **Quiet and small**: one portable `.exe`, no installer, no background service,
  and no Razer software.
- **Start at login**: turn it on from the tray menu.

## Quick start

razertray needs Windows 10 or 11 and a Razer mouse that is connected by cable or
through its wireless receiver.

1. Download `razertray.exe` from the
   [latest release](https://github.com/nuxencs/razertray/releases/latest).
2. Move it to a folder where it can stay, for example
   `%LOCALAPPDATA%\Programs\razertray`.
3. Double-click it. razertray has no window. A battery icon appears in the
   notification area next to the clock. If you cannot see it, open the hidden
   icons with the arrow (**^**) next to the clock.
4. Hover over the icon to see the mouse name and the battery percentage.
   Right-click it for the menu.
5. Optional: select **Start at login** in the menu, so razertray starts each
   time you sign in to Windows.

The first reading can take a few seconds. If the icon shows a gray **x**, see
[Troubleshooting](docs/troubleshooting.md#the-icon-shows-a-gray-x).

## How it reads the battery

razertray asks the mouse for its battery level directly over USB, with the same
commands that the OpenRazer Linux driver uses. It checks once a minute and keeps
everything on your computer: it has no account, no network access, and no
telemetry.

## Support

- [GitHub Issues](https://github.com/nuxencs/razertray/issues): bug reports and
  feature requests. Please attach the log file; see
  [Troubleshooting](docs/troubleshooting.md#find-the-log-file).

## License

GPL-2.0-or-later. See [LICENSE](LICENSE).

The device list and the Razer protocol code are derived from
[OpenRazer](https://github.com/openrazer/openrazer), which uses the same
license. razertray is not affiliated with or endorsed by Razer Inc.
