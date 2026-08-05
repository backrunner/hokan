use ratatui::{
    buffer::Buffer,
    style::{Color, Modifier},
};
use unicode_width::UnicodeWidthStr;

use super::{layout::truncate_to_width, *};
use crate::terminal::{TerminalSize, icons::lookup_icon};

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
    let geometry =
        SurfaceGeometry::new_with_width(10, TerminalSize::new(24, 80).expect("valid size"), 4, 60)
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
    let geometry =
        SurfaceGeometry::new_with_width(10, TerminalSize::new(24, 80).expect("valid size"), 4, 60)
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
    let geometry =
        SurfaceGeometry::new_with_width(10, TerminalSize::new(24, 80).expect("valid size"), 4, 60)
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
    let geometry =
        SurfaceGeometry::new_with_width(10, TerminalSize::new(24, 80).expect("valid size"), 5, 60)
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
    let geometry =
        SurfaceGeometry::new_with_width(10, TerminalSize::new(24, 80).expect("valid size"), 4, 60)
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
    let geometry =
        SurfaceGeometry::new_with_width(10, TerminalSize::new(24, 80).expect("valid size"), 4, 60)
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
    let geometry =
        SurfaceGeometry::new_with_width(10, TerminalSize::new(24, 80).expect("valid size"), 4, 60)
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
    let geometry =
        SurfaceGeometry::new_with_width(10, TerminalSize::new(24, 80).expect("valid size"), 4, 60)
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
    let geometry =
        SurfaceGeometry::new_with_width(10, TerminalSize::new(24, 80).expect("valid size"), 4, 60)
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
    let geometry =
        SurfaceGeometry::new_with_width(10, TerminalSize::new(24, 80).expect("valid size"), 4, 60)
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
