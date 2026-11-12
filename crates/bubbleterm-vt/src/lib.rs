//! Terminal emulation adapter and owned render snapshots.
//!
//! VT parsing is delegated to the maintained `vt100` engine. This crate owns
//! BubbleTerm's stable grid/snapshot API so the renderer never depends on the
//! parser's internal representation.

#![forbid(unsafe_code)]

use std::num::NonZeroU16;

/// Maximum number of live cells accepted by one terminal grid.
pub const MAX_GRID_CELLS: usize = 1_048_576;

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

    fn cell_count(self) -> usize {
        usize::from(self.columns) * usize::from(self.rows)
    }
}

/// Failure to create or resize a terminal grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalError {
    EmptyGrid,
    GridTooLarge,
}

impl std::fmt::Display for TerminalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyGrid => formatter.write_str("terminal grid dimensions must be non-zero"),
            Self::GridTooLarge => formatter.write_str("terminal grid exceeds the cell limit"),
        }
    }
}

impl std::error::Error for TerminalError {}

/// Terminal color preserved by the VT adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalColor {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl From<vt100::Color> for TerminalColor {
    fn from(color: vt100::Color) -> Self {
        match color {
            vt100::Color::Default => Self::Default,
            vt100::Color::Idx(index) => Self::Indexed(index),
            vt100::Color::Rgb(red, green, blue) => Self::Rgb(red, green, blue),
        }
    }
}

/// Owned cell data suitable for rendering without borrowing the VT parser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalCell {
    pub text: String,
    pub foreground: TerminalColor,
    pub background: TerminalColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub wide: bool,
    pub wide_continuation: bool,
}

/// Immutable, owned view of the visible terminal grid.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GridSnapshot {
    pub size: GridSize,
    pub cells: Vec<TerminalCell>,
    pub cursor_row: u16,
    pub cursor_column: u16,
    pub cursor_visible: bool,
    pub alternate_screen: bool,
}

impl GridSnapshot {
    /// Returns a cell by row and column.
    pub fn cell(&self, row: u16, column: u16) -> Option<&TerminalCell> {
        if row >= self.size.rows || column >= self.size.columns {
            return None;
        }
        let index = usize::from(row) * usize::from(self.size.columns) + usize::from(column);
        self.cells.get(index)
    }
}

/// VT parser plus BubbleTerm's renderer-facing terminal API.
pub struct Terminal {
    parser: vt100::Parser,
    size: GridSize,
}

impl Terminal {
    /// Creates a terminal with bounded dimensions and scrollback capacity.
    pub fn new(size: GridSize, scrollback_capacity: usize) -> Result<Self, TerminalError> {
        Self::validate_size(size)?;
        let rows = NonZeroU16::new(size.rows).ok_or(TerminalError::EmptyGrid)?;
        let columns = NonZeroU16::new(size.columns).ok_or(TerminalError::EmptyGrid)?;
        Ok(Self {
            parser: vt100::Parser::new(rows, columns, scrollback_capacity),
            size,
        })
    }

    fn validate_size(size: GridSize) -> Result<(), TerminalError> {
        if size.columns == 0 || size.rows == 0 {
            return Err(TerminalError::EmptyGrid);
        }
        if size.cell_count() > MAX_GRID_CELLS {
            return Err(TerminalError::GridTooLarge);
        }
        Ok(())
    }

    /// Feeds an arbitrary PTY byte chunk into the VT parser.
    ///
    /// Chunk boundaries are intentionally irrelevant: escape sequences and
    /// UTF-8 characters may be split across successive calls.
    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Resizes the parser while preserving its supported scrollback state.
    pub fn resize(&mut self, size: GridSize) -> Result<(), TerminalError> {
        Self::validate_size(size)?;
        let rows = NonZeroU16::new(size.rows).ok_or(TerminalError::EmptyGrid)?;
        let columns = NonZeroU16::new(size.columns).ok_or(TerminalError::EmptyGrid)?;
        self.parser.set_size(rows, columns);
        self.size = size;
        Ok(())
    }

