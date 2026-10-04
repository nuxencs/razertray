# Supported devices

razertray reads every connected Razer device that reports a battery level. It is
made and tested for Razer wireless mice.

## Known devices

razertray knows 66 Razer mice and wireless receivers by name, for example
models from the Basilisk, DeathAdder, Viper, Naga, Mamba, Lancehead, Orochi,
Cobra and Pro Click families. Many wireless mice have two entries: one for the
mouse connected by cable, and one for its wireless receiver.

The list comes from the [OpenRazer](https://openrazer.github.io/) project, the
Linux driver for Razer devices. A weekly job compares it with OpenRazer, and new
devices arrive with the next razertray release. The full list is in
[`src/device_map.rs`](../src/device_map.rs). Search it for your model, or for the
`pid` that `razertray.exe --once` shows.

## Devices that are not in the list

razertray also tries every other Razer device that it finds, for example a
newer mouse or some wireless keyboards. If the device answers like a Razer
mouse, it appears under the name that Windows reports for it.

To talk to an unknown device, razertray tries the possible settings one after
the other, and remembers the one that works in
[`pid_cache.toml`](configuration.md#other-files-in-the-folder). The first
reading of an unknown device can therefore take a few seconds longer.

Devices that do not report a battery level, such as most wired keyboards and
mouse pads, do not appear in the tray. `razertray.exe --once` lists them under
`Errors:`, and the log file shows a warning for them at each check.

## Charging status

Most wireless mice report if they are charging, and the icon turns blue while
they charge. Some mice cannot report it, for example the Hyperspeed models that
use AA batteries. razertray always shows these mice as not charging, and the
icon never turns blue.

For unknown devices, razertray asks for the charging status. If the device does
not answer, razertray shows it as not charging.

## Limits

- The mouse, or its wireless receiver, must be connected and awake. A mouse that
  is off or in deep sleep has no reading.
- Bluetooth connections are not tested.
- The level comes directly from the device. Newer models that are not in the
  list can report a level that is off by a few percent, or no level at all.
