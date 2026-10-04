//! Draws the tray icon: a battery glyph, or the percentage as digits.

use crate::config::ViewMode;
use crate::model::BatteryState;
use anyhow::{Context, Result};
use tray_icon::Icon;

#[cfg(test)]
mod preview;

type Rgba = [u8; 4];

/// Edge length of the battery glyph. Windows shows tray icons at 16x16 px at 100% scale.
const BATTERY_SIZE: usize = 16;
/// Edge length of the digit icon. Larger than the tray slot so the
/// downscaled digits stay sharp.
const TEXT_SIZE: usize = 32;

/// At or below this level the icon is red. The README documents these colors.
const CRITICAL_PERCENT: u8 = 15;
/// At or below this level the icon is orange.
const LOW_PERCENT: u8 = 35;

const CHARGING_COLOR: Rgba = [90, 170, 255, 255];
const CRITICAL_COLOR: Rgba = [230, 70, 70, 255];
const LOW_COLOR: Rgba = [240, 170, 70, 255];
const OK_COLOR: Rgba = [90, 220, 120, 255];
const BORDER_COLOR: Rgba = [210, 210, 210, 255];
const UNKNOWN_COLOR: Rgba = [150, 150, 150, 255];
const OUTLINE_COLOR: Rgba = [25, 25, 25, 235];

/// Renders the icon for `device`, or a neutral battery when there is no reading.
pub(super) fn render(view: ViewMode, device: Option<&BatteryState>) -> Result<Icon> {
    let canvas = match (view, device) {
        (ViewMode::Text, Some(device)) => text_canvas(device),
        (_, device) => battery_canvas(device),
    };
    canvas.into_icon()
}

/// Square RGBA pixel buffer.
#[derive(Debug)]
struct Canvas {
    size: usize,
    pixels: Vec<u8>,
}

impl Canvas {
    fn new(size: usize) -> Self {
        Self {
            size,
            pixels: vec![0; size * size * 4],
        }
    }

    /// Sets one pixel. Points outside the canvas are ignored.
    fn put(&mut self, x: usize, y: usize, rgba: Rgba) {
        if x < self.size && y < self.size {
            let idx = (y * self.size + x) * 4;
            self.pixels[idx..idx + 4].copy_from_slice(&rgba);
        }
    }

    #[cfg(test)]
    fn count(&self, rgba: Rgba) -> usize {
        self.pixels.chunks_exact(4).filter(|px| *px == rgba).count()
    }

    fn into_icon(self) -> Result<Icon> {
        let size = u32::try_from(self.size).context("icon size does not fit u32")?;
        Icon::from_rgba(self.pixels, size, size).context("failed building tray icon")
    }
}

/// Color ramp shared by the battery fill and the digits.
fn level_color(device: &BatteryState) -> Rgba {
    match device.percent.get() {
        _ if device.charging => CHARGING_COLOR,
        p if p <= CRITICAL_PERCENT => CRITICAL_COLOR,
        p if p <= LOW_PERCENT => LOW_COLOR,
        _ => OK_COLOR,
    }
}

fn battery_canvas(device: Option<&BatteryState>) -> Canvas {
    let mut canvas = Canvas::new(BATTERY_SIZE);

    // Body border.
    for x in 2..14 {
        canvas.put(x, 4, BORDER_COLOR);
        canvas.put(x, 11, BORDER_COLOR);
    }
    for y in 4..12 {
        canvas.put(2, y, BORDER_COLOR);
        canvas.put(13, y, BORDER_COLOR);
    }
    // Cap.
    for (x, y) in [(14, 7), (15, 7), (14, 8), (15, 8)] {
        canvas.put(x, y, BORDER_COLOR);
    }

    match device {
        Some(device) => {
            // 10 columns inside the border; even 1% shows one column.
            let width = (usize::from(device.percent.get()) * 10)
                .div_ceil(100)
                .max(1);
            let color = level_color(device);
            for x in 3..3 + width {
                for y in 5..11 {
                    canvas.put(x, y, color);
                }
            }
        }
        None => {
            // An "x" mark: no reading.
            for i in 0..5 {
                canvas.put(5 + i, 6 + i, UNKNOWN_COLOR);
                canvas.put(9 - i, 6 + i, UNKNOWN_COLOR);
            }
        }
    }
    canvas
}

