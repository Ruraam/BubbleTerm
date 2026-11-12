//! Terminal emulation boundary. A mature VT engine will be integrated behind
//! this crate's API so application code remains independent of its internals.

#![forbid(unsafe_code)]

/// Terminal dimensions measured in character cells.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GridSize {
    pub columns: u16,
    pub rows: u16,
}

impl GridSize {
    /// Creates a grid size, rejecting empty dimensions.
    pub const fn new(columns: u16, rows: u16) -> Option<Self> {
        if columns == 0 || rows == 0 {
            None
        } else {
            Some(Self { columns, rows })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::GridSize;

    #[test]
    fn rejects_empty_grid() {
        assert!(GridSize::new(0, 24).is_none());
        assert!(GridSize::new(80, 0).is_none());
    }
}
