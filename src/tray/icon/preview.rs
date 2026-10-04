//! Keeps `docs/assets/tray-icons.svg`, the README image, in sync with the icons.
//!
//! The image is drawn from the real icon pixels. After a change to the icon
//! drawing, run `UPDATE_ICON_PREVIEW=1 cargo test icon_preview` to redraw it.

use super::{Canvas, battery_canvas, text_canvas};
use crate::model::{BatteryState, DeviceKey, Percent};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

/// Displayed edge length of every icon, in SVG pixels.
const ICON_PX: f64 = 80.0;
const TILE_W: f64 = 150.0;
const LEFT: f64 = 200.0;
const PAD: f64 = 32.0;
const ROW_H: f64 = 150.0;
/// Windows 11 dark taskbar.
const BACKGROUND: &str = "#202020";
const FOREGROUND: &str = "#f3f3f3";
const MUTED: &str = "#a8a8a8";
const FONT: &str = "Segoe UI, system-ui, -apple-system, Helvetica, Arial, sans-serif";

fn device(percent: u8, charging: bool) -> BatteryState {
    BatteryState {
        key: DeviceKey::new(1, None),
        name: String::new(),
        pid: 1,
        percent: Percent::try_from(percent).expect("valid percent"),
        charging,
    }
}

fn render() -> String {
    let battery = |p, charging| battery_canvas(Some(&device(p, charging)));
    let text = |p, charging| text_canvas(&device(p, charging));
    let rows = [
        (
            "Battery icon",
            "default view",
            vec![
                (battery(82, false), "82%"),
                (battery(28, false), "28%"),
                (battery(9, false), "9%"),
                (battery(64, true), "64%, charging"),
                (battery_canvas(None), "No reading"),
            ],
        ),
        (
            "Percentage as text",
            "optional view",
            vec![
                (text(82, false), "82%"),
                (text(28, false), "28%"),
                (text(9, false), "9%"),
                (text(100, true), "100%, charging"),
            ],
        ),
    ];

    let width = LEFT + 5.0 * TILE_W + PAD;
    let height = PAD * 2.0 + ROW_H * 2.0 - 10.0;
    let mut svg = String::new();
    let _ = writeln!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" role="img" aria-labelledby="title desc">"#
    );
    svg.push_str("<title id=\"title\">razertray tray icons</title>\n");
    svg.push_str(
        "<desc id=\"desc\">The tray icons razertray draws on a dark taskbar: a battery that \
         fills and changes color with the level (green, orange, red, blue while charging, and \
         a gray mark when there is no reading), and the optional view that shows the \
         percentage as colored digits.</desc>\n",
    );
    let _ = writeln!(
        svg,
        r#"<rect width="{width}" height="{height}" rx="16" fill="{BACKGROUND}"/>"#
    );
    let _ = writeln!(svg, r#"<g font-family="{FONT}">"#);

    for (row, (title, subtitle, tiles)) in rows.iter().enumerate() {
        let top = PAD + f64::from(u8::try_from(row).expect("two rows")) * ROW_H;
        let label_y = top + ICON_PX / 2.0;
        let _ = writeln!(
            svg,
            r#"<text x="{PAD}" y="{label_y}" fill="{FOREGROUND}" font-size="20" font-weight="600">{title}</text>"#
        );
        let _ = writeln!(
            svg,
            r#"<text x="{PAD}" y="{}" fill="{MUTED}" font-size="16">{subtitle}</text>"#,
            label_y + 26.0
        );
        for (tile, (canvas, caption)) in tiles.iter().enumerate() {
            let center = LEFT + (f64::from(u8::try_from(tile).expect("few tiles")) + 0.5) * TILE_W;
            svg.push_str("<g shape-rendering=\"crispEdges\">\n");
            push_pixels(&mut svg, canvas, center - ICON_PX / 2.0, top);
            svg.push_str("</g>\n");
            let _ = writeln!(
                svg,
                r#"<text x="{center}" y="{}" fill="{FOREGROUND}" font-size="16" text-anchor="middle">{caption}</text>"#,
                top + ICON_PX + 30.0
            );
        }
    }
    svg.push_str("</g></svg>\n");
    svg
}

/// Draws `canvas` as one rect per horizontal run of equal pixels.
fn push_pixels(svg: &mut String, canvas: &Canvas, x0: f64, y0: f64) {
    let size = canvas.size;
    let scale = ICON_PX / f64::from(u32::try_from(size).expect("small icon"));
    let pixel = |x: usize, y: usize| -> &[u8] {
        let idx = (y * size + x) * 4;
        &canvas.pixels[idx..idx + 4]
    };
    let coord = |n: usize| f64::from(u32::try_from(n).expect("small icon")) * scale;

    for y in 0..size {
        let mut x = 0;
        while x < size {
            let rgba = pixel(x, y);
            let run = (x..size).take_while(|&nx| pixel(nx, y) == rgba).count();
            if rgba[3] > 0 {
                let opacity = if rgba[3] == 255 {
                    String::new()
                } else {
                    format!(r#" fill-opacity="{:.2}""#, f64::from(rgba[3]) / 255.0)
                };
                let _ = writeln!(
                    svg,
                    r##"<rect x="{}" y="{}" width="{}" height="{scale}" fill="#{:02x}{:02x}{:02x}"{opacity}/>"##,
                    x0 + coord(x),
                    y0 + coord(y),
                    coord(run),
                    rgba[0],
                    rgba[1],
                    rgba[2],
                );
            }
            x += run;
        }
    }
}

#[test]
fn icon_preview_is_current() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/assets/tray-icons.svg");
    let svg = render();
    if std::env::var_os("UPDATE_ICON_PREVIEW").is_some() {
        fs::write(&path, &svg).expect("write icon preview");
        return;
    }
    // Windows checkouts can convert line endings.
    let committed = fs::read_to_string(&path)
        .expect("read icon preview")
        .replace("\r\n", "\n");
    assert!(
        committed == svg,
        "docs/assets/tray-icons.svg is out of date; run UPDATE_ICON_PREVIEW=1 cargo test icon_preview"
    );
}
