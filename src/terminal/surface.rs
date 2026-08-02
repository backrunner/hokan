use std::fmt;

use ratatui::{
    buffer::Buffer,
    layout::{Rect, Size},
    style::{Color, Modifier, Style},
};
use unicode_segmentation::UnicodeSegmentation;

use super::TerminalSize;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SanitizedText(String);

impl SanitizedText {
    #[must_use]
    pub fn new(raw: &str) -> Self {
        let mut output = String::with_capacity(raw.len());
        for grapheme in raw.graphemes(true) {
            for character in grapheme.chars() {
                if is_bidi_control(character) {
                    output.push_str(&format!("\\u{{{:x}}}", character as u32));
                } else if character == '\n' {
                    output.push_str("\\n");
                } else if character == '\r' {
                    output.push_str("\\r");
                } else if character == '\t' {
                    output.push_str("\\t");
                } else if character == '\u{1b}' {
                    output.push_str("\\x1b");
                } else if character.is_control() || ('\u{80}'..='\u{9f}').contains(&character) {
                    output.push_str(&format!("\\x{:02x}", character as u32));
                } else {
                    output.push(character);
                }
            }
        }
        Self(output)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl AsRef<str> for SanitizedText {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for SanitizedText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RiskLevel {
    ReadOnly,
    Low,
    Medium,
    High,
    Unknown,
}

impl RiskLevel {
    fn marker(self) -> Option<&'static str> {
        match self {
            Self::ReadOnly | Self::Low => None,
            Self::Medium => Some("~"),
            Self::High => Some("!"),
            Self::Unknown => Some("?"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverlayRow {
    pub id: u64,
    pub kind: SanitizedText,
    pub primary: SanitizedText,
    pub description: SanitizedText,
    pub annotation: Option<SanitizedText>,
    pub risk: RiskLevel,
}

impl OverlayRow {
    #[must_use]
    pub fn new(id: u64, kind: &str, primary: &str, description: &str, risk: RiskLevel) -> Self {
        Self {
            id,
            kind: SanitizedText::new(kind),
            primary: SanitizedText::new(primary),
            description: SanitizedText::new(description),
            annotation: None,
            risk,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OverlayView {
    pub rows: Vec<OverlayRow>,
    pub selected: Option<u64>,
    pub status: Option<SanitizedText>,
}

impl OverlayView {
    #[must_use]
    pub fn with_rows(rows: Vec<OverlayRow>, selected: Option<u64>) -> Self {
        Self {
            rows,
            selected,
            status: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceGeometry {
    pub rect: Rect,
    pub terminal: TerminalSize,
}

impl SurfaceGeometry {
    pub fn new(origin_row: u16, terminal: TerminalSize, height: u16) -> crate::Result<Self> {
        Self::new_with_width(origin_row, terminal, height, u16::MAX)
    }

    pub fn new_with_width(
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
        Ok(Self {
            rect: Rect::new(
                0,
                origin_row,
                (terminal.cols - 1).min(max_width.max(1)),
                height,
            ),
            terminal,
        })
    }

    #[must_use]
    pub const fn ratatui_size(self) -> Size {
        Size::new(self.rect.width, self.rect.height)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SurfaceTheme {
    pub normal: Style,
    pub selected: Style,
    pub kind: Style,
    pub description: Style,
    pub status: Style,
}

impl Default for SurfaceTheme {
    fn default() -> Self {
        Self {
            normal: Style::default().fg(Color::White),
            selected: Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(42, 48, 58))
                .add_modifier(Modifier::BOLD),
            kind: Style::default().fg(Color::Cyan),
            description: Style::default().fg(Color::DarkGray),
            status: Style::default().fg(Color::Yellow),
        }
    }
}

impl SurfaceTheme {
    #[must_use]
    pub fn plain() -> Self {
        Self {
            normal: Style::default(),
            selected: Style::default().add_modifier(Modifier::REVERSED),
            kind: Style::default(),
            description: Style::default(),
            status: Style::default(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct OverlaySurfaceRenderer {
    height: u16,
    theme: SurfaceTheme,
}

impl OverlaySurfaceRenderer {
    #[must_use]
    pub const fn new(height: u16, theme: SurfaceTheme) -> Self {
        Self { height, theme }
    }

    #[must_use]
    pub const fn height(self) -> u16 {
        self.height
    }

    pub fn render(&self, geometry: SurfaceGeometry, view: &OverlayView) -> Buffer {
        debug_assert_eq!(geometry.rect.height, self.height);
        let mut buffer = Buffer::empty(geometry.rect);
        let status_row = view.status.as_ref().map(|_| self.height.saturating_sub(1));
        let row_limit = status_row.unwrap_or(self.height) as usize;

        let remaining_width = geometry.rect.width as usize;
        let marker_width = 2;
        let kind_width = 9.min(remaining_width.saturating_sub(marker_width));
        let primary_width = if remaining_width > marker_width + kind_width + 18 {
            ((remaining_width - marker_width - kind_width) / 2).min(36)
        } else {
            remaining_width.saturating_sub(marker_width + kind_width)
        };
        let description_x = (marker_width + kind_width + primary_width + 1) as u16;

        for (index, row) in view.rows.iter().take(row_limit).enumerate() {
            let y = geometry.rect.y + index as u16;
            let selected = view.selected == Some(row.id);
            let row_style = if selected {
                self.theme.selected
            } else {
                self.theme.normal
            };
            buffer.set_style(
                Rect::new(geometry.rect.x, y, geometry.rect.width, 1),
                row_style,
            );

            let marker = match (selected, row.risk.marker()) {
                (true, Some(risk)) => format!(">{risk}"),
                (true, None) => "> ".to_owned(),
                (false, Some(risk)) => format!(" {risk}"),
                (false, None) => "  ".to_owned(),
            };
            buffer.set_stringn(geometry.rect.x, y, marker, marker_width, row_style);
            buffer.set_stringn(
                geometry.rect.x + marker_width as u16,
                y,
                format!("[{}]", row.kind),
                kind_width,
                self.theme.kind,
            );
            buffer.set_stringn(
                geometry.rect.x + marker_width as u16 + kind_width as u16,
                y,
                row.primary.as_str(),
                primary_width,
                row_style,
            );
            if description_x < geometry.rect.width {
                let separator_x = geometry.rect.x + description_x - 1;
                buffer.set_stringn(separator_x, y, " ", 1, self.theme.description);
                let suffix = match &row.annotation {
                    Some(annotation) if !annotation.is_empty() => {
                        format!("{} - {}", row.description, annotation)
                    }
                    _ => row.description.to_string(),
                };
                buffer.set_stringn(
                    geometry.rect.x + description_x,
                    y,
                    suffix,
                    geometry.rect.width.saturating_sub(description_x) as usize,
                    self.theme.description,
                );
            }
        }

        if let (Some(status), Some(status_row)) = (view.status.as_ref(), status_row) {
            let y = geometry.rect.y + status_row;
            buffer.set_style(
                Rect::new(geometry.rect.x, y, geometry.rect.width, 1),
                self.theme.normal,
            );
            buffer.set_stringn(
                geometry.rect.x,
                y,
                status.as_str(),
                geometry.rect.width as usize,
                self.theme.status,
            );
        }
        buffer
    }

    #[must_use]
    pub fn blank(&self, geometry: SurfaceGeometry) -> Buffer {
        Buffer::empty(geometry.rect)
    }
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character as u32,
        0x061c | 0x200e | 0x200f | 0x202a..=0x202e | 0x2066..=0x2069
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn geometry() -> SurfaceGeometry {
        SurfaceGeometry::new(10, TerminalSize::new(24, 80).expect("valid size"), 3)
            .expect("valid geometry")
    }

    #[test]
    fn display_text_escapes_controls_and_bidi() {
        let text = SanitizedText::new("a\n\t\x1b\u{202e}b\u{0085}");
        assert_eq!(text.as_str(), r"a\n\t\x1b\u{202e}b\x85");
        assert!(!text.as_str().chars().any(char::is_control));
    }

    #[test]
    fn geometry_never_uses_the_terminal_last_column() {
        let geometry = geometry();
        assert_eq!(geometry.rect.width, 79);
        assert_eq!(geometry.rect.right(), 79);
        assert!(geometry.rect.right() < geometry.terminal.cols);
    }

    #[test]
    fn fixed_height_surface_pads_removed_candidates() {
        let renderer = OverlaySurfaceRenderer::new(3, SurfaceTheme::default());
        let first = renderer.render(
            geometry(),
            &OverlayView::with_rows(
                vec![
                    OverlayRow::new(1, "HIS", "one", "first", RiskLevel::Low),
                    OverlayRow::new(2, "HIS", "two", "second", RiskLevel::Low),
                    OverlayRow::new(3, "HIS", "three", "third", RiskLevel::Low),
                ],
                Some(1),
            ),
        );
        let second = renderer.render(
            geometry(),
            &OverlayView::with_rows(
                vec![OverlayRow::new(1, "HIS", "one", "first", RiskLevel::Low)],
                Some(1),
            ),
        );
        assert!(first[(2, 11)].symbol() != " ");
        assert!(second[(2, 12)].symbol() == " ");
        assert_eq!(first.area(), second.area());
    }

    #[test]
    fn wide_text_is_stored_as_cells() {
        let renderer = OverlaySurfaceRenderer::new(3, SurfaceTheme::default());
        let buffer = renderer.render(
            geometry(),
            &OverlayView::with_rows(
                vec![OverlayRow::new(1, "FS", "中", "wide", RiskLevel::Low)],
                Some(1),
            ),
        );
        assert_eq!(UnicodeWidthStr::width(buffer[(11, 10)].symbol()), 2);
        assert_eq!(buffer[(12, 10)].symbol(), " ");
    }
}
