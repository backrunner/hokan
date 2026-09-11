use ratatui::style::{Color, Modifier, Style};

use super::RiskLevel;

/// Color roles for the bordered overlay. The colored theme uses a restrained
/// true-color palette when the terminal allows color; the plain theme (`color = never` or
/// `NO_COLOR`) keeps the box-drawing glyphs but drops every color, marking
/// the selected row with REVERSED instead of a background.
#[derive(Clone, Copy, Debug)]
pub struct SurfaceTheme {
    pub normal: Style,
    /// Interior-wide style of the selected row (background or REVERSED).
    pub selected: Style,
    pub border: Style,
    pub marker: Style,
    pub prefix: Style,
    pub icon: Style,
    pub icon_selected: Style,
    pub description: Style,
    /// Danger confirmation rows (the EXEC row): red tones from the shared palette.
    pub danger: Style,
    pub status: Style,
    pub hint_key: Style,
    pub hint_text: Style,
    colored: bool,
}

impl Default for SurfaceTheme {
    fn default() -> Self {
        // These values mirror the documentation preview: graphite surfaces,
        // a calm blue selection state, and amber/red only for risk. Keeping
        // the palette here makes the overlay consistent across every source.
        Self {
            normal: Style::default()
                .fg(Color::Rgb(232, 237, 245))
                .bg(Color::Rgb(20, 24, 32)),
            selected: Style::default()
                .fg(Color::Rgb(245, 248, 255))
                .bg(Color::Rgb(32, 52, 86)),
            border: Style::default()
                .fg(Color::Rgb(86, 101, 125))
                .bg(Color::Rgb(20, 24, 32)),
            marker: Style::default()
                .fg(Color::Rgb(137, 174, 255))
                .add_modifier(Modifier::BOLD),
            prefix: Style::default()
                .fg(Color::Rgb(137, 174, 255))
                .add_modifier(Modifier::BOLD),
            icon: Style::default().fg(Color::Rgb(112, 126, 150)),
            icon_selected: Style::default().fg(Color::Rgb(162, 194, 255)),
            description: Style::default().fg(Color::Rgb(156, 169, 190)),
            danger: Style::default()
                .fg(Color::Rgb(255, 126, 126))
                .add_modifier(Modifier::BOLD),
            status: Style::default().fg(Color::Rgb(243, 194, 95)),
            hint_key: Style::default()
                .fg(Color::Rgb(137, 174, 255))
                .add_modifier(Modifier::BOLD),
            hint_text: Style::default().fg(Color::Rgb(126, 141, 164)),
            colored: true,
        }
    }
}

impl SurfaceTheme {
    #[must_use]
    pub fn plain() -> Self {
        Self {
            normal: Style::default(),
            selected: Style::default().add_modifier(Modifier::REVERSED),
            border: Style::default(),
            marker: Style::default(),
            prefix: Style::default(),
            icon: Style::default(),
            icon_selected: Style::default(),
            description: Style::default(),
            danger: Style::default().add_modifier(Modifier::BOLD),
            status: Style::default(),
            hint_key: Style::default(),
            hint_text: Style::default(),
            colored: false,
        }
    }

    pub(super) fn tag(&self, kind: &str) -> Style {
        if !self.colored {
            return Style::default();
        }
        let color = match kind {
            "SPEC" => Color::Rgb(185, 148, 255),
            "HELP" => Color::Rgb(117, 183, 255),
            "HIS" => Color::Rgb(116, 207, 167),
            "FILE" => Color::Rgb(117, 183, 255),
            "PROJ" => Color::Rgb(93, 199, 212),
            "PID" | "NET" => Color::Rgb(243, 194, 95),
            "CMD" => Color::Rgb(205, 214, 229),
            "AI" | "EXEC" => Color::Rgb(255, 126, 126),
            _ => Color::Rgb(126, 141, 164),
        };
        Style::default().fg(color)
    }

    pub(super) fn risk(&self, risk: RiskLevel) -> Style {
        if !self.colored {
            return Style::default();
        }
        Style::default().fg(risk.color())
    }
}
