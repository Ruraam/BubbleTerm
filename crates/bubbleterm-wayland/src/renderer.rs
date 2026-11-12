//! CPU renderer for the owned VT grid snapshots.
use anyhow::{Result, ensure};
use bubbleterm_vt::{GridSnapshot, TerminalCell, TerminalColor};
use font8x8::{BASIC_FONTS, UnicodeFonts};
use unicode_normalization::UnicodeNormalization;

use crate::CellMetrics;

const DEFAULT_FG: (u8, u8, u8) = (220, 220, 228);
const DEFAULT_BG: (u8, u8, u8) = (24, 24, 32);
const MAX_RENDER_BYTES: u32 = 64 * 1024 * 1024;

/// Render a VT snapshot into a Wayland ARGB8888 buffer (little-endian BGRA bytes).
pub fn render_snapshot(
    snapshot: &GridSnapshot,
    width: u32,
    height: u32,
    metrics: CellMetrics,
) -> Result<Vec<u8>> {
    ensure!(
        width > 0 && height > 0,
        "render dimensions must be non-zero"
    );
    ensure!(
        metrics.width > 0 && metrics.height > 0,
        "cell metrics must be non-zero"
    );
    ensure!(
        metrics.width <= 256 && metrics.height <= 256,
        "cell metrics exceed the renderer safety limit"
    );

    let stride = width
        .checked_mul(4)
        .ok_or_else(|| anyhow::anyhow!("render stride overflow"))?;
    let byte_len = stride
        .checked_mul(height)
        .ok_or_else(|| anyhow::anyhow!("render buffer overflow"))?;
    ensure!(byte_len <= MAX_RENDER_BYTES, "render buffer exceeds 64 MiB");

    let mut target = RenderTarget {
        pixels: vec![0; byte_len as usize],
        width,
        height,
        stride,
    };
    target.fill_rect(0, 0, width, height, DEFAULT_BG);

    for row in 0..snapshot.size.rows {
        for column in 0..snapshot.size.columns {
            let Some(cell) = snapshot.cell(row, column) else {
                continue;
            };
            let x = u32::from(column) * metrics.width;
            let y = u32::from(row) * metrics.height;
            if x < width && y < height {
                draw_cell(&mut target, x, y, metrics, cell);
            }
        }
    }

    if snapshot.cursor_visible
        && snapshot.cursor_row < snapshot.size.rows
        && snapshot.cursor_column < snapshot.size.columns
    {
        let x = u32::from(snapshot.cursor_column) * metrics.width;
        let y = u32::from(snapshot.cursor_row) * metrics.height;
        let cursor_height = 2.min(metrics.height);
        let cursor_y = y.saturating_add(metrics.height - cursor_height);
        for cy in cursor_y..y.saturating_add(metrics.height).min(height) {
            for cx in x..x.saturating_add(metrics.width).min(width) {
                target.invert_pixel(cx, cy);
            }
        }
    }

    Ok(target.pixels)
}

struct RenderTarget {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    stride: u32,
}

impl RenderTarget {
    fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: (u8, u8, u8)) {
        let right = x.saturating_add(width).min(self.width);
        let bottom = y.saturating_add(height).min(self.height);
        for py in y.min(self.height)..bottom {
            for px in x.min(self.width)..right {
                self.set_pixel(px, py, color);
            }
        }
    }

    fn set_pixel(&mut self, x: u32, y: u32, color: (u8, u8, u8)) {
        if x >= self.width || y >= self.height {
            return;
        }
        let offset = (y * self.stride + x * 4) as usize;
        let pixel = &mut self.pixels[offset..offset + 4];
        pixel[0] = color.2;
        pixel[1] = color.1;
        pixel[2] = color.0;
        pixel[3] = 255;
    }

    fn invert_pixel(&mut self, x: u32, y: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let offset = (y * self.stride + x * 4) as usize;
        let pixel = &mut self.pixels[offset..offset + 4];
        pixel[0] = 255 - pixel[0];
        pixel[1] = 255 - pixel[1];
        pixel[2] = 255 - pixel[2];
        pixel[3] = 255;
    }
}

