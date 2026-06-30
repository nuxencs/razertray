# Notifications

## Windows low-battery toast format

Low-battery notifications use this collapsed title format:

- `{device_name}: {battery_percent}%`

Toast body lines:

- `Battery low`
- `Plug in charger soon` — or, when a runout estimate is available,
  `~{time} left — plug in soon` (e.g. `~1h 30m left — plug in soon`). See
  [forecast.md](forecast.md).

## Sender identity (AUMID)

On Windows, the app registers an AppUserModelId (AUMID) under:

- `HKCU\\SOFTWARE\\Classes\\AppUserModelId\\razertray`

Values written:

- `DisplayName = razertray`
- `IconUri = <current executable path>` (best effort)

Notifications are sent with app id `razertray` when registration succeeds.

If AUMID registration fails, razertray falls back to `Toast::POWERSHELL_APP_ID` so the notification is still delivered.