    /// Returns an owned snapshot of all visible cells and cursor state.
    pub fn snapshot(&self) -> GridSnapshot {
        let screen = self.parser.screen();
        let (cursor_row, cursor_column) = screen.cursor_position();
        let mut cells = Vec::with_capacity(self.size.cell_count());

        for row in 0..self.size.rows {
            for column in 0..self.size.columns {
                let cell = screen.cell(row, column);
                cells.push(match cell {
                    Some(cell) => TerminalCell {
                        text: cell.contents().to_owned(),
                        foreground: cell.fgcolor().into(),
                        background: cell.bgcolor().into(),
                        bold: cell.bold(),
                        dim: cell.dim(),
                        italic: cell.italic(),
                        underline: cell.underline(),
                        inverse: cell.inverse(),
                        wide: cell.is_wide(),
                        wide_continuation: cell.is_wide_continuation(),
                    },
                    None => TerminalCell {
                        text: String::new(),
                        foreground: TerminalColor::Default,
                        background: TerminalColor::Default,
                        bold: false,
                        dim: false,
                        italic: false,
                        underline: false,
                        inverse: false,
                        wide: false,
                        wide_continuation: false,
                    },
                });
            }
        }

        GridSnapshot {
            size: self.size,
            cells,
            cursor_row,
            cursor_column,
            cursor_visible: !screen.hide_cursor(),
            alternate_screen: screen.alternate_screen(),
        }
    }

    /// Returns the current grid dimensions.
    pub const fn size(&self) -> GridSize {
        self.size
    }

    /// Reports whether the child requested application cursor mode.
    pub fn application_cursor(&self) -> bool {
        self.parser.screen().application_cursor()
    }

    /// Reports whether the child requested bracketed paste mode.
    pub fn bracketed_paste(&self) -> bool {
        self.parser.screen().bracketed_paste()
    }
}

#[cfg(test)]
mod tests {
    use super::{GridSize, MAX_GRID_CELLS, Terminal, TerminalColor, TerminalError};

    fn terminal(columns: u16, rows: u16) -> Terminal {
        Terminal::new(GridSize::new(columns, rows).unwrap(), 100)
            .expect("terminal grid should be valid")
    }

    #[test]
    fn rejects_empty_grid() {
        assert!(GridSize::new(0, 24).is_none());
        assert!(GridSize::new(80, 0).is_none());
        assert_eq!(
            Terminal::new(GridSize { columns: 0, rows: 24 }, 0).err(),
            Some(TerminalError::EmptyGrid)
        );
    }

    #[test]
    fn rejects_excessively_large_grid() {
        assert_eq!(
            Terminal::new(
                GridSize {
                    columns: u16::MAX,
                    rows: 17,
                },
                0
            )
            .err(),
            Some(TerminalError::GridTooLarge)
        );
    }

    #[test]
    fn parses_plain_text_and_split_utf8() {
        let mut terminal = terminal(12, 2);
        terminal.process(b"hello ");
        terminal.process(&[0xc3]);
        terminal.process(&[0xa9]);

        let snapshot = terminal.snapshot();
        assert_eq!(snapshot.cell(0, 0).unwrap().text, "h");
        assert_eq!(snapshot.cell(0, 6).unwrap().text, "é");
        assert_eq!(snapshot.size, GridSize { columns: 12, rows: 2 });
    }

    #[test]
    fn parses_sgr_colors_and_attributes() {
        let mut terminal = terminal(20, 2);
        terminal.process(b"\x1b[1;3;4;38;2;12;34;56mX");

        let cell = terminal.snapshot().cell(0, 0).unwrap().clone();
        assert_eq!(cell.text, "X");
        assert_eq!(cell.foreground, TerminalColor::Rgb(12, 34, 56));
        assert!(cell.bold);
        assert!(cell.italic);
        assert!(cell.underline);
    }

    #[test]
    fn parses_cursor_visibility_and_application_mode() {
        let mut terminal = terminal(20, 2);
        terminal.process(b"\x1b[?25l\x1b[?1h\x1b[2;4H");

        let snapshot = terminal.snapshot();
        assert!(!snapshot.cursor_visible);
        assert!(terminal.application_cursor());
        assert_eq!((snapshot.cursor_row, snapshot.cursor_column), (1, 3));
    }

    #[test]
    fn resize_updates_dimensions_and_preserves_content() {
        let mut terminal = terminal(8, 2);
        terminal.process(b"bubble");
        terminal
            .resize(GridSize::new(12, 3).unwrap())
            .expect("resize should succeed");

        let snapshot = terminal.snapshot();
        assert_eq!(snapshot.size, GridSize { columns: 12, rows: 3 });
        assert_eq!(snapshot.cell(0, 0).unwrap().text, "b");
        assert_eq!(snapshot.cells.len(), 36);
    }

    #[test]
    fn resize_rejects_grid_over_limit_without_mutating_state() {
        let mut terminal = terminal(8, 2);
        let result = terminal.resize(GridSize {
            columns: 65_535,
            rows: 65_535,
        });
        assert_eq!(result, Err(TerminalError::GridTooLarge));
        assert_eq!(terminal.size(), GridSize { columns: 8, rows: 2 });
    }
}
