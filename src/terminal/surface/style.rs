use ratatui::style::{Color, Modifier, Style};

use super::RiskLevel;

/// Color roles for the bordered overlay. The colored theme only uses
/// terminal-adaptive ANSI colors; the plain theme (`color = never` or
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
    /// Danger confirmation rows (the EXEC row): red tones, ANSI-adaptive.
    pub danger: Style,
    pub status: Style,
    pub hint_key: Style,
    pub hint_text: Style,
    colored: bool,
}

impl Default for SurfaceTheme {
    fn default() -> Self {
        Self {
            normal: Style::default(),
            selected: Style::default().bg(Color::DarkGray),
            border: Style::default().fg(Color::Magenta),
            marker: Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            prefix: Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            icon: Style::default().fg(Color::DarkGray),
            icon_selected: Style::default().fg(Color::Green),
            description: Style::default().fg(Color::DarkGray),
            danger: Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            status: Style::default().fg(Color::Yellow),
            hint_key: Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
            hint_text: Style::default().fg(Color::DarkGray),
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
            "SPEC" => Color::Magenta,
            "HELP" => Color::Blue,
            "HIS" => Color::Green,
            "FILE" => Color::Blue,
            "PROJ" => Color::Cyan,
            "PID" | "NET" => Color::Yellow,
            "CMD" => Color::White,
            "AI" | "EXEC" => Color::Red,
            _ => Color::DarkGray,
        };
        Style::default().fg(color).add_modifier(Modifier::DIM)
    }

    pub(super) fn risk(&self, risk: RiskLevel) -> Style {
        if !self.colored {
            return Style::default();
        }
        Style::default().fg(risk.color())
    }
}
