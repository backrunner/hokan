use std::fmt;

use ratatui::{
    buffer::Buffer,
    layout::{Rect, Size},
    style::{Color, Modifier, Style},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{TerminalSize, icons, icons::FALLBACK_GLYPH};

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

    fn color(self) -> Color {
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

    fn tag(&self, kind: &str) -> Style {
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

    fn risk(&self, risk: RiskLevel) -> Style {
        if !self.colored {
            return Style::default();
        }
        Style::default().fg(risk.color())
    }
}

const SIDE_PAD: usize = 1;
const ICON_SECTION: usize = 2;
const MARKER_WIDTH: usize = 1;
/// Reserved columns for the risk glyph between the selection marker and the
/// icon (glyph + trailing space); always reserved so columns stay aligned.
const RISK_SLOT: usize = 2;
const GAP: usize = 2;
const MAX_TAG_WIDTH: usize = 8;
const MAX_DESCRIPTION_WIDTH: usize = 24;
const MIN_PRIMARY_WIDTH: usize = 8;
/// Smallest description column worth rendering; below this the row's primary
/// takes the whole width.
const MIN_DESC_VISIBLE: usize = 8;
/// Dashes between the top-left corner and the pagination text (iris uses 3).
const PAGINATION_PAD: usize = 3;
/// Dashes kept between right-aligned edge content and the right corner.
const EDGE_TRAIL: usize = 2;

#[derive(Clone, Copy, Debug)]
pub struct OverlaySurfaceRenderer {
    height: u16,
    theme: SurfaceTheme,
    icons: bool,
}

impl OverlaySurfaceRenderer {
    #[must_use]
    pub const fn new(height: u16, theme: SurfaceTheme, icons: bool) -> Self {
        Self {
            height,
            theme,
            icons,
        }
    }

    #[must_use]
    pub const fn height(self) -> u16 {
        self.height
    }

    pub fn render(&self, geometry: SurfaceGeometry, view: &OverlayView) -> Buffer {
        debug_assert_eq!(geometry.rect.height, self.height);
        let mut buffer = Buffer::empty(geometry.rect);
        let width = geometry.rect.width as usize;
        if geometry.rect.height < 2 || width < 3 {
            return buffer;
        }
        let inner = width - 2;
        let top_y = geometry.rect.y;
        let bottom_y = geometry.rect.y + geometry.rect.height - 1;
        let x = geometry.rect.x;

        let pagination = view
            .pagination
            .map(|(position, total)| truncate_to_width(&format!(" {position}/{total} "), inner));
        let top_content: Vec<(&str, Style)> = pagination
            .iter()
            .map(|text| (text.as_str(), self.theme.hint_text))
            .collect();
        self.render_edge(&mut buffer, top_y, "╭", "╮", &top_content, PAGINATION_PAD);

        let bottom_content: Vec<(String, Style)> = if let Some(status) = &view.status {
            vec![(
                truncate_to_width(status.as_str(), inner.saturating_sub(EDGE_TRAIL)),
                self.theme.status,
            )]
        } else {
            self.hint_segments(inner)
                .into_iter()
                .map(|(text, style)| (text.to_owned(), style))
                .collect()
        };
        let bottom_refs: Vec<(&str, Style)> = bottom_content
            .iter()
            .map(|(text, style)| (text.as_str(), *style))
            .collect();
        let bottom_width: usize = bottom_refs
            .iter()
            .map(|(text, _)| UnicodeWidthStr::width(*text))
            .sum();
        let bottom_pad = inner.saturating_sub(bottom_width + EDGE_TRAIL);
        self.render_edge(&mut buffer, bottom_y, "╰", "╯", &bottom_refs, bottom_pad);

        let row_capacity = usize::from(geometry.rect.height - 2);
        let page_rows = &view.rows[..view.rows.len().min(row_capacity)];
        let shared_primary_w = self.shared_primary_width(width, page_rows);
        for (index, row) in page_rows.iter().enumerate() {
            let y = top_y + 1 + index as u16;
            self.render_row(
                &mut buffer,
                y,
                row,
                view.selected == Some(row.id),
                view,
                shared_primary_w,
            );
        }
        for index in view.rows.len().min(row_capacity)..row_capacity {
            let y = top_y + 1 + index as u16;
            buffer.set_stringn(x, y, "│", 1, self.theme.border);
            buffer.set_stringn(x + width as u16 - 1, y, "│", 1, self.theme.border);
        }
        buffer
    }

    fn render_row(
        &self,
        buffer: &mut Buffer,
        y: u16,
        row: &OverlayRow,
        selected: bool,
        view: &OverlayView,
        shared_primary_w: Option<usize>,
    ) {
        let rect = buffer.area();
        let x = rect.x;
        let width = rect.width as usize;
        let inner = width - 2;
        buffer.set_stringn(x, y, "│", 1, self.theme.border);
        buffer.set_stringn(x + width as u16 - 1, y, "│", 1, self.theme.border);
        let row_style = if selected {
            self.theme.selected
        } else {
            self.theme.normal
        };
        buffer.set_style(Rect::new(x + 1, y, inner as u16, 1), row_style);

        let icons_w = if self.icons { ICON_SECTION } else { 0 };
        let left_w = SIDE_PAD + MARKER_WIDTH + 1 + RISK_SLOT + icons_w;
        let (tag, tag_w) = self.tag_display(row.kind.as_str());
        // +1 keeps a visible gap between the description and the source tag.
        let right_w = SIDE_PAD + tag_w + 1;
        let avail = inner.saturating_sub(left_w + right_w);

        let description = match &row.annotation {
            Some(annotation) if !annotation.is_empty() => {
                format!("{} - {}", row.description, annotation)
            }
            _ => row.description.to_string(),
        };
        let description_natural_w = UnicodeWidthStr::width(description.as_str());
        let (primary_w, description_w) = if description_natural_w == 0 {
            (avail, 0)
        } else {
            match shared_primary_w {
                Some(shared) if avail >= MIN_PRIMARY_WIDTH + GAP + MIN_DESC_VISIBLE => {
                    let primary_w = shared.min(avail - GAP - MIN_DESC_VISIBLE);
                    let description_w = (avail - GAP - primary_w).min(MAX_DESCRIPTION_WIDTH);
                    (primary_w, description_w)
                }
                _ => (avail, 0),
            }
        };

        let mut cursor_x = x + 1;
        buffer.set_stringn(cursor_x, y, " ", SIDE_PAD, row_style);
        cursor_x += SIDE_PAD as u16;
        let (marker, marker_style) = if selected {
            ("▶", self.theme.marker)
        } else {
            (" ", row_style)
        };
        buffer.set_stringn(cursor_x, y, marker, MARKER_WIDTH, marker_style);
        cursor_x += MARKER_WIDTH as u16 + 1;
        if let Some(risk_marker) = row.risk.marker() {
            buffer.set_stringn(
                cursor_x,
                y,
                risk_marker,
                RISK_SLOT - 1,
                self.theme.risk(row.risk),
            );
        }
        cursor_x += RISK_SLOT as u16;
        if self.icons {
            let glyph = row.icon.unwrap_or(FALLBACK_GLYPH);
            let icon_style = if selected {
                self.theme.icon_selected
            } else {
                self.theme.icon
            };
            buffer.set_stringn(cursor_x, y, glyph, ICON_SECTION, icon_style);
            cursor_x += ICON_SECTION as u16;
        }

        let primary = truncate_to_width(row.primary.as_str(), primary_w);
        let primary_style = if row.danger {
            self.theme.danger
        } else if selected {
            self.theme.normal.add_modifier(Modifier::BOLD)
        } else {
            self.theme.normal
        };
        let highlight = view.highlight.as_ref().map_or("", SanitizedText::as_str);
        let matched = common_prefix_len(&primary, highlight);
        if matched > 0 {
            let (head, tail) = primary.split_at(matched);
            let head_w = UnicodeWidthStr::width(head);
            buffer.set_stringn(cursor_x, y, head, primary_w, self.theme.prefix);
            buffer.set_stringn(
                cursor_x + head_w as u16,
                y,
                tail,
                primary_w.saturating_sub(head_w),
                primary_style,
            );
        } else {
            buffer.set_stringn(cursor_x, y, primary.as_str(), primary_w, primary_style);
        }

        if description_w > 0 {
            let description_x = x + 1 + (left_w + primary_w + GAP) as u16;
            buffer.set_stringn(
                description_x,
                y,
                truncate_to_width(&description, description_w),
                description_w,
                if row.danger {
                    self.theme.danger
                } else {
                    self.theme.description
                },
            );
        }

        let tag_x = x + (width - 1 - SIDE_PAD - tag_w) as u16;
        buffer.set_stringn(
            tag_x,
            y,
            tag.as_str(),
            tag_w,
            self.theme.tag(row.kind.as_str()),
        );
    }

    /// Source tag text and its display width: a Nerd Font glyph when icons
    /// are enabled, the ASCII label otherwise.
    fn tag_display(&self, kind: &str) -> (String, usize) {
        if self.icons {
            (icons::source_glyph(kind).to_owned(), 1)
        } else {
            let tag = truncate_to_width(kind, MAX_TAG_WIDTH);
            let width = UnicodeWidthStr::width(tag.as_str());
            (tag, width)
        }
    }

    /// Shared primary column width for rows that show a description, so the
    /// description column lines up across the whole page (iris achieves the
    /// same with a fixed title width). Rows without a description always use
    /// the full available width for their primary.
    fn shared_primary_width(&self, width: usize, rows: &[OverlayRow]) -> Option<usize> {
        let inner = width.saturating_sub(2);
        let icons_w = if self.icons { ICON_SECTION } else { 0 };
        let left_w = SIDE_PAD + MARKER_WIDTH + 1 + RISK_SLOT + icons_w;
        let mut shared: Option<usize> = None;
        for row in rows {
            let has_description = !row.description.is_empty()
                || row
                    .annotation
                    .as_ref()
                    .is_some_and(|annotation| !annotation.is_empty());
            if !has_description {
                continue;
            }
            let (_, tag_w) = self.tag_display(row.kind.as_str());
            let avail = inner.saturating_sub(left_w + SIDE_PAD + tag_w + 1);
            if avail < MIN_PRIMARY_WIDTH + GAP + MIN_DESC_VISIBLE {
                continue;
            }
            let natural =
                UnicodeWidthStr::width(row.primary.as_str()).min(avail - GAP - MIN_DESC_VISIBLE);
            shared = Some(shared.map_or(natural, |current| current.max(natural)));
        }
        shared.map(|width| width.max(MIN_PRIMARY_WIDTH))
    }

    /// One horizontal box edge with optional styled `content` embedded in the
    /// dash run, starting `left_pad` columns after the left corner.
    fn render_edge(
        &self,
        buffer: &mut Buffer,
        y: u16,
        left: &str,
        right: &str,
        content: &[(&str, Style)],
        left_pad: usize,
    ) {
        let rect = buffer.area();
        let x = rect.x;
        let width = rect.width as usize;
        let inner = width - 2;
        buffer.set_stringn(x, y, left, 1, self.theme.border);
        buffer.set_stringn(x + width as u16 - 1, y, right, 1, self.theme.border);
        buffer.set_stringn(x + 1, y, "─".repeat(inner), inner, self.theme.border);
        let content_w: usize = content
            .iter()
            .map(|(text, _)| UnicodeWidthStr::width(*text))
            .sum();
        if content_w == 0 || content_w > inner {
            return;
        }
        let mut cursor_x = x + 1 + left_pad.min(inner - content_w) as u16;
        for (text, style) in content {
            buffer.set_stringn(cursor_x, y, *text, content_w, *style);
            cursor_x += UnicodeWidthStr::width(*text) as u16;
        }
    }

    /// ` Tab 回填 · Enter 执行 · Esc 关闭 ` — key names in `hint_key`, labels
    /// and separators in `hint_text`. Empty when the box is too narrow.
    fn hint_segments(&self, inner: usize) -> Vec<(&'static str, Style)> {
        let segments: Vec<(&'static str, Style)> = vec![
            (" ", self.theme.hint_text),
            ("Tab", self.theme.hint_key),
            (" 回填 · ", self.theme.hint_text),
            ("Enter", self.theme.hint_key),
            (" 执行 · ", self.theme.hint_text),
            ("Esc", self.theme.hint_key),
            (" 关闭 ", self.theme.hint_text),
        ];
        let width: usize = segments
            .iter()
            .map(|(text, _)| UnicodeWidthStr::width(*text))
            .sum();
        if width + EDGE_TRAIL > inner {
            Vec::new()
        } else {
            segments
        }
    }

    #[must_use]
    pub fn blank(&self, geometry: SurfaceGeometry) -> Buffer {
        Buffer::empty(geometry.rect)
    }
}

/// Display-width truncation: keeps whole characters and appends `…` when the
/// text does not fit.
fn truncate_to_width(text: &str, max_width: usize) -> String {
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
fn common_prefix_len(a: &str, b: &str) -> usize {
    let mut len = 0;
    for (left, right) in a.chars().zip(b.chars()) {
        if left != right {
            break;
        }
        len += left.len_utf8();
    }
    len
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
    use crate::terminal::icons::lookup_icon;

    fn geometry() -> SurfaceGeometry {
        SurfaceGeometry::new(10, TerminalSize::new(24, 80).expect("valid size"), 3)
            .expect("valid geometry")
    }

    fn row_text(buffer: &Buffer, y: u16) -> String {
        let area = buffer.area();
        let mut text = String::new();
        for x in area.x..area.x + area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text
    }

    fn rows() -> Vec<OverlayRow> {
        let mut first = OverlayRow::new(
            1,
            "SPEC",
            "git checkout -- .",
            "switch paths",
            RiskLevel::Low,
        );
        first.icon = Some(lookup_icon("git"));
        let mut second = OverlayRow::new(2, "HIS", "git commit -m \"wip\"", "", RiskLevel::Low);
        second.icon = Some(lookup_icon("git"));
        vec![first, second]
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
    fn anchored_geometry_follows_the_cursor_but_never_overflows() {
        let size = TerminalSize::new(24, 80).expect("valid size");
        let geometry = SurfaceGeometry::new_anchored(10, 4, size, 3, 40).expect("geometry");
        assert_eq!(geometry.rect.x, 10);
        assert_eq!(geometry.rect.width, 40);
        let clamped = SurfaceGeometry::new_anchored(70, 4, size, 3, 40).expect("geometry");
        assert_eq!(clamped.rect.x, 39);
        assert_eq!(clamped.rect.right(), 79);
        assert!(clamped.rect.right() < clamped.terminal.cols);
    }

    #[test]
    fn renders_rounded_borders_hints_and_right_aligned_tags() {
        let renderer = OverlaySurfaceRenderer::new(4, SurfaceTheme::default(), true);
        let geometry = SurfaceGeometry::new_with_width(
            10,
            TerminalSize::new(24, 80).expect("valid size"),
            4,
            60,
        )
        .expect("valid geometry");
        let buffer = renderer.render(geometry, &OverlayView::with_rows(rows(), Some(1)));
        let top = row_text(&buffer, 10);
        assert!(
            top.starts_with('╭') && top.trim_end().ends_with('╮'),
            "{top}"
        );
        let first = row_text(&buffer, 11);
        assert!(first.starts_with('│'), "{first}");
        assert!(first.contains('▶'), "{first}");
        assert!(first.contains("git checkout"), "{first}");
        assert!(first.trim_end().ends_with("\u{f02d} │"), "{first}");
        let bottom = row_text(&buffer, 13);
        assert!(bottom.starts_with('╰'), "{bottom}");
        assert!(bottom.contains("Tab"), "{bottom}");
        assert!(bottom.contains("Enter"), "{bottom}");
        assert!(bottom.contains("Esc"), "{bottom}");
        assert!(bottom.trim_end().ends_with("──╯"), "{bottom}");
    }

    #[test]
    fn status_replaces_the_hint_footer_and_pagination_marks_the_top_edge() {
        let renderer = OverlaySurfaceRenderer::new(4, SurfaceTheme::default(), true);
        let geometry = SurfaceGeometry::new_with_width(
            10,
            TerminalSize::new(24, 80).expect("valid size"),
            4,
            60,
        )
        .expect("valid geometry");
        let mut view = OverlayView::with_rows(rows(), Some(2));
        view.status = Some(SanitizedText::new("HK-CMP-STALE"));
        view.pagination = Some((2, 20));
        let buffer = renderer.render(geometry, &view);
        let top = row_text(&buffer, 10);
        assert!(top.contains(" 2/20 "), "{top}");
        assert!(top.find("2/20").is_some_and(|index| index >= 4), "{top}");
        let bottom = row_text(&buffer, 13);
        assert!(bottom.contains("HK-CMP-STALE"), "{bottom}");
        assert!(!bottom.contains("Tab"), "{bottom}");
    }

    #[test]
    fn risk_marker_sits_next_to_the_selection_marker() {
        let renderer = OverlaySurfaceRenderer::new(4, SurfaceTheme::default(), true);
        let geometry = SurfaceGeometry::new_with_width(
            10,
            TerminalSize::new(24, 80).expect("valid size"),
            4,
            60,
        )
        .expect("valid geometry");
        let view = OverlayView::with_rows(
            vec![OverlayRow::new(
                1,
                "CMD",
                "rm -rf /tmp/x",
                "danger",
                RiskLevel::High,
            )],
            Some(1),
        );
        let buffer = renderer.render(geometry, &view);
        let row = row_text(&buffer, 11);
        assert!(row.starts_with("│ ▶ ! "), "{row}");
        assert!(row.trim_end().ends_with("\u{f120} │"), "{row}");
    }

    #[test]
    fn description_column_is_aligned_across_rows() {
        let renderer = OverlaySurfaceRenderer::new(5, SurfaceTheme::default(), true);
        let geometry = SurfaceGeometry::new_with_width(
            10,
            TerminalSize::new(24, 80).expect("valid size"),
            5,
            60,
        )
        .expect("valid geometry");
        let view = OverlayView::with_rows(
            vec![
                OverlayRow::new(1, "HIS", "ls", "list files", RiskLevel::Low),
                OverlayRow::new(2, "SPEC", "ls -lah", "long listing", RiskLevel::ReadOnly),
                OverlayRow::new(3, "FILE", "script.sh", "shell script", RiskLevel::Low),
            ],
            None,
        );
        let buffer = renderer.render(geometry, &view);
        let column_of = |y: u16, needle: &str| {
            let text = row_text(&buffer, y);
            text[..text.find(needle).expect("description text")]
                .chars()
                .count()
        };
        let first = column_of(11, "list files");
        assert_eq!(first, column_of(12, "long listing"));
        assert_eq!(first, column_of(13, "shell script"));
    }

    #[test]
    fn selected_row_primary_is_bold() {
        let renderer = OverlaySurfaceRenderer::new(4, SurfaceTheme::default(), true);
        let geometry = SurfaceGeometry::new_with_width(
            10,
            TerminalSize::new(24, 80).expect("valid size"),
            4,
            60,
        )
        .expect("valid geometry");
        let buffer = renderer.render(geometry, &OverlayView::with_rows(rows(), Some(1)));
        let area = buffer.area();
        let text = row_text(&buffer, 11);
        let byte_offset = text.find("git checkout").expect("primary text");
        let offset = text[..byte_offset].chars().count() as u16;
        assert!(
            buffer[(area.x + offset, 11)]
                .modifier
                .contains(Modifier::BOLD)
        );
        let unselected = row_text(&buffer, 12);
        let unselected_offset = unselected[..unselected.find("git commit").expect("primary")]
            .chars()
            .count() as u16;
        assert!(
            !buffer[(area.x + unselected_offset, 12)]
                .modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn nerd_fonts_off_leaves_no_icon_gap() {
        let renderer = OverlaySurfaceRenderer::new(4, SurfaceTheme::default(), false);
        let geometry = SurfaceGeometry::new_with_width(
            10,
            TerminalSize::new(24, 80).expect("valid size"),
            4,
            60,
        )
        .expect("valid geometry");
        let buffer = renderer.render(geometry, &OverlayView::with_rows(rows(), Some(1)));
        let row = row_text(&buffer, 11);
        // Risk slot stays reserved even with icons off, keeping columns aligned.
        assert!(row.starts_with("│ ▶   git checkout"), "{row}");
        let iconless = row_text(&buffer, 12);
        assert!(iconless.starts_with("│     git commit"), "{iconless}");
    }

    #[test]
    fn typed_prefix_is_highlighted_green_and_bold() {
        let renderer = OverlaySurfaceRenderer::new(4, SurfaceTheme::default(), true);
        let geometry = SurfaceGeometry::new_with_width(
            10,
            TerminalSize::new(24, 80).expect("valid size"),
            4,
            60,
        )
        .expect("valid geometry");
        let mut view = OverlayView::with_rows(rows(), Some(1));
        view.highlight = Some(SanitizedText::new("git che"));
        let buffer = renderer.render(geometry, &view);
        let area = buffer.area();
        let row_start = row_text(&buffer, 11);
        let byte_offset = row_start.find("git che").expect("primary text");
        let offset = row_start[..byte_offset].chars().count() as u16;
        for index in 0..7 {
            let cell = &buffer[(area.x + offset + index, 11)];
            assert_eq!(cell.fg, Color::Green);
            assert!(cell.modifier.contains(Modifier::BOLD));
        }
        let tail = &buffer[(area.x + offset + 7, 11)];
        assert_ne!(tail.fg, Color::Green);
    }

    #[test]
    fn selected_row_background_spans_the_interior() {
        let renderer = OverlaySurfaceRenderer::new(4, SurfaceTheme::default(), true);
        let geometry = SurfaceGeometry::new_with_width(
            10,
            TerminalSize::new(24, 80).expect("valid size"),
            4,
            60,
        )
        .expect("valid geometry");
        let buffer = renderer.render(geometry, &OverlayView::with_rows(rows(), Some(1)));
        let area = buffer.area();
        for x in area.x + 1..area.x + area.width - 1 {
            assert_eq!(buffer[(x, 11)].bg, Color::DarkGray, "column {x}");
        }
        assert_ne!(buffer[(area.x + 1, 12)].bg, Color::DarkGray);
        let plain = OverlaySurfaceRenderer::new(4, SurfaceTheme::plain(), true);
        let plain_buffer = plain.render(geometry, &OverlayView::with_rows(rows(), Some(1)));
        assert!(
            plain_buffer[(area.x + 1, 11)]
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(plain_buffer[(area.x, 11)].fg, Color::Reset);
    }

    #[test]
    fn fixed_height_surface_pads_removed_candidates() {
        let renderer = OverlaySurfaceRenderer::new(4, SurfaceTheme::default(), true);
        let geometry = SurfaceGeometry::new_with_width(
            10,
            TerminalSize::new(24, 80).expect("valid size"),
            4,
            60,
        )
        .expect("valid geometry");
        let buffer = renderer.render(
            geometry,
            &OverlayView::with_rows(
                vec![OverlayRow::new(1, "HIS", "one", "first", RiskLevel::Low)],
                Some(1),
            ),
        );
        // The unused second row keeps its borders and a blank interior, so a
        // shrinking candidate list never leaves stale cells behind.
        let padded = row_text(&buffer, 12);
        assert!(padded.starts_with('│'), "{padded}");
        assert!(padded.trim_end().ends_with('│'), "{padded}");
        let cell_count = padded.chars().count();
        assert!(
            padded
                .chars()
                .skip(1)
                .take(cell_count.saturating_sub(2))
                .all(|character| character == ' '),
            "{padded}"
        );
        assert_eq!(*buffer.area(), geometry.rect);
    }

    #[test]
    fn wide_text_is_stored_as_cells() {
        let renderer = OverlaySurfaceRenderer::new(3, SurfaceTheme::default(), true);
        let buffer = renderer.render(
            geometry(),
            &OverlayView::with_rows(
                vec![OverlayRow::new(1, "FILE", "中文", "wide", RiskLevel::Low)],
                Some(1),
            ),
        );
        let row = row_text(&buffer, 11);
        let byte_offset = row.find('中').expect("wide glyph");
        let offset = row[..byte_offset].chars().count() as u16;
        assert_eq!(UnicodeWidthStr::width(buffer[(offset, 11)].symbol()), 2);
        assert_eq!(buffer[(offset + 1, 11)].symbol(), " ");
    }

    #[test]
    fn danger_row_uses_red_tones_and_the_exec_tag() {
        let renderer = OverlaySurfaceRenderer::new(4, SurfaceTheme::default(), true);
        let geometry = SurfaceGeometry::new_with_width(
            10,
            TerminalSize::new(24, 80).expect("valid size"),
            4,
            60,
        )
        .expect("valid geometry");
        let mut row = OverlayRow::new(
            1,
            "EXEC",
            "rm -rf /tmp/x",
            "destructive command · recursive operation",
            RiskLevel::High,
        );
        row.danger = true;
        let buffer = renderer.render(geometry, &OverlayView::with_rows(vec![row], None));
        let text = row_text(&buffer, 11);
        assert!(text.contains("rm -rf /tmp/x"), "{text}");
        assert!(text.contains("! ❯ rm -rf /tmp/x"), "{text}");
        assert!(text.trim_end().ends_with("\u{f071} │"), "{text}");
        let area = buffer.area();
        let byte_offset = text.find("rm -rf").expect("primary text");
        let offset = row_text(&buffer, 11)[..byte_offset].chars().count() as u16;
        let cell = &buffer[(area.x + offset, 11)];
        assert_eq!(cell.fg, Color::Red);
        assert!(cell.modifier.contains(Modifier::BOLD));

        let plain = OverlaySurfaceRenderer::new(4, SurfaceTheme::plain(), true);
        let plain_buffer = plain.render(
            geometry,
            &OverlayView::with_rows(
                vec![{
                    let mut row = OverlayRow::new(1, "EXEC", "rm -rf /tmp/x", "", RiskLevel::High);
                    row.danger = true;
                    row
                }],
                None,
            ),
        );
        let plain_cell = &plain_buffer[(area.x + offset, 11)];
        assert_eq!(plain_cell.fg, Color::Reset);
        assert!(plain_cell.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn truncation_marks_overflow_with_an_ellipsis() {
        assert_eq!(truncate_to_width("abcdef", 4), "abc…");
        assert_eq!(truncate_to_width("abc", 4), "abc");
        assert_eq!(truncate_to_width("中文abc", 3), "中…");
    }
}
