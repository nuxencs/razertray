# Low-battery notifications

razertray shows a Windows notification when a device gets low, so you can charge
it before it turns off.

## When a notification appears

razertray checks each connected device after every battery reading. It shows a
notification when all of these are true:

- The level is at or below `low_battery_threshold` (15% by default).
- The device is not charging.
- razertray did not already show a notification for this device in the last
  `low_battery_cooldown_minutes` (120 minutes by default).

This applies to every connected device, not only the device in the tray icon.
Each device has its own cooldown. When you restart razertray, the cooldowns
start again, so a low device can notify again right after the start.

The notification looks like this:

> **Razer Viper Ultimate Wireless: 12%**
> Battery low
> Plug in charger soon

## Change the level or the repeat interval

Set `low_battery_threshold` and `low_battery_cooldown_minutes` in
[`config.toml`](configuration.md). For example, to get a notification at 25% and
at most once an hour:

```toml
low_battery_threshold = 25
low_battery_cooldown_minutes = 60
```

To turn the notifications off in razertray, set `low_battery_threshold = 0`.
This turns them off for all levels above 0%.

## Notification settings in Windows

The notifications use the name **razertray**. You can change or turn them off in
**Settings** > **System** > **Notifications**, like the notifications of any
other app. Do not disturb (Windows 11) and Focus assist (Windows 10) hide them
too.

To show its name as the sender, razertray registers itself once in the registry,
under `HKEY_CURRENT_USER\Software\Classes\AppUserModelId\razertray`. If that
fails, Windows shows the notifications as coming from **Windows PowerShell**
instead. The notifications still work.
