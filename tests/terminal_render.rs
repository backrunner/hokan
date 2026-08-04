use avt::Vt;
use hokan::terminal::{
    BufferRevision, CellPos, CursorRestore, FrameRevision, FrameTicket, OverlayCompositor,
    OverlayRow, OverlaySurfaceRenderer, OverlayView, RiskLevel, ScreenEpoch, ScreenRevision,
    SurfaceGeometry, SurfaceKey, SurfaceTheme, SyncOutputCapability, TerminalSize, WidthPolicy,
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
