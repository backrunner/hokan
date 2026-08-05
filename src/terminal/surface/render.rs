use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use unicode_width::UnicodeWidthStr;

use super::{
    super::{icons, icons::FALLBACK_GLYPH},
    OverlayRow, OverlayView, SanitizedText, SurfaceGeometry, SurfaceTheme,
    layout::{
        EDGE_TRAIL, GAP, ICON_SECTION, MARKER_WIDTH, MAX_DESCRIPTION_WIDTH, MAX_TAG_WIDTH,
        MIN_DESC_VISIBLE, MIN_PRIMARY_WIDTH, PAGINATION_PAD, RISK_SLOT, SIDE_PAD,
        common_prefix_len, truncate_to_width,
    },
};

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
