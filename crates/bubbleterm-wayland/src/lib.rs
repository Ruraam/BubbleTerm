//! Native Wayland client boundary. X11 is intentionally unsupported.

#![forbid(unsafe_code)]

pub mod resize;

pub use resize::{CellMetrics, SurfaceSize, pty_size_for_surface, resize_pty_for_surface};

/// The only supported window-system backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowSystem {
    Wayland,
}
