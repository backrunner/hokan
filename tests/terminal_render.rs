use avt::Vt;
use hokan::terminal::{
    BufferRevision, CellPos, CursorRestore, FrameRevision, FrameTicket, OverlayCompositor,
    OverlayRow, OverlaySurfaceRenderer, OverlayView, RiskLevel, ScreenEpoch, ScreenRevision,
    SurfaceGeometry, SurfaceKey, SurfaceTheme, SyncOutputCapability, TerminalModel, TerminalSize,
    WidthPolicy,
};

const COLS: u16 = 80;
const ROWS: u16 = 24;
const OVERLAY_TOP: u16 = 4;
const OVERLAY_HEIGHT: u16 = 3;

#[test]
fn fallback_navigation_has_no_blank_intermediate_and_both_models_agree() {
    let size = TerminalSize::new(ROWS, COLS).expect("test terminal size is valid");
    let geometry =
        SurfaceGeometry::new(OVERLAY_TOP, size, OVERLAY_HEIGHT).expect("surface fits terminal");
    let key = SurfaceKey {
        screen_epoch: ScreenEpoch::new(1),
        rect: geometry.rect,
        theme_revision: 1,
        width_policy: WidthPolicy::Auto,
    };
    let cursor = CursorRestore {
        position: CellPos::new(3, 4),
        visible: true,
        sgr: b"\x1b[0m".to_vec(),
    };
    let renderer = OverlaySurfaceRenderer::new(OVERLAY_HEIGHT, SurfaceTheme::default(), true);
    let mut compositor = OverlayCompositor::default();

    let first_buffer = renderer.render(geometry, &view(1));
    let first = compositor
        .prepare(
            key,
            first_buffer,
            ticket(1),
            &cursor,
            SyncOutputCapability::UnsupportedFallback,
            None,
        )
        .expect("initial frame should compose");
    let first_bytes = first.staged().bytes.clone();
    compositor
        .commit(first)
        .expect("initial frame should commit");

    let second_buffer = renderer.render(geometry, &view(2));
    let second = compositor
        .prepare(
            key,
            second_buffer,
            ticket(2),
            &cursor,
            SyncOutputCapability::UnsupportedFallback,
            None,
        )
        .expect("navigation frame should compose");
    // Only the single item row changes: the borders (pagination in the top
    // edge, hints in the bottom edge) are identical between the two frames.
    assert_eq!(second.staged().changed_rows, vec![5]);
    assert_forbidden_sequences_absent(&second.staged().bytes);

    let mut vt100 = vt100::Parser::new(ROWS, COLS, 0);
    let mut avt = Vt::new(COLS as usize, ROWS as usize);
    let prompt = b"\x1b[4;1H$ ls";
    vt100.process(prompt);
    avt.feed_str(std::str::from_utf8(prompt).expect("prompt transcript is UTF-8"));
    vt100.process(&first_bytes);
    avt.feed_str(std::str::from_utf8(&first_bytes).expect("frame transcript is UTF-8"));

    // Feed avt only complete UTF-8 sequences: the overlay now contains
    // multi-byte glyphs (box drawing, ▶, Nerd Font icons), and feeding single
    // bytes as chars would mangle them into mojibake. vt100 handles partial
    // sequences natively, so it keeps the per-byte cadence.
    let mut pending: Vec<u8> = Vec::new();
    for &byte in &second.staged().bytes {
        vt100.process(std::slice::from_ref(&byte));
        pending.push(byte);
        if let Ok(chunk) = std::str::from_utf8(&pending) {
            avt.feed_str(chunk);
            pending.clear();
        }

        assert!(!vt100_region_is_blank(vt100.screen()));
        assert!(!avt_region_is_blank(&avt));
        assert!(vt100_row(vt100.screen(), 3).starts_with("$ ls"));
        assert!(avt.line(3).chars().collect::<String>().starts_with("$ ls"));
    }
    assert!(
        pending.is_empty(),
        "frame bytes must end on a UTF-8 boundary"
    );

    assert_models_match(&vt100, &avt);
    assert_eq!(vt100.screen().cursor_position(), (3, 4));
    assert_eq!(avt.cursor(), (4, 3));
    assert!(avt.cursor().visible);
}