fn draw_cell(target: &mut RenderTarget, x: u32, y: u32, metrics: CellMetrics, cell: &TerminalCell) {
    let mut foreground = resolve_color(cell.foreground, true, cell.bold);
    let mut background = resolve_color(cell.background, false, false);
    if cell.inverse {
        std::mem::swap(&mut foreground, &mut background);
    }
    if cell.dim {
        foreground = (foreground.0 / 2, foreground.1 / 2, foreground.2 / 2);
    }

    target.fill_rect(x, y, metrics.width, metrics.height, background);
    if cell.wide_continuation || cell.text.is_empty() {
        return;
    }

    // BASIC_FONTS is embedded in the binary. Unsupported Unicode currently
    // receives a visible replacement glyph rather than depending on host fonts.
    let character = cell.text.nfc().next().unwrap_or(' ');
    if !character.is_ascii()
        && !is_custom_unicode_glyph(character)
        && draw_noto_glyph(target, x, y, metrics, character, foreground, cell.bold)
    {
        if cell.underline && metrics.height > 1 {
            let underline_y = y + metrics.height - 2;
            target.fill_rect(x, underline_y, metrics.width, 1, foreground);
        }
        return;
    }

    let Some(glyph) = glyph_bitmap(character) else {
        return;
    };

    let glyph_width = metrics.width.min(8);
    let glyph_height = metrics.height.min(8);
    let x_offset = (metrics.width - glyph_width) / 2;
    let y_offset = (metrics.height - glyph_height) / 2;

    for py in 0..glyph_height {
        let source_y = py * 8 / glyph_height;
        let bits = glyph[source_y as usize];
        let italic_offset = if cell.italic { (7 - source_y) / 3 } else { 0 };
        for px in 0..glyph_width {
            let source_x = px * 8 / glyph_width;
            if bits & (1 << source_x) == 0 {
                continue;
            }

            let dx = x + x_offset + px + italic_offset;
            let dy = y + y_offset + py;
            target.set_pixel(dx, dy, foreground);
            if cell.bold && px + 1 < glyph_width {
                target.set_pixel(dx + 1, dy, foreground);
            }
        }
    }

    if cell.underline && metrics.height > 1 {
        let underline_y = y + metrics.height - 2;
        target.fill_rect(x, underline_y, metrics.width, 1, foreground);
    }
}

/// Select the closest bundled raster height without upscaling a smaller glyph.
fn noto_raster_height(cell_height: u32) -> noto_sans_mono_bitmap::RasterHeight {
    use noto_sans_mono_bitmap::RasterHeight;

    match cell_height {
        0..=16 => RasterHeight::Size16,
        17..=20 => RasterHeight::Size20,
        21..=24 => RasterHeight::Size24,
        _ => RasterHeight::Size32,
    }
}

/// Render a bundled Noto Sans Mono glyph into the current terminal cell.
fn draw_noto_glyph(
    target: &mut RenderTarget,
    x: u32,
    y: u32,
    metrics: CellMetrics,
    character: char,
    foreground: (u8, u8, u8),
    bold: bool,
) -> bool {
    use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster};

    let weight = if bold { FontWeight::Bold } else { FontWeight::Regular };
    let raster_height = noto_raster_height(metrics.height);
    let Some(glyph) = get_raster(character, weight, raster_height) else {
        return false;
    };
    let raster = glyph.raster();
    let source_width = glyph.width();
    let source_height = glyph.height();
    if source_width == 0 || source_height == 0 || raster.len() != source_height {
        return false;
    }

    let draw_width = metrics.width.min(source_width as u32);
    let draw_height = metrics.height.min(source_height as u32);
    let x_offset = (metrics.width - draw_width) / 2;
    let y_offset = (metrics.height - draw_height) / 2;

    for dy in 0..draw_height {
        let source_y = (dy as usize * source_height) / draw_height as usize;
        let row = raster[source_y];
        if row.len() != source_width {
            return false;
        }
        for dx in 0..draw_width {
            let source_x = (dx as usize * source_width) / draw_width as usize;
            let coverage = u16::from(row[source_x]);
            if coverage == 0 {
                continue;
            }
            let background_offset =
                (((y + y_offset + dy) * target.stride) + ((x + x_offset + dx) * 4)) as usize;
            if background_offset + 3 >= target.pixels.len() {
                continue;
            }
            let (background_blue, background_green, background_red) = (
                target.pixels[background_offset],
                target.pixels[background_offset + 1],
                target.pixels[background_offset + 2],
            );
            let blend = |fg: u8, bg: u8| {
                ((u16::from(fg) * coverage + u16::from(bg) * (255 - coverage)) / 255) as u8
            };
            target.set_pixel(
                x + x_offset + dx,
                y + y_offset + dy,
                (
                    blend(foreground.0, background_red),
                    blend(foreground.1, background_green),
                    blend(foreground.2, background_blue),
                ),
            );
        }
    }
    true
}

