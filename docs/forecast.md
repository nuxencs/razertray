# Battery forecast & logging

## What it does

For each watched device razertray estimates how long the battery will last and
surfaces it as a runout time (e.g. `~25h left`). It also writes a battery line to
the log whenever a reading changes, so the log finally shows useful signal
instead of only errors.

The forecast is **pure post-processing** of the readings the poll loop already
collects — it never adds a device query, so it has **no effect on battery life**.

## How the estimate works

- **Raw resolution.** The device reports battery as a raw `0..=255` value
  (`hid::client`); the user-facing percentage is that value floored to whole
  percent. The forecast keeps the *raw* value for its math (~0.4% per step), so a
  60-second sample isn't lost to integer quantization.
- **Anchored discharge rate.** At the start of each discharge segment we record an
  *anchor* `(time, raw)`. The rate is the average drop from that anchor to the
  latest reading: `rate = (anchor_raw − now_raw) / elapsed`. Averaging over the
  whole segment is stable even though a wireless mouse sleeps a lot and samples
  arrive sparsely and irregularly — a sliding-window fit would be jumpy on that
  data.
- **Runout.** `time_to_empty = now_raw / rate`, shown via `Forecast`.

### Segment resets

The current discharge segment restarts (so a stale rate isn't reused) when:

- the device reports **charging** — the forecast shows `charging` and the anchor
  is cleared; a fresh segment starts on the next discharging reading;
- the level **rises above the segment's lowest point** by more than a small
  epsilon while not flagged charging — a top-up, cell swap, or a charge on a
  device that doesn't report charging status. Comparing against the running
  minimum (not the segment start) means a recharge is detected even when it
  climbs gradually or never reaches the level the segment began at; the forecast
  drops back to `estimating…` until the new segment warms up.

> ~10 supported devices don't report charging status, so `is_charging` is always
> false for them. The running-minimum reset is what keeps those from showing a
> stale "discharging" runout while they're actually on the charger.

### Warm-up

Until a segment is at least ~15 minutes old **and** has dropped ≥5 raw steps
(~2%), the forecast reports `estimating…` rather than a wild number from a single
step. History is in-memory only, so after a restart the forecast re-warms over a
few minutes.

## Where it shows up

- **Tray tooltip / status item** — appends `· ~25h left` while discharging.
- **Log line** — on each change (whole-percent change or charging flip):

  ```
  Razer Deathadder V4 Pro Wireless: 78% (-2% in 47m, ~25h left @ 3.1%/h)
  ```

  The `(-2% in 47m)` part is the drop since the previous reading — i.e. how much
  power was lost since the last check.
- **Low-battery toast** — the body leads with the runout estimate when available
  (`~1h 30m left — plug in soon`), see [notifications.md](notifications.md).

> Runout is shown as a duration rather than an absolute clock time
> (`empty ≈ 16:40`); the log line already carries an absolute timestamp. An
> absolute ETA would need a date/time dependency, deliberately avoided for now.

## Poll-error logging

A wireless mouse that sleeps fails every poll while asleep (`send_feature_report
failed`, `device stayed busy/unresponsive after retries`). These are benign and
used to be logged at `WARN` on every cycle, flooding the log. Now they are
throttled:

- **First failure** logs once at `WARN`. Benign sleep failures are marked
  `device unreachable (asleep?); suppressing repeats`; anything else is a plain
  `WARN` so genuine faults stay visible.
- **Repeats** are suppressed; a summary is logged at most every ~15 minutes
  (`still failing: … (N poll(s) over Xh)`). Suppression is keyed on the failure
  *category* (benign-sleep vs genuine fault), not the exact wording — a sleeping
  device fails at different transport stages from poll to poll, and all of those
  count as the same ongoing episode.
- A **change of category** (benign sleep ⇄ genuine fault) is surfaced
  immediately and starts a new episode.
- **Recovery** logs once when the device reads successfully again
  (`… recovered after N failed poll(s) over Xm`).

Both the error and success paths refer to a device by the same resolved name
(the curated device-map name when known), so the log is consistent.
