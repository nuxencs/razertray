# Configuration

razertray keeps its settings in `%APPDATA%\razertray\config.toml`. It creates
this file with the default values on the first start. To open the folder, enter
`%APPDATA%\razertray` in the address bar of File Explorer.

You set most settings from the [tray menu](using-the-tray.md#the-menu). Edit the
file only for the settings that the menu does not have.

## Edit the file

1. Right-click the tray icon and select **Exit**.
2. Open `config.toml` in a text editor, for example Notepad.
3. Change the values and save the file.
4. Start razertray again.

Close razertray first. razertray reads the file only when it starts, and it
writes the file again when you use the menu. Changes that you save while it runs
have no effect and can be overwritten.

## Settings

This is the default file:

```toml
poll_interval_seconds = 60
low_battery_threshold = 15
low_battery_cooldown_minutes = 120
selected_device_id = ""
autostart = false
log_level = "info"
view_mode = "icon"
```

| Setting | Default | Accepted values | Meaning |
|---|---|---|---|
| `poll_interval_seconds` | `60` | Whole seconds. Values below `5` count as `5`. | How often razertray reads the battery while a device answers. See [How often razertray checks](using-the-tray.md#how-often-razertray-checks). |
| `low_battery_threshold` | `15` | `0` to `100` | Show a [notification](notifications.md) at or below this percentage. `0` turns them off for every level above 0%. |
| `low_battery_cooldown_minutes` | `120` | Whole minutes | Minimum time between two notifications for the same device. |
| `selected_device_id` | `""` | Set from the menu | The device that the tray icon shows. `""` means none. Use **Select Device** in the menu to change it. |
| `autostart` | `false` | `true`, `false` | Start razertray when you sign in. Use **Start at login** in the menu to change it. |
| `log_level` | `"info"` | `"error"`, `"warn"`, `"info"`, `"debug"`, `"trace"` | How much razertray writes to the [log file](troubleshooting.md#find-the-log-file). Use `"debug"` when you report a problem. |
| `view_mode` | `"icon"` | `"icon"`, `"text"` | Show the battery, or the percentage as digits. Use **Show percentage as text** in the menu to change it. |

## If the file has an error

If razertray cannot read `config.toml`, for example because of a typo, it still
starts. It then:

1. Renames your file to `config.toml.invalid`.
2. Creates a new `config.toml` with the default values.
3. Writes the reason, with the line number, to the
   [log file](troubleshooting.md#find-the-log-file).

To get your settings back, exit razertray, fix the error in
`config.toml.invalid`, and rename it to `config.toml`.

An invalid `log_level` does not reset the file. razertray then uses `"info"` and
writes a warning to the log.

## Other files in the folder

| File | Contents |
|---|---|
| `pid_cache.toml` | How to talk to devices that are not in the built-in list. razertray fills it in automatically. You can delete it; razertray finds the values again. |
| `razertray.log` | The log file. razertray starts a new file at about 1 MiB and keeps the two previous files as `razertray.log.1` and `razertray.log.2`. |
| `*.invalid` | A file that razertray could not read and replaced. See [If the file has an error](#if-the-file-has-an-error). |
