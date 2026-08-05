use std::fmt;

use ratatui::style::Color;
use unicode_segmentation::UnicodeSegmentation;

mod layout;
mod render;
mod style;
#[cfg(test)]
mod tests;

pub use layout::SurfaceGeometry;
pub use render::OverlaySurfaceRenderer;
pub use style::SurfaceTheme;

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
    pub(super) fn marker(self) -> Option<&'static str> {
        match self {
            Self::ReadOnly | Self::Low => None,
            Self::Medium => Some("~"),
            Self::High => Some("!"),
            Self::Unknown => Some("?"),
        }
    }

    pub(super) fn color(self) -> Color {
        match self {
            Self::Medium => Color::Yellow,
            Self::High | Self::Unknown => Color::Red,
            Self::ReadOnly | Self::Low => Color::Reset,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverlayRow {
    pub id: u64,
    /// Source tag text (SPEC, HIS, FILE, …) rendered right-aligned, unbracketed.
    pub kind: SanitizedText,
    pub primary: SanitizedText,
    pub description: SanitizedText,
    pub annotation: Option<SanitizedText>,
    pub risk: RiskLevel,
    /// Nerd Font glyph for the row, resolved from the first command word.
    pub icon: Option<&'static str>,
    /// Render the primary and description in the theme's danger role (red
    /// tones when colored); used by the danger-confirmation EXEC row.
    pub danger: bool,
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
            icon: None,
            danger: false,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OverlayView {
    pub rows: Vec<OverlayRow>,
    pub selected: Option<u64>,
    /// Status text embedded in the bottom border instead of the key hints.
    pub status: Option<SanitizedText>,
    /// `(position, total)` pagination embedded in the top border.
    pub pagination: Option<(usize, usize)>,
    /// Typed text used to highlight the matching prefix of each primary.
    pub highlight: Option<SanitizedText>,
}

impl OverlayView {
    #[must_use]
    pub fn with_rows(rows: Vec<OverlayRow>, selected: Option<u64>) -> Self {
        Self {
            rows,
            selected,
            status: None,
            pagination: None,
            highlight: None,
        }
    }
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character as u32,
        0x061c | 0x200e | 0x200f | 0x202a..=0x202e | 0x2066..=0x2069
    )
}
