use anyhow::{Context, Result};
use tray_icon::Icon;

const SIZE: usize = 16;
/// Text icons render at higher resolution so the downscaled digits stay crisp.
const TEXT_SIZE: usize = 32;

pub fn neutral_icon() -> Result<Icon> {
    build_icon(None, false)
}

pub fn battery_icon(percent: u8, charging: bool) -> Result<Icon> {
    build_icon(Some(percent), charging)
}

/// Render the battery percentage as text (the digits themselves are the icon).
/// Used by the "show percentage as text" view mode. Digits are colored by
/// level and given a dark outline so they read on light or dark taskbars.
pub fn text_icon(percent: u8, charging: bool) -> Result<Icon> {
    let mut pixels = vec![0u8; TEXT_SIZE * TEXT_SIZE * 4];
    let label = percent.min(100).to_string();
    draw_number(&mut pixels, &label, level_color(percent, charging));
    Icon::from_rgba(pixels, TEXT_SIZE as u32, TEXT_SIZE as u32)
        .context("failed building text tray icon")
}

/// Shared color ramp for both the battery fill and the text digits.
fn level_color(percent: u8, charging: bool) -> [u8; 4] {
    if charging {
        [90, 170, 255, 255]
    } else if percent <= 15 {
        [230, 70, 70, 255]
    } else if percent <= 35 {
        [240, 170, 70, 255]
    } else {
        [90, 220, 120, 255]
    }
}

/// 3x5 pixel glyph for a single digit. Each row's low 3 bits are columns
/// (0b100 = leftmost).
fn digit_glyph(d: u8) -> [u8; 5] {
    match d {
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

/// Draw the digit string centered on the TEXT_SIZE canvas, with a 1px dark
/// outline around the colored glyphs for contrast.
fn draw_number(pixels: &mut [u8], label: &str, color: [u8; 4]) {
    let outline = [25, 25, 25, 235];
    let n = label.len();
    if n == 0 {
        return;
    }

    // Glyph cell is 3 units wide; 1 unit gap between glyphs. Fit to canvas
    // (minus 1px padding each side) in both axes and pick the largest scale.
    let units_w = 3 * n + (n - 1);
    let scale = ((TEXT_SIZE - 2) / units_w).clamp(1, (TEXT_SIZE - 2) / 5);
    let total_w = units_w * scale;
    let total_h = 5 * scale;
    let x0 = (TEXT_SIZE - total_w) / 2;
    let y0 = (TEXT_SIZE - total_h) / 2;

    // First rasterize a fill mask, then derive the outline from it.
    let mut mask = vec![false; TEXT_SIZE * TEXT_SIZE];
    for (gi, ch) in label.bytes().enumerate() {
        let glyph = digit_glyph(ch.wrapping_sub(b'0'));
        let gx = x0 + gi * 4 * scale;
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..3 {
                if bits & (0b100 >> col) != 0 {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let x = gx + col * scale + dx;
                            let y = y0 + row * scale + dy;
                            if x < TEXT_SIZE && y < TEXT_SIZE {
                                mask[y * TEXT_SIZE + x] = true;
                            }
                        }
                    }
                }
            }
        }
    }

    // Outline: any non-fill pixel adjacent (8-neighborhood) to a fill pixel.
    for y in 0..TEXT_SIZE {
        for x in 0..TEXT_SIZE {
            if mask[y * TEXT_SIZE + x] {
                continue;
            }
            let mut touches = false;
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let nx = x as i32 + dx;
                    let ny = y as i32 + dy;
                    if nx >= 0
                        && ny >= 0
                        && (nx as usize) < TEXT_SIZE
                        && (ny as usize) < TEXT_SIZE
                        && mask[ny as usize * TEXT_SIZE + nx as usize]
                    {
                        touches = true;
                    }
                }
            }
            if touches {
                put_sized(pixels, TEXT_SIZE, x, y, outline);
            }
        }
    }

    // Colored fill on top.
    for y in 0..TEXT_SIZE {
        for x in 0..TEXT_SIZE {
            if mask[y * TEXT_SIZE + x] {
                put_sized(pixels, TEXT_SIZE, x, y, color);
            }
        }
    }
}

fn put_sized(pixels: &mut [u8], size: usize, x: usize, y: usize, rgba: [u8; 4]) {
    if x >= size || y >= size {
        return;
    }
    let idx = (y * size + x) * 4;
    pixels[idx..idx + 4].copy_from_slice(&rgba);
}

fn build_icon(percent: Option<u8>, charging: bool) -> Result<Icon> {
    let mut pixels = vec![0u8; SIZE * SIZE * 4];

    draw_battery_shell(&mut pixels);

    match percent {
        Some(level) => draw_battery_fill(&mut pixels, level, charging),
        None => draw_unknown_mark(&mut pixels),
    }

    Icon::from_rgba(pixels, SIZE as u32, SIZE as u32).context("failed building tray icon")
}

fn draw_battery_shell(pixels: &mut [u8]) {
    let border = [210, 210, 210, 255];

    // Body border
    for x in 2..14 {
        put(pixels, x, 4, border);
        put(pixels, x, 11, border);
    }
    for y in 4..12 {
        put(pixels, 2, y, border);
        put(pixels, 13, y, border);
    }

    // Battery cap
    put(pixels, 14, 7, border);
    put(pixels, 15, 7, border);
    put(pixels, 14, 8, border);
    put(pixels, 15, 8, border);
}

fn draw_battery_fill(pixels: &mut [u8], percent: u8, charging: bool) {
    let color = level_color(percent, charging);

    let clamped = percent.min(100) as usize;
    let width = (clamped * 10).div_ceil(100);

    for x in 3..(3 + width.max(1)) {
        for y in 5..11 {
            put(pixels, x, y, color);
        }
    }
}

fn draw_unknown_mark(pixels: &mut [u8]) {
    let color = [150, 150, 150, 255];
    for i in 0..5 {
        put(pixels, 5 + i, 6 + i, color);
        put(pixels, 9 - i, 6 + i, color);
    }
}

fn put(pixels: &mut [u8], x: usize, y: usize, rgba: [u8; 4]) {
    put_sized(pixels, SIZE, x, y, rgba);
}

#[cfg(test)]
mod tests {
    use super::{battery_icon, neutral_icon, text_icon};

    #[test]
    fn icon_generation_works() {
        assert!(neutral_icon().is_ok());
        assert!(battery_icon(5, false).is_ok());
        assert!(battery_icon(50, false).is_ok());
        assert!(battery_icon(100, true).is_ok());
    }

    #[test]
    fn text_icon_generation_works() {
        // 1, 2, and 3 digit values must all rasterize without panicking.
        assert!(text_icon(7, false).is_ok());
        assert!(text_icon(76, false).is_ok());
        assert!(text_icon(100, true).is_ok());
        assert!(text_icon(0, false).is_ok());
    }
}