#[test]
fn moved_overlay_blanks_vacated_cells_and_both_models_agree() {
    let size = TerminalSize::new(ROWS, COLS).expect("test terminal size is valid");
    let geometry_a = SurfaceGeometry::new_anchored(0, OVERLAY_TOP, size, OVERLAY_HEIGHT, 40)
        .expect("surface A fits terminal");
    let geometry_b = SurfaceGeometry::new_anchored(6, OVERLAY_TOP, size, OVERLAY_HEIGHT, 40)
        .expect("surface B fits terminal");
    assert_eq!(geometry_a.rect.x, 0);
    assert_eq!(geometry_b.rect.x, 6);
    let key = |rect| SurfaceKey {
        screen_epoch: ScreenEpoch::new(1),
        rect,
        theme_revision: 1,
        width_policy: WidthPolicy::Auto,
    };
    let cursor = CursorRestore {
        position: CellPos::new(3, 4),
        visible: true,
        sgr: b"\x1b[0m".to_vec(),
    };
    let renderer = OverlaySurfaceRenderer::new(OVERLAY_HEIGHT, SurfaceTheme::default(), true);
    let mut compositor = OverlayCompositor::default();

    let first = compositor
        .prepare(
            key(geometry_a.rect),
            renderer.render(geometry_a, &view(1)),
            ticket(1),
            &cursor,
            SyncOutputCapability::UnsupportedFallback,
            None,
        )
        .expect("initial frame should compose");
    let first_bytes = first.staged().bytes.clone();
    compositor
        .commit(first)
        .expect("initial frame should commit");
    let second = compositor
        .prepare(
            key(geometry_b.rect),
            renderer.render(geometry_b, &view(1)),
            ticket(2),
            &cursor,
            SyncOutputCapability::UnsupportedFallback,
            None,
        )
        .expect("moved frame should compose");
    assert_forbidden_sequences_absent(&second.staged().bytes);

    let mut vt100 = vt100::Parser::new(ROWS, COLS, 0);
    let mut avt = Vt::new(COLS as usize, ROWS as usize);
    vt100.process(&first_bytes);
    avt.feed_str(std::str::from_utf8(&first_bytes).expect("frame transcript is UTF-8"));
    assert_eq!(
        vt100
            .screen()
            .cell(OVERLAY_TOP, 0)
            .expect("cell")
            .contents(),
        "╭"
    );

    let mut pending: Vec<u8> = Vec::new();
    for &byte in &second.staged().bytes {
        vt100.process(std::slice::from_ref(&byte));
        pending.push(byte);
        if let Ok(chunk) = std::str::from_utf8(&pending) {
            avt.feed_str(chunk);
            pending.clear();
        }
    }
    assert!(
        pending.is_empty(),
        "frame bytes must end on a UTF-8 boundary"
    );

    // The vacated left edge of the old box is blank in both models…
    for row in OVERLAY_TOP..OVERLAY_TOP + OVERLAY_HEIGHT {
        for col in 0..6 {
            let contents = vt100.screen().cell(row, col).expect("cell").contents();
            assert!(
                contents.is_empty() || contents == " ",
                "vacated cell ({row}, {col}) still holds {contents:?}"
            );
        }
    }
    // …and the box now sits at its new column in both models.
    assert_eq!(
        vt100
            .screen()
            .cell(OVERLAY_TOP, 6)
            .expect("cell")
            .contents(),
        "╭"
    );
    assert_models_match(&vt100, &avt);
    assert_eq!(vt100.screen().cursor_position(), (3, 4));
    assert_eq!(avt.cursor(), (4, 3));
}

#[test]
fn hidden_chinese_overlay_clears_every_background_cell() {
    assert_chinese_overlay_cleanup(false, false);
}

#[test]
fn moved_chinese_overlay_clears_every_vacated_background_cell() {
    assert_chinese_overlay_cleanup(true, false);
}

#[test]
fn chinese_overlay_cleanup_preserves_shell_replacements() {
    assert_chinese_overlay_cleanup(false, true);
}

