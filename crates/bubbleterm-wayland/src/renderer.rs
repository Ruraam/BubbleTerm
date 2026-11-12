//! CPU renderer for the owned VT grid snapshots.
use anyhow::{Result, ensure};
use bubbleterm_vt::{GridSnapshot, TerminalCell, TerminalColor};
use crate::CellMetrics;
use font8x8::{BASIC_FONTS, UnicodeFonts};

const DEFAULT_FG: (u8, u8, u8) = (220, 220, 228);
const DEFAULT_BG: (u8, u8, u8) = (24, 24, 32);

/// Render a VT snapshot into a Wayland ARGB8888 buffer (little-endian BGRA bytes).
pub fn render_snapshot(
    snapshot: &GridSnapshot,
    width: u32,
    height: u32,
    metrics: CellMetrics,
) -> Result<Vec<u8>> {
    ensure!(width > 0 && height > 0, "render dimensions must be non-zero");
    ensure!(metrics.width > 0 && metrics.height > 0, "cell metrics must be non-zero");
    let stride = width.checked_mul(4).ok_or_else(|| anyhow::anyhow!("render stride overflow"))?;
    let byte_len = stride.checked_mul(height).ok_or_else(|| anyhow::anyhow!("render buffer overflow"))?;
    ensure!(byte_len <= 64 * 1024 * 1024, "render buffer exceeds 64 MiB");
    let mut pixels = vec![0u8; byte_len as usize];
    for pixel in pixels.chunks_exact_mut(4) {
        write_pixel(pixel, DEFAULT_BG);
    }

    for row in 0..snapshot.size.rows {
        for column in 0..snapshot.size.columns {
            let Some(cell) = snapshot.cell(row, column) else { continue };
            let x = u32::from(column) * metrics.width;
            let y = u32::from(row) * metrics.height;
            if x >= width || y >= height { continue; }
            draw_cell(&mut pixels, width, height, stride, x, y, metrics, cell);
        }
    }

    if snapshot.cursor_visible
        && snapshot.cursor_row < snapshot.size.rows
        && snapshot.cursor_column < snapshot.size.columns
    {
        let x = u32::from(snapshot.cursor_column) * metrics.width;
        let y = u32::from(snapshot.cursor_row) * metrics.height;
        let cursor_height = 2.min(metrics.height);
        for cy in y..(y + metrics.height).min(height) {
            for cx in x..(x + metrics.width).min(width) {
                if cy >= y + metrics.height - cursor_height {
                    let offset = (cy * stride + cx * 4) as usize;
                    invert_pixel(&mut pixels[offset..offset + 4]);
                }
            }
        }
    }
    Ok(pixels)
}

fn draw_cell(
    pixels: &mut [u8], width: u32, height: u32, stride: u32,
    x: u32, y: u32, metrics: CellMetrics, cell: &TerminalCell,
) {
    let (mut fg, mut bg) = (resolve_color(cell.foreground, true, cell.bold), resolve_color(cell.background, false, false));
    if cell.inverse { std::mem::swap(&mut fg, &mut bg); }
    if cell.dim { fg = (fg.0 / 2, fg.1 / 2, fg.2 / 2); }

    for cy in y..(y + metrics.height).min(height) {
        for cx in x..(x + metrics.width).min(width) {
            let offset = (cy * stride + cx * 4) as usize;
            write_pixel(&mut pixels[offset..offset + 4], bg);
        }
    }

    if cell.wide_continuation || cell.text.is_empty() { return; }
    let character = cell.text.chars().next().unwrap_or(' ');
    let glyph = BASIC_FONTS.get(character).or_else(|| BASIC_FONTS.get('?'));
    let Some(glyph) = glyph else { return };
    let glyph_width = metrics.width.min(8);
    let glyph_height = metrics.height.min(8);
    let x_offset = (metrics.width - glyph_width) / 2;
    let y_offset = (metrics.height - glyph_height) / 2;

    for py in 0..glyph_height {
        let source_y = py * 8 / glyph_height.max(1);
        let bits = glyph[source_y as usize];
        for px in 0..glyph_width {
            let source_x = px * 8 / glyph_width.max(1);
            let set = bits & (1 << source_x) != 0;
            if !set { continue; }
            let dx = x + x_offset + px;
            let dy = y + y_offset + py;
            if dx >= width || dy >= height { continue; }
            let offset = (dy * stride + dx * 4) as usize;
            write_pixel(&mut pixels[offset..offset + 4], fg);
            if cell.bold && dx + 1 < width && px + 1 < glyph_width {
                let next = (dy * stride + (dx + 1) * 4) as usize;
                write_pixel(&mut pixels[next..next + 4], fg);
            }
        }
    }

    if cell.underline && metrics.height > 1 {
        let uy = (y + metrics.height - 2).min(height - 1);
        for ux in x..(x + metrics.width).min(width) {
            let offset = (uy * stride + ux * 4) as usize;
            write_pixel(&mut pixels[offset..offset + 4], fg);
        }
    }
}