/// 3x5 pixel glyph for one digit. Bit 2 of each row is the left column.
fn digit_glyph(digit: u8) -> [u8; 5] {
    match digit {
        0 => [0b111, 0b101, 0b101, 0b101, 0b111],
        1 => [0b010, 0b110, 0b010, 0b010, 0b111],
        2 => [0b111, 0b001, 0b111, 0b100, 0b111],
        3 => [0b111, 0b001, 0b111, 0b001, 0b111],
        4 => [0b101, 0b101, 0b111, 0b001, 0b001],
        5 => [0b111, 0b100, 0b111, 0b001, 0b111],
        6 => [0b111, 0b100, 0b111, 0b101, 0b111],
        7 => [0b111, 0b001, 0b001, 0b001, 0b001],
        8 => [0b111, 0b101, 0b111, 0b101, 0b111],
        9 => [0b111, 0b101, 0b111, 0b001, 0b111],
        _ => [0; 5],
    }
}

/// The percentage as digits, centered, colored by level, with a dark
/// outline so it reads on light and dark taskbars.
fn text_canvas(device: &BatteryState) -> Canvas {
    let size = TEXT_SIZE;
    let digits: Vec<u8> = device
        .percent
        .get()
        .to_string()
        .bytes()
        .map(|b| b - b'0')
        .collect();

    // A glyph is 3 units wide with a 1 unit gap. Use the largest scale that
    // fits with 1 px padding on each side.
    let n = digits.len();
    let units_w = 4 * n - 1;
    let scale = ((size - 2) / units_w).min((size - 2) / 5);
    let x0 = (size - units_w * scale) / 2;
    let y0 = (size - 5 * scale) / 2;

    let mut mask = vec![false; size * size];
    for (i, &digit) in digits.iter().enumerate() {
        let gx = x0 + i * 4 * scale;
        for (row, bits) in digit_glyph(digit).into_iter().enumerate() {
            for col in (0..3).filter(|col| bits & (0b100 >> col) != 0) {
                for dy in 0..scale {
                    for dx in 0..scale {
                        mask[(y0 + row * scale + dy) * size + gx + col * scale + dx] = true;
                    }
                }
            }
        }
    }

    let mut canvas = Canvas::new(size);
    let color = level_color(device);
    for y in 0..size {
        for x in 0..size {
            if mask[y * size + x] {
                canvas.put(x, y, color);
            } else if touches_mask(&mask, size, x, y) {
                canvas.put(x, y, OUTLINE_COLOR);
            }
        }
    }
    canvas
}

/// True when any of the 8 neighbors of (x, y) is set.
fn touches_mask(mask: &[bool], size: usize, x: usize, y: usize) -> bool {
    let xs = x.saturating_sub(1)..=(x + 1).min(size - 1);
    (y.saturating_sub(1)..=(y + 1).min(size - 1))
        .any(|ny| xs.clone().any(|nx| mask[ny * size + nx]))
}

#[cfg(test)]
mod tests {
    use super::{
        CHARGING_COLOR, CRITICAL_COLOR, LOW_COLOR, OK_COLOR, OUTLINE_COLOR, UNKNOWN_COLOR,
        battery_canvas, text_canvas,
    };
    use crate::model::{BatteryState, DeviceKey, Percent};

    fn device(percent: u8, charging: bool) -> BatteryState {
        BatteryState {
            key: DeviceKey::new(1, None),
            name: "Mouse".to_owned(),
            pid: 1,
            percent: Percent::try_from(percent).expect("valid percent"),
            charging,
        }
    }

    #[test]
    fn battery_fill_grows_with_level() {
        let filled = |p| battery_canvas(Some(&device(p, true))).count(CHARGING_COLOR);
        let mut previous = 0;
        for p in 0..=100 {
            let now = filled(p);
            assert!(now >= previous, "fill shrank at {p}%");
            previous = now;
        }
        assert!(filled(0) > 0, "an empty battery still shows one column");
        assert!(filled(100) > filled(50));
    }

    #[test]
    fn colors_follow_the_documented_thresholds() {
        let fill = |p, charging| {
            let canvas = battery_canvas(Some(&device(p, charging)));
            [CHARGING_COLOR, CRITICAL_COLOR, LOW_COLOR, OK_COLOR]
                .into_iter()
                .find(|&c| canvas.count(c) > 0)
        };
        assert_eq!(fill(15, false), Some(CRITICAL_COLOR));
        assert_eq!(fill(16, false), Some(LOW_COLOR));
        assert_eq!(fill(35, false), Some(LOW_COLOR));
        assert_eq!(fill(36, false), Some(OK_COLOR));
        assert_eq!(fill(5, true), Some(CHARGING_COLOR));
    }

    #[test]
    fn no_reading_shows_the_unknown_mark() {
        assert!(battery_canvas(None).count(UNKNOWN_COLOR) > 0);
    }

    #[test]
    fn every_percentage_renders_as_outlined_digits() {
        for p in 0..=100 {
            let canvas = text_canvas(&device(p, false));
            assert!(canvas.count(OUTLINE_COLOR) > 0, "{p}% has no outline");
        }
    }
}