fn assert_chinese_overlay_cleanup(move_overlay: bool, shell_overwrite: bool) {
    // Background assertions must remain effective when the test runner sets NO_COLOR.
    crossterm::style::force_color_output(true);
    let size = TerminalSize::new(ROWS, COLS).expect("valid size");
    let geometry =
        SurfaceGeometry::new_anchored(0, OVERLAY_TOP, size, 4, 60).expect("surface fits terminal");
    let key = SurfaceKey {
        screen_epoch: ScreenEpoch::new(1),
        rect: geometry.rect,
        theme_revision: 1,
        width_policy: WidthPolicy::Auto,
    };
    let cursor = CursorRestore {
        position: CellPos::new(3, 4),
        visible: true,
        sgr: b"\x1b[0m".to_vec(),
    };
    let view = OverlayView::with_rows(
        vec![
            OverlayRow::new(1, "HIS", "中文目录", "显示当前目录文件", RiskLevel::Low),
            OverlayRow::new(2, "HIS", "中文路径", "显示当前目录文件", RiskLevel::Low),
        ],
        Some(1),
    );
    let renderer = OverlaySurfaceRenderer::new(4, SurfaceTheme::default(), true);
    let buffer = renderer.render(geometry, &view);
    let text_row = geometry.rect.y + 1;
    let text_col = (geometry.rect.x..geometry.rect.right())
        .find(|col| buffer[(*col, text_row)].symbol() == "中")
        .expect("Chinese primary text");
    let mut compositor = OverlayCompositor::default();
    let mut model = TerminalModel::new(size);
    let mut vt100 = vt100::Parser::new(ROWS, COLS, 0);
    let mut background_probe = vt100::Parser::new(ROWS, COLS, 0);
    let mut avt = Vt::new(COLS as usize, ROWS as usize);
    let first = compositor
        .prepare(
            key,
            buffer,
            ticket(1),
            &cursor,
            SyncOutputCapability::UnsupportedFallback,
            Some(&model),
        )
        .expect("first frame");
    let bytes = &first.staged().bytes;
    model.apply_hokan_frame(bytes);
    vt100.process(bytes);
    // vt100 and avt both clear the adjacent cell as a side effect of
    // overwriting a wide glyph. That can mask an incomplete erase on real
    // terminals. Replay the same colors with wide glyphs replaced by spaces
    // to check that cleanup explicitly resets every occupied column.
    let background_bytes: String = std::str::from_utf8(bytes)
        .expect("UTF-8 frame")
        .chars()
        .map(|ch| {
            let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if width > 1 {
                " ".repeat(width)
            } else {
                ch.to_string()
            }
        })
        .collect();
    background_probe.process(background_bytes.as_bytes());
    avt.feed_str(std::str::from_utf8(bytes).expect("UTF-8 frame"));
    compositor.commit(first).expect("commit first frame");

    if shell_overwrite {
        // Replace two old CJK glyphs with a different wide glyph and ASCII.
        // Cleanup must not blank the replacement glyph's second column.
        let shell = format!("\x1b[{};{}H界OK", text_row + 1, text_col + 1);
        model.process(shell.as_bytes()).expect("shell output");
        vt100.process(shell.as_bytes());
        background_probe.process(shell.as_bytes());
        avt.feed_str(&shell);
        compositor.invalidate_diff_base();
    }

    let moved = SurfaceGeometry::new_anchored(19, OVERLAY_TOP, size, 4, 60)
        .expect("moved surface fits terminal");
    let cleanup = if move_overlay {
        compositor
            .prepare(
                SurfaceKey {
                    rect: moved.rect,
                    ..key
                },
                renderer.render(moved, &view),
                ticket(2),
                &cursor,
                SyncOutputCapability::UnsupportedFallback,
                Some(&model),
            )
            .expect("moved frame")
    } else {
        compositor
            .prepare_hide(
                ticket(2),
                &cursor,
                SyncOutputCapability::UnsupportedFallback,
                Some(&model),
            )
            .expect("hide frame")
            .expect("painted footprint")
    };
    assert_forbidden_sequences_absent(&cleanup.staged().bytes);
    vt100.process(&cleanup.staged().bytes);
    background_probe.process(&cleanup.staged().bytes);
    avt.feed_str(std::str::from_utf8(&cleanup.staged().bytes).expect("UTF-8 frame"));
    for row in geometry.rect.y..geometry.rect.bottom() {
        for col in geometry.rect.x..geometry.rect.right() {
            if move_overlay && moved.rect.contains((col, row).into()) {
                continue;
            }
            if shell_overwrite && row == text_row && (text_col..text_col + 4).contains(&col) {
                continue;
            }
            let cell = vt100.screen().cell(row, col).expect("cell");
            assert!(
                cell.contents().trim().is_empty(),
                "vt100 text at ({row}, {col})"
            );
            assert_eq!(
                cell.bgcolor(),
                vt100::Color::Default,
                "vt100 background at ({row}, {col})"
            );
            assert_eq!(
                background_probe
                    .screen()
                    .cell(row, col)
                    .expect("cell")
                    .bgcolor(),
                vt100::Color::Default,
                "cleanup did not explicitly reset background at ({row}, {col})"
            );
            let cell = &avt.line(row as usize).cells()[col as usize];
            assert_eq!(
                cell.pen().background(),
                None,
                "avt background at ({row}, {col})"
            );
        }
    }
    if shell_overwrite {
        assert_eq!(
            vt100
                .screen()
                .cell(text_row, text_col)
                .expect("cell")
                .contents(),
            "界"
        );
        assert!(
            vt100
                .screen()
                .cell(text_row, text_col + 1)
                .expect("cell")
                .is_wide_continuation()
        );
        assert_eq!(
            vt100
                .screen()
                .cell(text_row, text_col + 2)
                .expect("cell")
                .contents(),
            "O"
        );
        assert_eq!(
            vt100
                .screen()
                .cell(text_row, text_col + 3)
                .expect("cell")
                .contents(),
            "K"
        );
    }
    assert_models_match(&vt100, &avt);
    assert_eq!(vt100.screen().cursor_position(), (3, 4));
    assert_eq!(avt.cursor(), (4, 3));
}

