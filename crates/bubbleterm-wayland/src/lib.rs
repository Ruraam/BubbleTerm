//! Native Wayland client boundary. X11 is intentionally unsupported.

#![forbid(unsafe_code)]

/// The only supported window-system backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowSystem {
    Wayland,
}
