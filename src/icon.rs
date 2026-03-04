use anyhow::{Context, Result};
use tray_icon::Icon;

const SIZE: usize = 16;

pub fn neutral_icon() -> Result<Icon> {
    build_icon(None, false)
}

pub fn battery_icon(percent: u8, charging: bool) -> Result<Icon> {
    build_icon(Some(percent), charging)
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
    let color = if charging {
        [90, 170, 255, 255]
    } else if percent <= 15 {
        [230, 70, 70, 255]
    } else if percent <= 35 {
        [240, 170, 70, 255]
    } else {
        [90, 220, 120, 255]
    };

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
    if x >= SIZE || y >= SIZE {
        return;
    }

    let idx = (y * SIZE + x) * 4;
    pixels[idx] = rgba[0];
    pixels[idx + 1] = rgba[1];
    pixels[idx + 2] = rgba[2];
    pixels[idx + 3] = rgba[3];
}

#[cfg(test)]
mod tests {
    use super::{battery_icon, neutral_icon};

    #[test]
    fn icon_generation_works() {
        assert!(neutral_icon().is_ok());
        assert!(battery_icon(5, false).is_ok());
        assert!(battery_icon(50, false).is_ok());
        assert!(battery_icon(100, true).is_ok());
    }
}