fn view(selected: u64) -> OverlayView {
    OverlayView::with_rows(
        vec![
            OverlayRow::new(1, "HIS", "ls", "list files", RiskLevel::Low),
            OverlayRow::new(2, "SPEC", "ls -lah", "long listing", RiskLevel::ReadOnly),
            OverlayRow::new(3, "FS", "script.sh", "shell script", RiskLevel::Low),
        ],
        Some(selected),
    )
}

fn ticket(frame_revision: u64) -> FrameTicket {
    FrameTicket {
        buffer_revision: BufferRevision::new(1),
        frame_revision: FrameRevision::new(frame_revision),
        screen_revision: ScreenRevision::new(1),
        screen_epoch: ScreenEpoch::new(1),
    }
}

fn vt100_region_is_blank(screen: &vt100::Screen) -> bool {
    (OVERLAY_TOP..OVERLAY_TOP + OVERLAY_HEIGHT).all(|row| {
        (0..COLS - 1).all(|col| {
            screen
                .cell(row, col)
                .is_none_or(|cell| cell.contents().trim().is_empty())
        })
    })
}

fn avt_region_is_blank(terminal: &Vt) -> bool {
    (OVERLAY_TOP as usize..(OVERLAY_TOP + OVERLAY_HEIGHT) as usize)
        .all(|row| terminal.line(row).chars().all(char::is_whitespace))
}

fn vt100_row(screen: &vt100::Screen, row: u16) -> String {
    (0..COLS)
        .filter_map(|col| {
            let cell = screen.cell(row, col)?;
            // Skip the padding cell of wide glyphs (CJK hints in the border
            // edges): avt's `line()` yields each glyph once, so continuation
            // cells must not contribute a placeholder space either.
            if cell.is_wide_continuation() {
                return None;
            }
            Some(cell.contents().chars().next().unwrap_or(' '))
        })
        .collect()
}

fn assert_models_match(vt100: &vt100::Parser, avt: &Vt) {
    for row in 0..ROWS {
        let vt100_row = vt100_row(vt100.screen(), row);
        let avt_row: String = avt.line(row as usize).chars().take(COLS as usize).collect();
        assert_eq!(
            vt100_row.trim_end(),
            avt_row.trim_end(),
            "virtual terminals differ on row {row}"
        );
    }
}

fn assert_forbidden_sequences_absent(bytes: &[u8]) {
    assert!(
        !bytes
            .windows(4)
            .any(|window| window == b"\x1b[2J" || window == b"\x1b[3J")
    );
    assert!(
        !bytes
            .windows(2)
            .any(|window| window == b"\x1b7" || window == b"\x1b8")
    );
    assert!(
        !bytes
            .windows(8)
            .any(|window| window == b"\x1b[?1049h" || window == b"\x1b[?1049l")
    );
}
