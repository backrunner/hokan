use ratatui::layout::{Rect, Size};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::super::TerminalSize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceGeometry {
    pub rect: Rect,
    pub terminal: TerminalSize,
}

impl SurfaceGeometry {
    pub fn new(origin_row: u16, terminal: TerminalSize, height: u16) -> crate::Result<Self> {
        Self::new_anchored(0, origin_row, terminal, height, u16::MAX)
    }

    pub fn new_with_width(
        origin_row: u16,
        terminal: TerminalSize,
        height: u16,
        max_width: u16,
    ) -> crate::Result<Self> {
        Self::new_anchored(0, origin_row, terminal, height, max_width)
    }

    /// Geometry anchored at `origin_col` (the edit-line cursor column): the
    /// box's left edge follows the cursor, clamped so the right edge never
    /// touches the terminal's last column.
    pub fn new_anchored(
        origin_col: u16,
        origin_row: u16,
        terminal: TerminalSize,
        height: u16,
        max_width: u16,
    ) -> crate::Result<Self> {
        if height == 0 {
            return Err(crate::Error::InvalidGeometry(
                "overlay height must be non-zero".into(),
            ));
        }
        if terminal.cols < 2 {
            return Err(crate::Error::InvalidGeometry(
                "overlay requires at least two terminal columns".into(),
            ));
        }
        let bottom = origin_row.checked_add(height).ok_or_else(|| {
            crate::Error::InvalidGeometry("overlay row arithmetic overflow".into())
        })?;
        if bottom > terminal.rows {
            return Err(crate::Error::InvalidGeometry(format!(
                "overlay {origin_row}+{height} exceeds terminal height {}",
                terminal.rows
            )));
        }
        let width = (terminal.cols - 1).min(max_width.max(1));
        let max_x = (terminal.cols - 1).saturating_sub(width);
        Ok(Self {
            rect: Rect::new(origin_col.min(max_x), origin_row, width, height),
            terminal,
        })
    }

    #[must_use]
    pub const fn ratatui_size(self) -> Size {
        Size::new(self.rect.width, self.rect.height)
    }
}

pub(super) const SIDE_PAD: usize = 1;
pub(super) const ICON_SECTION: usize = 2;
pub(super) const MARKER_WIDTH: usize = 1;
/// Reserved columns for the risk glyph between the selection marker and the
/// icon (glyph + trailing space); always reserved so columns stay aligned.
pub(super) const RISK_SLOT: usize = 2;
pub(super) const GAP: usize = 2;
pub(super) const MAX_TAG_WIDTH: usize = 8;
pub(super) const MAX_DESCRIPTION_WIDTH: usize = 24;
pub(super) const MIN_PRIMARY_WIDTH: usize = 8;
/// Smallest description column worth rendering; below this the row's primary
/// takes the whole width.
pub(super) const MIN_DESC_VISIBLE: usize = 8;
/// Dashes between the top-left corner and the pagination text (iris uses 3).
pub(super) const PAGINATION_PAD: usize = 3;
/// Dashes kept between right-aligned edge content and the right corner.
pub(super) const EDGE_TRAIL: usize = 2;

/// Display-width truncation: keeps whole characters and appends `…` when the
/// text does not fit.
pub(super) fn truncate_to_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_owned();
    }
    let mut output = String::new();
    let mut width = 0;
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width >= max_width {
            break;
        }
        width += character_width;
        output.push(character);
    }
    output.push('…');
    output
}

/// Length in bytes of the longest common char prefix of `a` and `b`.
pub(super) fn common_prefix_len(a: &str, b: &str) -> usize {
    let mut len = 0;
    for (left, right) in a.chars().zip(b.chars()) {
        if left != right {
            break;
        }
        len += left.len_utf8();
    }
    len
}
