# Notifications

## Low-battery notification

Title:

- `{device_name}: {battery_percent}%`

Body:

- the time-remaining estimate, when available, or `Battery low`
- `Plug in the charger soon`

If charge state could not be read, the recovery text is `Check the charger
soon`. This avoids claiming that the device is not already charging.

Alerts apply to the displayed device by default. **Preferences > Alert for all
devices** enables independent alerts for each readable device. Each device has
its own cooldown.

## Information and error notifications

The first run explains how to open the tray menu. A valid configuration with
unsafe values shows **Configuration was adjusted**. An invalid file that was
preserved and replaced shows **Configuration was reset**. Settings failures
show a short action message. Adjustment notifications point to `config.toml`
because some adjusted values are not available in Preferences. Detailed errors
remain in the log file.

## Sender identity

On Windows, the app registers an AppUserModelId under:

- `HKCU\SOFTWARE\Classes\AppUserModelId\razertray`

Values:

- `DisplayName = razertray`
- `IconUri = <current executable path>` when available

If registration fails, razertray uses the PowerShell notification sender so the
notification can still be delivered.