fn resolve_color(color: TerminalColor, foreground: bool, bold: bool) -> (u8, u8, u8) {
    match color {
        TerminalColor::Default => if foreground { DEFAULT_FG } else { DEFAULT_BG },
        TerminalColor::Rgb(r, g, b) => (r, g, b),
        TerminalColor::Indexed(index) => {
            let index = if bold && foreground && index < 8 { index + 8 } else { index };
            if index < 16 {
                const PALETTE: [(u8,u8,u8);16] = [
                    (0,0,0),(205,49,49),(13,188,121),(229,229,16),
                    (36,114,200),(188,63,188),(17,168,205),(229,229,229),
                    (102,102,102),(241,76,76),(35,209,139),(245,245,67),
                    (59,142,234),(214,112,214),(41,184,219),(255,255,255),
                ];
                PALETTE[index as usize]
            } else if index < 232 {
                let value = index - 16;
                let levels = [0, 95, 135, 175, 215, 255];
                (levels[(value / 36) as usize], levels[((value % 36) / 6) as usize], levels[(value % 6) as usize])
            } else {
                let gray = 8 + (index - 232) * 10;
                (gray, gray, gray)
            }
        }
    }
}

fn write_pixel(target: &mut [u8], rgb: (u8,u8,u8)) {
    target[0] = rgb.2;
    target[1] = rgb.1;
    target[2] = rgb.0;
    target[3] = 255;
}

fn invert_pixel(pixel: &mut [u8]) {
    pixel[0] = 255 - pixel[0];
    pixel[1] = 255 - pixel[1];
    pixel[2] = 255 - pixel[2];
    pixel[3] = 255;
}

#[cfg(test)]
mod tests {
    use super::*;
    use bubbleterm_vt::{GridSize, Terminal};

    #[test]
    fn paints_default_background_and_visible_glyph_pixels() {
        let mut terminal = Terminal::new(GridSize::new(2, 1).unwrap(), 0).unwrap();
        terminal.process(b"A");
        let image = render_snapshot(&terminal.snapshot(), 16, 8, CellMetrics { width: 8, height: 8 }).unwrap();
        assert_eq!(&image[0..4], &[32, 24, 24, 255]);
        assert!(image.chunks_exact(4).any(|pixel| pixel[0..3] != [32, 24, 24]));
    }

    #[test]
    fn maps_ansi_rgb_foreground_to_bgra() {
        let mut terminal = Terminal::new(GridSize::new(1, 1).unwrap(), 0).unwrap();
        terminal.process(b"\x1b[38;2;12;34;56mX");
        let image = render_snapshot(&terminal.snapshot(), 8, 8, CellMetrics { width: 8, height: 8 }).unwrap();
        assert!(image.chunks_exact(4).any(|pixel| pixel[0..3] == [56, 34, 12]));
    }

    #[test]
    fn rejects_zero_dimensions_and_oversized_buffers() {
        let terminal = Terminal::new(GridSize::new(1, 1).unwrap(), 0).unwrap();
        assert!(render_snapshot(&terminal.snapshot(), 0, 8, CellMetrics { width: 8, height: 8 }).is_err());
        assert!(render_snapshot(&terminal.snapshot(), 8192, 8192, CellMetrics { width: 8, height: 8 }).is_err());
    }
}
