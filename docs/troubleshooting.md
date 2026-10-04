# Troubleshooting

Start with the symptom that you see. Each section lists checks in order, from
the most common cause.

## I cannot see the icon

1. Select the arrow (**^**) next to the clock. Windows often puts new tray icons
   there. Drag the icon to the taskbar to keep it visible.
2. Open Task Manager and look for `razertray.exe`. If it is not there, start
   `razertray.exe` again.

If you see two razertray icons, razertray runs twice. Select **Exit** on one of
them.

## The icon shows a gray x

The gray **x** means that razertray has no reading.

1. Move the mouse to wake it up, then right-click the icon and select
   **Refresh now**.
2. Make sure that the mouse is on, and that its wireless receiver or cable is
   connected.
3. Run `razertray.exe --once` in a terminal (see
   [Command-line check](installation.md#command-line-check)). It shows each device
   that razertray finds, and an error for each device that does not answer.
4. If another program that controls the mouse runs, for example Razer Synapse,
   exit it and select **Refresh now** again. Such programs can use the same
   connection as razertray.
5. Check that razertray supports your device. See
   [Supported devices](supported-devices.md).

If the mouse shows a reading sometimes but often has a gray **x**, it probably
sleeps between checks. razertray keeps the last reading through two missed
checks; see [How often razertray checks](using-the-tray.md#how-often-razertray-checks).

## The level does not change

razertray reads the battery once a minute. To read it now, select
**Refresh now**. If the mouse misses a check, razertray shows the last reading
for up to two more checks.

## No low-battery notification

1. Check the level: the default limit is 15% or less. See
   [When a notification appears](notifications.md#when-a-notification-appears).
2. A charging mouse does not cause a notification.
3. After a notification, razertray waits 120 minutes before the next one for the
   same device.
4. In Windows, open **Settings** > **System** > **Notifications**. Make sure that
   notifications are on, and that **razertray** (or **Windows PowerShell**) is
   allowed. Turn off Do not disturb or Focus assist to test.
5. Search the [log file](#find-the-log-file) for `toast`.

## Start at login does not work

1. Right-click the icon and check that **Start at login** has a check mark.
2. If you moved `razertray.exe`, start it once from its new folder. razertray then
   updates the startup entry.
3. Open **Settings** > **Apps** > **Startup**, and make sure that **razertray**
   is on there.

## My settings went back to the defaults

razertray could not read `config.toml`. It renamed the file to
`config.toml.invalid` and started with default settings. See
[If the file has an error](configuration.md#if-the-file-has-an-error) to get
your settings back.

## Find the log file

The log file is `%APPDATA%\razertray\razertray.log`. To open the folder, enter
`%APPDATA%\razertray` in the address bar of File Explorer. Older entries are in
`razertray.log.1` and `razertray.log.2`.

To get more detail for a bug report:

1. Exit razertray.
2. In `config.toml`, set `log_level = "debug"`.
3. Start razertray, and wait until the problem occurs again.
4. Attach `razertray.log` to a
   [GitHub issue](https://github.com/nuxencs/razertray/issues). Set `log_level`
   back to `"info"` when you are done.

The log contains device names and battery levels. razertray does not write your
user name or the folder paths on your computer to it.
