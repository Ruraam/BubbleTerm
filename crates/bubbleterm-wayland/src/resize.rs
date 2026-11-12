//! Translate Wayland surface dimensions into kernel PTY dimensions.

use anyhow::{Result, ensure};
use bubbleterm_pty::PtySession;
use portable_pty::PtySize;

/// Committed drawable area, in physical pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceSize {
    pub width: u32,
    pub height: u32,
}

/// Measured terminal cell size, in physical pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellMetrics {
    pub width: u32,
    pub height: u32,
}

/// Convert a Wayland surface size to a PTY winsize.
///
/// A zero-sized surface is a normal transient state during configure and
/// returns `None`, leaving the previous PTY size unchanged. Cell metrics
/// must always be non-zero. Cell counts use floor division so partially
/// visible cells are never exposed to the terminal application.
pub fn pty_size_for_surface(surface: SurfaceSize, cell: CellMetrics) -> Result<Option<PtySize>> {
    ensure!(
        cell.width > 0 && cell.height > 0,
        "terminal cell dimensions must be non-zero"
    );

    if surface.width == 0 || surface.height == 0 {
        return Ok(None);
    }

    let cols = surface.width / cell.width;
    let rows = surface.height / cell.height;
    ensure!(
        cols > 0 && rows > 0,
        "surface is smaller than one terminal cell"
    );

    let size = PtySize {
        rows: u16::try_from(rows)?,
        cols: u16::try_from(cols)?,
        pixel_width: u16::try_from(surface.width.min(u32::from(u16::MAX)))?,
        pixel_height: u16::try_from(surface.height.min(u32::from(u16::MAX)))?,
    };

    Ok(Some(size))
}

/// Apply a committed Wayland surface size to its corresponding PTY session.
///
/// Call this from the Wayland configure/commit path after updating the
/// renderer's physical-pixel cell metrics. The PTY master updates the kernel
/// winsize; the kernel delivers SIGWINCH to the foreground process group.
pub fn resize_pty_for_surface(
    session: &PtySession,
    surface: SurfaceSize,
    cell: CellMetrics,
) -> Result<Option<PtySize>> {
    let Some(size) = pty_size_for_surface(surface, cell)? else {
        return Ok(None);
    };

    session.resize(size)?;
    Ok(Some(size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_surface_pixels_to_whole_terminal_cells() {
        let size = pty_size_for_surface(
            SurfaceSize {
                width: 1287,
                height: 803,
            },
            CellMetrics {
                width: 9,
                height: 17,
            },
        )
        .unwrap()
        .unwrap();

        assert_eq!(size.cols, 143);
        assert_eq!(size.rows, 47);
        assert_eq!(size.pixel_width, 1287);
        assert_eq!(size.pixel_height, 803);
    }

    #[test]
    fn zero_sized_surface_preserves_previous_pty_size() {
        assert_eq!(
            pty_size_for_surface(
                SurfaceSize {
                    width: 0,
                    height: 600,
                },
                CellMetrics {
                    width: 8,
                    height: 16,
                },
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn rejects_zero_cell_metrics() {
        assert!(
            pty_size_for_surface(
                SurfaceSize {
                    width: 800,
                    height: 600,
                },
                CellMetrics {
                    width: 0,
                    height: 16,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_surface_smaller_than_one_cell() {
        assert!(
            pty_size_for_surface(
                SurfaceSize {
                    width: 7,
                    height: 15,
                },
                CellMetrics {
                    width: 8,
                    height: 16,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_cell_counts_that_do_not_fit_pty_winsize() {
        assert!(
            pty_size_for_surface(
                SurfaceSize {
                    width: 600_000,
                    height: 600,
                },
                CellMetrics {
                    width: 1,
                    height: 16,
                },
            )
            .is_err()
        );
    }
}