fn is_custom_unicode_glyph(character: char) -> bool {
    matches!(
        character,
        '─' | '│'
            | '┌'
            | '┐'
            | '└'
            | '┘'
            | '├'
            | '┤'
            | '┬'
            | '┴'
            | '┼'
            | '█'
            | '▀'
            | '▄'
            | '▌'
            | '▐'
            | '░'
            | '▒'
            | '▓'
    )
}

/// Resolve common terminal Unicode drawing characters without host fonts.
/// ASCII uses the embedded font; box drawing and block elements are generated
/// as compact 8x8 bitmaps. Other unsupported codepoints use a visible fallback.
fn glyph_bitmap(character: char) -> Option<[u8; 8]> {
    if let Some(glyph) = BASIC_FONTS.get(character) {
        return Some(glyph);
    }

    let mut rows = [0_u8; 8];
    let box_directions = match character {
        '─' => Some((false, false, true, true)),
        '│' => Some((true, true, false, false)),
        '┌' => Some((false, true, false, true)),
        '┐' => Some((false, true, true, false)),
        '└' => Some((true, false, false, true)),
        '┘' => Some((true, false, true, false)),
        '├' => Some((true, true, false, true)),
        '┤' => Some((true, true, true, false)),
        '┬' => Some((false, true, true, true)),
        '┴' => Some((true, false, true, true)),
        '┼' => Some((true, true, true, true)),
        _ => None,
    };

    if let Some((up, down, left, right)) = box_directions {
        if up {
            rows[..4].fill(0x18);
        }
        if down {
            rows[4..].fill(0x18);
        }
        for row in &mut rows[3..5] {
            if left {
                *row |= 0x0f;
            }
            if right {
                *row |= 0xf0;
            }
        }
        return Some(rows);
    }

    match character {
        '█' => Some([0xff; 8]),
        '▀' => Some([0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0]),
        '▄' => Some([0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]),
        '▌' => Some([0xf0; 8]),
        '▐' => Some([0x0f; 8]),
        '░' => Some([0x11, 0x44, 0x11, 0x44, 0x11, 0x44, 0x11, 0x44]),
        '▒' => Some([0x55, 0xaa, 0x55, 0xaa, 0x55, 0xaa, 0x55, 0xaa]),
        '▓' => Some([0xee, 0xbb, 0xee, 0xbb, 0xee, 0xbb, 0xee, 0xbb]),
        _ => BASIC_FONTS.get('?'),
    }
}

