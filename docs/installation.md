# Installation

razertray is one portable `razertray.exe` file. It has no installer and changes
nothing on your computer until you start it.

## Requirements

- Windows 10 or Windows 11.
- A Razer mouse that reports its battery level, connected by cable or through its
  wireless receiver. See [Supported devices](supported-devices.md).

You do not need Razer Synapse or any other Razer software.

## Download

1. Open the [latest release](https://github.com/nuxencs/razertray/releases/latest).
2. Download `razertray.exe`. Optional: also download `SHA256SUMS.txt` to check
   the file.
3. Move `razertray.exe` to a folder where it can stay, for example
   `%LOCALAPPDATA%\Programs\razertray`. **Start at login** remembers this
   location, so do not start it from a folder that you clean up often, such as
   Downloads.

### Check the download (optional)

In PowerShell, in the folder with both files:

```powershell
Get-FileHash .\razertray.exe -Algorithm SHA256
Get-Content .\SHA256SUMS.txt
```

The two hash values must be the same (PowerShell shows the hash in uppercase,
the file in lowercase). If they are different, do not run the file. Download it
again.

## First start

Double-click `razertray.exe`.

The release file is not code-signed. For that reason, Windows SmartScreen can
show **Windows protected your PC**. To start razertray, select **More info**,
then **Run anyway**.

razertray has no window. A battery icon appears in the notification area next
to the clock. If you cannot see it, select the arrow (**^**) next to the clock.
To keep the icon visible, drag it from there to the taskbar.

On the first start, razertray creates its settings folder,
`%APPDATA%\razertray`. See [Configuration](configuration.md).

## Start at login

Right-click the tray icon and select **Start at login**. A check mark shows that
it is on. Select it again to turn it off. It is off by default.

When it is on, razertray adds a `razertray` entry to the current user's Windows
startup list. You can also see and turn off this entry in **Settings** >
**Apps** > **Startup**.

## Update

1. Right-click the tray icon and select **Exit**.
2. Replace `razertray.exe` with the new version, in the same folder.
3. Start it again.

Your settings stay in `%APPDATA%\razertray`. If you move `razertray.exe` to a
different folder, start it once from the new folder. razertray then updates
**Start at login** to the new location.

## Uninstall

1. Right-click the tray icon. If **Start at login** has a check mark, select it
   to turn it off.
2. Select **Exit**.
3. Delete `razertray.exe`.
4. Optional: delete the `%APPDATA%\razertray` folder. It holds your settings and
   the log file.

razertray also registers itself as a notification sender the first time it
shows a notification. To remove that entry too, delete the registry key
`HKEY_CURRENT_USER\Software\Classes\AppUserModelId\razertray`. It is harmless
if you keep it.

## Command-line check

To check the battery once without the tray, run this in a terminal, in the
folder with `razertray.exe`:

```text
razertray.exe --once
```

Example output:

```text
Razer Viper Ultimate Wireless pid=0x007B battery=76% not-charging
```

`pid` is the USB product ID of the device. If no device answers, the output is
`No supported Razer devices found.` Devices that are found but do not answer are
listed under `Errors:`.

Because razertray is a tray app, the terminal prompt can return before these
lines appear. The lines still appear after a moment.
