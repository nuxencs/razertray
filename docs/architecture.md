# Architecture

razertray uses a small event-driven core. Platform and HID details stay at the
edges.

## State flow

1. The polling worker emits a poll ID and starts a bounded HID scan.
2. The HID client returns typed readings and typed errors.
3. `AppCore` accepts only the active poll ID, updates observations, and projects
   one complete `TrayView`.
4. The tray applies that view as an idempotent projection.
5. Side effects use a small command set for configuration writes and
   notifications.

This split keeps the core testable without a Windows tray or physical hardware.

## State truth rules

- Poll activity and device observation are separate states.
- A successful poll controls which readings are current.
- An old reading can remain visible only as stale data.
- Current poll diagnostics remain visible with fresh or stale readings.
- The preferred device changes only after an explicit user selection.
- A readable fallback does not replace the saved preference.
- A failed configuration write restores the prior in-memory setting.
- Windows startup state comes from the Windows Run registry key.

## HID boundary

The scanner groups candidate HID interfaces for each logical device. The client
tries candidates in a deterministic order. It tries a cached transaction ID,
known device metadata, and bounded protocol fallbacks. Failed cached IDs are
removed. Battery and charging queries aggregate relevant candidate results
before declaring a state unsupported. Transport and protocol failures can retry
within a per-device time budget. If the bounded candidate set is truncated,
unsupported evidence remains partial. The interface that supplied the battery
reading is tried first for charging status, followed by bounded fallbacks.

The private transport seam supports retry tests without exposing HID mechanics
to the rest of the app.

## Diagnostics

`PollResult` is the structured diagnostic boundary. It preserves raw battery
values, charge-state uncertainty, typed error scopes and kinds, and readable
devices in the same result. Independent transport failures remain separate
diagnostics. JSON output uses this type directly.

The tray shows a bounded diagnostic summary. Logs and CLI or JSON output retain
the complete typed details.

Exact repeated errors are throttled in logs without collapsing distinct probe
details. A later successful poll records recovery.

## Forecast

Forecasting is private application state. It consumes successful, timestamped
readings only. It resets after charging, large gaps, or implausible upward
changes. An expired estimate restarts calibration instead of remaining at zero.
Each projected estimate carries its timestamp. The tray refreshes at the next
display boundary and removes an expired estimate between hardware polls.
The tray and notification commands receive the current estimate. Forecast
samples and discharge-rate state remain private to `AppCore`.