fn resolve_color(color: TerminalColor, foreground: bool, bold: bool) -> (u8, u8, u8) {
    match color {
        TerminalColor::Default => {
            if foreground {
                DEFAULT_FG
            } else {
                DEFAULT_BG
            }
        }
        TerminalColor::Rgb(red, green, blue) => (red, green, blue),
        TerminalColor::Indexed(index) => {
            let index = if bold && foreground && index < 8 {
                index + 8
            } else {
                index
            };
            if index < 16 {
                const PALETTE: [(u8, u8, u8); 16] = [
                    (0, 0, 0),
                    (205, 49, 49),
                    (13, 188, 121),
                    (229, 229, 16),
                    (36, 114, 200),
                    (188, 63, 188),
                    (17, 168, 205),
                    (229, 229, 229),
                    (102, 102, 102),
                    (241, 76, 76),
                    (35, 209, 139),
                    (245, 245, 67),
                    (59, 142, 234),
                    (214, 112, 214),
                    (41, 184, 219),
                    (255, 255, 255),
                ];
                PALETTE[index as usize]
            } else if index < 232 {
                let value = index - 16;
                let levels = [0, 95, 135, 175, 215, 255];
                (
                    levels[(value / 36) as usize],
                    levels[((value % 36) / 6) as usize],
                    levels[(value % 6) as usize],
                )
            } else {
                let gray = 8 + (index - 232) * 10;
                (gray, gray, gray)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bubbleterm_vt::{GridSize, Terminal};

    fn metrics() -> CellMetrics {
        CellMetrics {
            width: 8,
            height: 8,
        }
    }

    #[test]
    fn paints_default_background_and_visible_glyph_pixels() {
        let mut terminal = Terminal::new(GridSize::new(2, 1).unwrap(), 0).unwrap();
        terminal.process(b"A");
        let image = render_snapshot(&terminal.snapshot(), 16, 8, metrics()).unwrap();
        assert_eq!(&image[0..4], &[32, 24, 24, 255]);
        assert!(
            image
                .chunks_exact(4)
                .any(|pixel| pixel[0..3] != [32, 24, 24])
        );
    }

    #[test]
    fn embedded_glyphs_cover_ascii_box_drawing_blocks_and_shades() {
        let supported = [
            'A', '0', '?', '─', '│', '┌', '┐', '└', '┘', '├', '┤', '┬', '┴', '┼', '█', '▀', '▄',
            '▌', '▐', '░', '▒', '▓',
        ];

        for character in supported {
            let bitmap = glyph_bitmap(character)
                .unwrap_or_else(|| panic!("missing bitmap for {character:?}"));
            assert!(
                bitmap.iter().any(|row| *row != 0),
                "glyph {character:?} must contain visible pixels"
            );
        }
    }

    #[test]
    fn bundled_noto_font_renders_precomposed_latin_unicode() {
        use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster};

        for character in ['é', 'à', 'ñ', 'Ā', 'Œ', '£'] {
            assert!(
                get_raster(character, FontWeight::Regular, RasterHeight::Size16).is_some(),
                "bundled font should contain {character:?}"
            );
        }

        let mut terminal = Terminal::new(GridSize::new(1, 1).unwrap(), 0).unwrap();
        terminal.process("é".as_bytes());
        let image = render_snapshot(&terminal.snapshot(), 8, 8, metrics()).unwrap();
        assert!(
            image
                .chunks_exact(4)
                .any(|pixel| pixel[0..3] != [32, 24, 24]),
            "accented glyph should paint visible pixels"
        );
    }

    #[test]
    fn noto_raster_selection_tracks_terminal_cell_height() {
        use noto_sans_mono_bitmap::RasterHeight;

        assert!(matches!(noto_raster_height(8), RasterHeight::Size16));
        assert!(matches!(noto_raster_height(16), RasterHeight::Size16));
        assert!(matches!(noto_raster_height(18), RasterHeight::Size20));
        assert!(matches!(noto_raster_height(22), RasterHeight::Size24));
        assert!(matches!(noto_raster_height(32), RasterHeight::Size32));
        assert!(matches!(noto_raster_height(64), RasterHeight::Size32));
    }

    #[test]
    fn bundled_noto_bold_glyphs_are_available() {
        use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster};

        for character in ['é', 'Ā', 'Œ'] {
            assert!(
                get_raster(character, FontWeight::Bold, RasterHeight::Size16).is_some(),
                "bold bundled font should contain {character:?}"
            );
        }
    }

    #[test]
    fn decomposed_latin_accents_render_like_precomposed_glyphs() {
        let mut decomposed = Terminal::new(GridSize::new(1, 1).unwrap(), 0).unwrap();
        decomposed.process("e\u{301}".as_bytes());

        let mut precomposed = Terminal::new(GridSize::new(1, 1).unwrap(), 0).unwrap();
        precomposed.process("é".as_bytes());

        let decomposed_image = render_snapshot(&decomposed.snapshot(), 8, 8, metrics()).unwrap();
        let precomposed_image = render_snapshot(&precomposed.snapshot(), 8, 8, metrics()).unwrap();
        assert_eq!(decomposed_image, precomposed_image);
    }

    #[test]
    fn unsupported_unicode_uses_visible_replacement_glyph() {
        let fallback = glyph_bitmap('?').expect("replacement glyph must exist");
        for character in ['界', 'Ж', '🙂', '\u{0301}'] {
            assert_eq!(
                glyph_bitmap(character),
                Some(fallback),
                "unsupported character {character:?} should use the replacement glyph"
            );
        }
    }

    #[test]
    fn maps_ansi_rgb_foreground_to_bgra() {
        let mut terminal = Terminal::new(GridSize::new(1, 1).unwrap(), 0).unwrap();
        terminal.process(b"\x1b[38;2;12;34;56mX");
        let image = render_snapshot(&terminal.snapshot(), 8, 8, metrics()).unwrap();
        assert!(
            image
                .chunks_exact(4)
                .any(|pixel| pixel[0..3] == [56, 34, 12])
        );
    }

    #[test]
    fn rejects_zero_dimensions_and_oversized_buffers() {
        let terminal = Terminal::new(GridSize::new(1, 1).unwrap(), 0).unwrap();
        assert!(render_snapshot(&terminal.snapshot(), 0, 8, metrics()).is_err());
        assert!(render_snapshot(&terminal.snapshot(), 8192, 8192, metrics()).is_err());
    }

    #[test]
    fn rejects_unreasonable_cell_metrics() {
        let terminal = Terminal::new(GridSize::new(1, 1).unwrap(), 0).unwrap();
        assert!(
            render_snapshot(
                &terminal.snapshot(),
                8,
                8,
                CellMetrics {
                    width: 1024,
                    height: 8,
                },
            )
            .is_err()
        );
    }
}
