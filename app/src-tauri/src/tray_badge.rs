//! The menu bar badge with the current layout: a rounded chip with the
//! layout's two-letter code knocked out of it, centred in the menu bar.
//!
//! Drawn here from a small pixel font of our own, so it needs no font files
//! and looks the same on every Mac. On macOS it is a template image (only the
//! alpha matters; the system tints it for light and dark menu bars). Windows
//! draws tray icons as they are, so there it gets a dark chip and white
//! letters.

use rekey_core::config::MenuBarSize;

/// Menu bar height in pixels at 2x: 22 points.
pub const CANVAS_HEIGHT: u32 = 44;

/// 5x7 glyphs, one `u8` per row, the low five bits from left to right.
fn glyph(c: char) -> Option<[u8; 7]> {
    Some(match c {
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        _ => return None,
    })
}

/// The two letters shown for a layout id.
pub fn code_for(layout: &str) -> &'static str {
    match layout {
        "us" => "EN",
        "ru" => "RU",
        "uk" => "UA",
        "he" => "HE",
        "ar" => "AR",
        "el" => "EL",
        "de" => "DE",
        "fr" => "FR",
        "es" => "ES",
        "tr" => "TR",
        _ => "",
    }
}

/// Pixel size of one font dot and the chip height, per size setting.
fn metrics(size: MenuBarSize) -> (u32, u32) {
    match size {
        MenuBarSize::Small => (2, 24),
        MenuBarSize::Medium => (3, 32),
        MenuBarSize::Large => (4, 40),
    }
}

/// An RGBA image of the badge: (pixels, width, height).
pub fn render(layout: &str, size: MenuBarSize, template: bool) -> (Vec<u8>, u32, u32) {
    let (dot, chip_h) = metrics(size);
    let code: Vec<[u8; 7]> = code_for(layout).chars().filter_map(glyph).collect();
    let text_w = if code.is_empty() {
        0
    } else {
        code.len() as u32 * 5 * dot + (code.len() as u32 - 1) * dot
    };
    let text_h = 7 * dot;
    let pad_x = (chip_h - text_h) / 2 + dot;
    let chip_w = (text_w + 2 * pad_x).max(chip_h);
    // A little room on each side so the chip does not touch its neighbours.
    let margin = 2;
    let width = chip_w + 2 * margin;
    let height = CANVAS_HEIGHT;
    let top = (height - chip_h) / 2;
    let radius = (chip_h as f32 * 0.28).round();

    let mut px = vec![0u8; (width * height * 4) as usize];
    let (fill, ink): ([u8; 4], [u8; 4]) = if template {
        ([0, 0, 0, 255], [0, 0, 0, 0])
    } else {
        ([46, 46, 52, 255], [255, 255, 255, 255])
    };
    let mut put = |x: u32, y: u32, rgba: [u8; 4]| {
        let i = ((y * width + x) * 4) as usize;
        px[i..i + 4].copy_from_slice(&rgba);
    };

    // Rounded chip, with a one-pixel soft edge on the corners.
    for y in 0..chip_h {
        for x in 0..chip_w {
            let cx = (x as f32 + 0.5).clamp(radius, chip_w as f32 - radius);
            let cy = (y as f32 + 0.5).clamp(radius, chip_h as f32 - radius);
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            let coverage = (radius + 0.5 - d).clamp(0.0, 1.0);
            if coverage > 0.0 {
                let mut c = fill;
                c[3] = (fill[3] as f32 * coverage).round() as u8;
                put(margin + x, top + y, c);
            }
        }
    }

    // Letters, centred in the chip.
    let text_x = margin + (chip_w - text_w) / 2;
    let text_y = top + (chip_h - text_h) / 2;
    for (n, rows) in code.iter().enumerate() {
        let gx = text_x + n as u32 * 6 * dot;
        for (row, bits) in rows.iter().enumerate() {
            for col in 0..5u32 {
                if bits & (0b10000 >> col) == 0 {
                    continue;
                }
                for dy in 0..dot {
                    for dx in 0..dot {
                        put(gx + col * dot + dx, text_y + row as u32 * dot + dy, ink);
                    }
                }
            }
        }
    }
    (px, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows and columns that have any opaque pixel.
    fn ink_bounds(px: &[u8], w: u32, h: u32) -> (u32, u32, u32, u32) {
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
        for y in 0..h {
            for x in 0..w {
                if px[((y * w + x) * 4 + 3) as usize] > 0 {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        (x0, y0, x1, y1)
    }

    #[test]
    fn every_layout_has_a_drawable_code() {
        for id in ["us", "ru", "uk", "he", "ar", "el", "de", "fr", "es", "tr"] {
            let code = code_for(id);
            assert_eq!(code.len(), 2, "{id}");
            assert!(code.chars().all(|c| glyph(c).is_some()), "{id}: {code}");
        }
    }

    #[test]
    fn the_badge_is_centred_in_the_menu_bar_at_every_size() {
        for size in [MenuBarSize::Small, MenuBarSize::Medium, MenuBarSize::Large] {
            let (px, w, h) = render("ru", size, true);
            assert_eq!(h, CANVAS_HEIGHT);
            assert_eq!(px.len(), (w * h * 4) as usize);
            let (x0, y0, x1, y1) = ink_bounds(&px, w, h);
            let (top, bottom) = (y0, h - 1 - y1);
            let (left, right) = (x0, w - 1 - x1);
            assert!(
                top.abs_diff(bottom) <= 1,
                "{size:?}: top {top}, bottom {bottom}"
            );
            assert!(
                left.abs_diff(right) <= 1,
                "{size:?}: left {left}, right {right}"
            );
        }
    }

    #[test]
    fn sizes_grow_and_letters_are_knocked_out() {
        let heights: Vec<u32> = [MenuBarSize::Small, MenuBarSize::Medium, MenuBarSize::Large]
            .into_iter()
            .map(|s| {
                let (px, w, h) = render("us", s, true);
                let (_, y0, _, y1) = ink_bounds(&px, w, h);
                y1 - y0
            })
            .collect();
        assert!(
            heights[0] < heights[1] && heights[1] < heights[2],
            "{heights:?}"
        );
        // The middle of the chip has transparent letter pixels in it.
        let (px, w, h) = render("us", MenuBarSize::Medium, true);
        let row = h / 2;
        let holes = (0..w)
            .filter(|x| px[((row * w + x) * 4 + 3) as usize] == 0)
            .count();
        assert!(holes > 6, "letters should be cut out of the chip");
    }
}
