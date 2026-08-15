use std::time::Duration;

use super::frame::scroll_room_bytes;
use super::*;
use crate::terminal::{
    CellPos, CursorRestore, DrainState, FrameRevision, OverlayRow, RenderBoundaryEvent, RiskLevel,
    WidthPolicy, render_boundary::encode_marker,
};

fn token() -> SessionToken {
    SessionToken::parse("0123456789abcdef0123456789abcdef").expect("fixture token is valid")
}

fn cell_row(parser: &vt100::Parser, row: u16, cols: u16) -> String {
    (0..cols)
        .map(|col| {
            parser
                .screen()
                .cell(row, col)
                .map_or_else(|| " ".to_string(), |cell| cell.contents().to_string())
        })
        .collect()
}

#[test]
fn scroll_room_bytes_scroll_from_the_last_row() {
    let restore = CursorRestore {
        position: CellPos::new(20, 13),
        visible: true,
        sgr: b"\x1b[0m".to_vec(),
    };
    let cursor = CellPos::new(22, 13);
    let plain = scroll_room_bytes(24, cursor, 2, &restore, false);
    assert_eq!(
        plain,
        b"\x1b[?25l\x1b[0m\x1b[24;1H\n\n\x1b[0m\x1b[21;14H\x1b[?25h".to_vec()
    );
    let synchronized = scroll_room_bytes(24, cursor, 2, &restore, true);
    assert_eq!(
        synchronized,
        b"\x1b[?2026h\x1b[?25l\x1b[0m\x1b[24;1H\n\n\x1b[0m\x1b[21;14H\x1b[?25h\x1b[?2026l".to_vec()
    );
    for bytes in [&plain, &synchronized] {
        assert!(!bytes.windows(4).any(|window| window == b"\x1b[2J"));
        assert!(
            !bytes
                .windows(8)
                .any(|window| window == b"\x1b[?1049h" || window == b"\x1b[?1049l")
        );
    }
}

#[test]
fn scroll_room_bytes_perform_real_scrolls_for_a_mid_screen_cursor() {
    let mut parser = vt100::Parser::new(24, 80, 0);
    parser.process(b"\x1b[21;1HROW_A");
    parser.process(b"\x1b[23;1HHK> echo LONG");
    assert_eq!(parser.screen().cursor_position(), (22, 13));
    let restore = CursorRestore {
        position: CellPos::new(22, 13),
        visible: true,
        sgr: b"\x1b[0m".to_vec(),
    };
    // The injection rides a mode-2026 transaction: the vt100 model must
    // tolerate the wrapper and still apply the same screen effect.
    let bytes = scroll_room_bytes(24, CellPos::new(22, 13), 2, &restore, true);
    parser.process(&bytes);
    // Two real scrolls: every row moved up by two, and the restore CUP
    // landed exactly where the shell's edit line is now — not two rows
    // above it, which is where a mid-screen `\n` would have left it.
    assert_eq!(parser.screen().cursor_position(), (20, 13));
    assert!(cell_row(&parser, 20, 13).starts_with("HK> echo LONG"));
    assert!(cell_row(&parser, 18, 5).starts_with("ROW_A"));
    assert!(cell_row(&parser, 22, 13).trim().is_empty());
}

#[test]
fn prepare_surface_scroll_keeps_the_actor_model_consistent() {
    let size = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
    let mut actor = OutputActor::new(Vec::new(), token(), size, 3);
    actor
        .model
        .process(b"\x1b[21;1HROW_A")
        .expect("marker row should parse");
    actor
        .model
        .process(b"\x1b[23;1HHK> echo LONG")
        .expect("edit line should parse");
    actor.model.establish_anchor();
    assert_eq!(actor.model.cursor(), CellPos::new(22, 13));
    let edit_line_before = actor
        .model
        .snapshot_region(ratatui::layout::Rect::new(0, 22, 80, 1));

    let geometry = actor
        .prepare_surface_geometry()
        .expect("geometry should prepare")
        .expect("overlay fits after scrolling");
    assert_eq!(geometry.rect, ratatui::layout::Rect::new(0, 21, 79, 3));

    // The model applied the same bytes the terminal received: the cursor
    // sits on the scrolled edit line and the old edit-line row changed.
    assert_eq!(actor.model.cursor(), CellPos::new(20, 13));
    assert!(actor.model.region_changed(&edit_line_before));

    let writer = actor.guard.finish().expect("guard should finish");
    let bottom_cup = b"\x1b[24;1H";
    let scrolls = b"\n\n";
    let restore_cup = b"\x1b[21;14H";
    let bottom_at = writer
        .windows(bottom_cup.len())
        .position(|window| window == bottom_cup)
        .expect("injection must first move to the last row");
    let scrolls_at = writer
        .windows(scrolls.len())
        .position(|window| window == scrolls)
        .expect("injection must emit the scroll newlines");
    let restore_at = writer
        .windows(restore_cup.len())
        .position(|window| window == restore_cup)
        .expect("injection must restore the cursor");
    assert!(bottom_at < scrolls_at && scrolls_at < restore_at);
    // Fallback capability: no synchronized transaction is opened.
    assert!(
        !writer
            .windows(b"\x1b[?2026h".len())
            .any(|window| window == b"\x1b[?2026h")
    );
}

#[test]
fn prompt_gate_repairs_kimi_input_modes_left_by_a_foreground_child() {
    let size = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
    let mut actor = OutputActor::new(Vec::new(), token(), size, 3);
    actor
        .handle_control(ControlCommand::SetForeground(true))
        .expect("foreground transition");
    actor
        .handle_child(ChildOutputBatch {
            read_cycle: 1,
            bytes: b"\x1b[?1003h\x1b[?1004h\x1b[?1006h\x1b[?2031h\x1b[>4;2m\x1b[>7u".to_vec(),
            drain: DrainState::DrainedToEagain,
        })
        .expect("TUI output");
    actor
        .handle_control(ControlCommand::SetForeground(false))
        .expect("foreground exit");
    let marker = encode_marker(
        &token(),
        RenderBoundaryEvent::PromptRendered {
            boundary_id: BoundaryId::new(1),
        },
    );
    actor
        .handle_child(ChildOutputBatch {
            read_cycle: 2,
            bytes: marker,
            drain: DrainState::DrainedToEagain,
        })
        .expect("prompt marker");
    actor
        .arm_prompt_gate(BoundaryId::new(1))
        .expect("prompt recovery");
    let output = actor.guard.finish().expect("guard should finish");
    for reset in [
        b"\x1b[?1003l".as_slice(),
        b"\x1b[?1004l".as_slice(),
        b"\x1b[?1006l".as_slice(),
        b"\x1b[?2031l".as_slice(),
        b"\x1b[>4;0m".as_slice(),
        b"\x1b[<u".as_slice(),
    ] {
        assert!(
            output.windows(reset.len()).any(|window| window == reset),
            "missing prompt recovery sequence {reset:?}"
        );
    }
}

#[test]
fn prompt_gate_recovers_when_desynchronization_precedes_the_marker() {
    let size = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
    let mut actor = OutputActor::new(Vec::new(), token(), size, 3);
    actor
        .handle_control(ControlCommand::SetForeground(true))
        .expect("foreground transition");
    let marker = encode_marker(
        &token(),
        RenderBoundaryEvent::PromptRendered {
            boundary_id: BoundaryId::new(1),
        },
    );
    let mut bytes = b"\x1b[?1003h\x1b[?1004h\x1b[>7u".to_vec();
    bytes.push(0xf5);
    bytes.extend_from_slice(&marker);
    actor
        .handle_child(ChildOutputBatch {
            read_cycle: 1,
            bytes,
            drain: DrainState::DrainedToEagain,
        })
        .expect("desynchronized prompt output");
    actor
        .handle_control(ControlCommand::SetForeground(false))
        .expect("foreground exit");
    actor
        .arm_prompt_gate(BoundaryId::new(1))
        .expect("prompt recovery");
    assert!(actor.pending_prompt_recovery.is_none());

    let output = actor.guard.finish().expect("guard should finish");
    assert!(output.windows(marker.len()).any(|window| window == marker));
    for reset in [
        b"\x1b[?1003l".as_slice(),
        b"\x1b[?1004l".as_slice(),
        b"\x1b[<u".as_slice(),
    ] {
        assert!(
            output.windows(reset.len()).any(|window| window == reset),
            "missing prompt recovery sequence {reset:?}"
        );
    }
}

#[test]
fn prompt_mode_recovery_waits_for_prompt_marker_when_control_wins_race() {
    let size = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
    let mut actor = OutputActor::new(Vec::new(), token(), size, 3);
    actor
        .handle_control(ControlCommand::SetForeground(true))
        .expect("foreground transition");
    actor
        .handle_control(ControlCommand::SetForeground(false))
        .expect("foreground exit");
    actor
        .arm_prompt_gate(BoundaryId::new(1))
        .expect("prompt gate");
    let marker = encode_marker(
        &token(),
        RenderBoundaryEvent::PromptRendered {
            boundary_id: BoundaryId::new(1),
        },
    );
    let mut bytes = b"\x1b[?1003h\x1b[?1004h\x1b[?2031h\x1b[>7uPROMPT ".to_vec();
    bytes.extend_from_slice(&marker);
    actor
        .handle_child(ChildOutputBatch {
            read_cycle: 1,
            bytes,
            drain: DrainState::DrainedToEagain,
        })
        .expect("prompt output");
    let output = actor.guard.finish().expect("guard should finish");
    assert!(
        output
            .windows(b"\x1b[?1003l".len())
            .any(|w| w == b"\x1b[?1003l")
    );
    assert!(output.windows(b"\x1b[<u".len()).any(|w| w == b"\x1b[<u"));
}

#[test]
fn prepare_surface_scroll_is_wrapped_in_a_transaction_when_2026_is_available() {
    let size = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
    let mut actor = OutputActor::new(Vec::new(), token(), size, 3);
    actor
        .model
        .process(b"\x1b[23;1HHK> echo LONG")
        .expect("edit line should parse");
    actor.model.establish_anchor();
    actor.capability = SyncOutputCapability::AvailableIdle;

    let geometry = actor
        .prepare_surface_geometry()
        .expect("geometry should prepare")
        .expect("overlay fits after scrolling");
    assert_eq!(geometry.rect, ratatui::layout::Rect::new(0, 21, 79, 3));
    assert_eq!(actor.model.cursor(), CellPos::new(20, 13));

    let writer = actor.guard.finish().expect("guard should finish");
    assert!(writer.starts_with(b"\x1b[?2026h"));
    let end_sync = b"\x1b[?2026l";
    let end_at = writer
        .windows(end_sync.len())
        .position(|window| window == end_sync)
        .expect("injection must close its own transaction");
    // Exactly one transaction: the restore presentation written by
    // finish() must come after the injection's closing sequence.
    assert_eq!(
        writer
            .windows(end_sync.len())
            .filter(|window| window == end_sync)
            .count(),
        1
    );
    assert!(writer[end_at + end_sync.len()..].ends_with(b"\x1b[?2004l\x1b[0m\x1b[?25h"));
}

#[test]
fn hide_overlay_guard_rejection_writes_a_debug_log_event() {
    let directory = tempfile::tempdir().expect("temporary state directory");
    let log = DebugLog::from_config(
        directory.path(),
        &crate::config::LoggingConfig {
            enabled: true,
            max_bytes: 64 * 1024,
            rotations: 1,
        },
    )
    .expect("logger should build")
    .expect("logger should be enabled");
    let size = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
    let mut actor = OutputActor::new(Vec::new(), token(), size, 3);
    actor.debug_log = Some(log);

    let frame = frame_request();
    let buffer = actor.renderer.render(frame.geometry, &frame.view);
    let cursor = actor.model.cursor_restore();
    let prepared = actor
        .compositor
        .prepare(
            frame.key,
            buffer,
            frame.ticket,
            &cursor,
            SyncOutputCapability::UnsupportedFallback,
            None,
        )
        .expect("frame should compose");
    actor
        .compositor
        .commit(prepared)
        .expect("frame should commit");
    actor.last_committed_ticket = Some(frame.ticket);
    // The model anchor is still Unknown: the guard must reject the hide
    // without touching the terminal, and record which guard fired.
    actor.hide_overlay().expect("hide should not error");

    let text = std::fs::read_to_string(directory.path().join("debug.log")).expect("debug log");
    let line = text
        .lines()
        .find(|line| line.contains("overlay-hide-rejected"))
        .expect("rejection event should be recorded");
    assert!(line.contains("confidence-unknown"), "event line: {line}");
    assert!(actor.compositor.current_key().is_none());
}

#[test]
fn hide_after_partial_shell_overwrite_clears_only_hokan_footprint() {
    let size = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
    let mut actor = OutputActor::new(Vec::new(), token(), size, 3);
    actor.model.invalidate().expect("fixture epoch");
    actor.model.establish_anchor();

    let frame = frame_request();
    let buffer = actor.renderer.render(frame.geometry, &frame.view);
    let prepared = actor
        .compositor
        .prepare(
            frame.key,
            buffer,
            frame.ticket,
            &actor.model.cursor_restore(),
            SyncOutputCapability::UnsupportedFallback,
            Some(&actor.model),
        )
        .expect("frame should compose");
    let frame_bytes = prepared.staged().bytes.clone();
    actor
        .guard
        .write_staged(prepared.staged())
        .expect("frame should be written");
    actor.model.apply_hokan_frame(&frame_bytes);
    actor
        .compositor
        .commit(prepared)
        .expect("frame should commit");
    actor.last_committed_ticket = Some(frame.ticket);

    // PTY output is drained before a queued Hide. It overwrites only two
    // cells in the box, invalidating the diff base while leaving the rest of
    // the committed footprint visible.
    actor
        .handle_child(ChildOutputBatch {
            read_cycle: 1,
            bytes: b"\x1b[6;1Hfg".to_vec(),
            drain: DrainState::DrainedToEagain,
        })
        .expect("shell redisplay should be written");
    assert!(actor.compositor.current_key().is_none());
    assert_eq!(actor.compositor.footprint_key(), Some(frame.key));

    actor.hide_overlay().expect("overlay should hide");
    let output = actor.guard.finish().expect("guard should finish");
    let mut parser = vt100::Parser::new(24, 80, 0);
    parser.process(&output);

    // Shell-owned text survives, while untouched top and bottom borders from
    // the old overlay are erased instead of becoming permanent screen smear.
    assert_eq!(parser.screen().cell(5, 0).expect("cell").contents(), "f");
    assert_eq!(parser.screen().cell(5, 1).expect("cell").contents(), "g");
    for row in [4, 6] {
        let contents = parser.screen().cell(row, 0).expect("cell").contents();
        assert!(
            contents.is_empty() || contents == " ",
            "stale border at ({row}, 0): {contents:?}"
        );
    }
}

fn frame_request() -> FrameRequest {
    let terminal = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
    let geometry = SurfaceGeometry::new(4, terminal, 3).expect("fixture geometry is valid");
    FrameRequest {
        ticket: FrameTicket {
            buffer_revision: BufferRevision::new(1),
            frame_revision: FrameRevision::new(1),
            screen_revision: ScreenRevision::new(2),
            screen_epoch: ScreenEpoch::new(1),
        },
        key: SurfaceKey {
            screen_epoch: ScreenEpoch::new(1),
            rect: geometry.rect,
            theme_revision: 1,
            width_policy: WidthPolicy::Auto,
        },
        geometry,
        view: OverlayView::with_rows(
            vec![OverlayRow::new(1, "HIS", "ls", "list", RiskLevel::Low)],
            Some(1),
        ),
    }
}

#[derive(Debug)]
struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "fixture failure"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn actor_failure_closes_mailbox_and_releases_waiters() {
    let size = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
    let (handle, join) = spawn_with_writer(FailingWriter, token(), size, 3)
        .expect("output actor thread should spawn");
    handle.probe(b"probe").expect("probe should queue");
    let error = join
        .join()
        .expect("actor should not panic")
        .expect_err("writer should fail");
    assert!(matches!(error, OutputError::Io(_)));
    assert!(matches!(handle.state(), Err(OutputError::Closed)));
    assert!(matches!(handle.barrier(), Err(OutputError::Closed)));
    assert!(matches!(
        handle.child_output(ChildOutputBatch {
            read_cycle: 1,
            bytes: vec![b'x'],
            drain: DrainState::DrainedToEagain,
        }),
        Err(OutputError::Closed)
    ));
}

#[test]
fn suspend_restores_and_resume_reapplies_child_input_modes() {
    const ENABLE: &[u8] = b"\x1b[?2004h";
    const DISABLE: &[u8] = b"\x1b[?2004l";
    const ENABLE_ALT_SCROLL: &[u8] = b"\x1b[?1007h";
    const DISABLE_ALT_SCROLL: &[u8] = b"\x1b[?1007l";
    const ENABLE_META_ESCAPE: &[u8] = b"\x1b[?1036h";
    const DISABLE_META_ESCAPE: &[u8] = b"\x1b[?1036l";

    let size = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
    let (handle, join) =
        spawn_with_writer(Vec::new(), token(), size, 3).expect("output actor should spawn");
    handle
        .child_output(ChildOutputBatch {
            read_cycle: 1,
            bytes: [ENABLE, ENABLE_ALT_SCROLL, ENABLE_META_ESCAPE].concat(),
            drain: DrainState::DrainedToEagain,
        })
        .expect("child mode should queue");
    handle.barrier().expect("child mode should be observed");
    handle
        .restore_for_suspend()
        .expect("suspend should restore the outer terminal");
    handle
        .resume_after_continue(size)
        .expect("resume should restore the child terminal");
    handle
        .restore_and_exit()
        .expect("final restore should queue");
    let output = join
        .join()
        .expect("actor should not panic")
        .expect("actor should exit cleanly")
        .writer;

    let transitions: Vec<_> = output
        .windows(ENABLE.len())
        .filter_map(|window| {
            if window == ENABLE {
                Some(true)
            } else if window == DISABLE {
                Some(false)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(transitions, [true, false, true, false]);
    for (enable, disable) in [
        (ENABLE_ALT_SCROLL, DISABLE_ALT_SCROLL),
        (ENABLE_META_ESCAPE, DISABLE_META_ESCAPE),
    ] {
        assert_eq!(
            output
                .windows(enable.len())
                .filter(|window| *window == enable)
                .count(),
            2,
            "child and resume should both enable {enable:?}"
        );
        assert_eq!(
            output
                .windows(disable.len())
                .filter(|window| *window == disable)
                .count(),
            2,
            "suspend and exit should both disable {disable:?}"
        );
    }
}

#[test]
fn live_overlay_configuration_updates_fixed_geometry_and_theme() {
    let size = TerminalSize::new(24, 120).expect("terminal size");
    let mut actor = OutputActor::new(Vec::new(), token(), size, 12);
    actor
        .handle_control(ControlCommand::ConfigureOverlay {
            max_height: 5,
            max_width: 60,
            color: false,
            nerd_fonts: true,
        })
        .expect("configure overlay");
    assert_eq!(actor.max_overlay_height, 5);
    assert_eq!(actor.max_overlay_width, 60);
    assert_eq!(actor.renderer.height(), 5);
    assert_eq!(actor.surface_theme.normal, ratatui::style::Style::default());
}

#[test]
fn end_to_end_gate_writes_child_before_overlay() {
    let size = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
    let (handle, join) =
        spawn_with_writer(Vec::new(), token(), size, 3).expect("output actor thread should spawn");
    handle
        .arm_prompt_gate(BoundaryId::new(1))
        .expect("prompt gate should queue");
    let prompt_marker = encode_marker(
        &token(),
        RenderBoundaryEvent::PromptRendered {
            boundary_id: BoundaryId::new(1),
        },
    );
    let mut prompt = b"prompt".to_vec();
    prompt.extend_from_slice(&prompt_marker);
    handle
        .child_output(ChildOutputBatch {
            read_cycle: 1,
            bytes: prompt,
            drain: DrainState::DrainedToEagain,
        })
        .expect("prompt output should queue");
    handle
        .confirm_cursor(super::super::CellPos::new(0, 0))
        .expect("cursor confirmation should queue");
    handle
        .arm_render_gate(RenderGateRequest {
            boundary_id: BoundaryId::new(1),
            buffer_revision: BufferRevision::new(1),
            deadline: Instant::now() + Duration::from_secs(1),
        })
        .expect("render gate should queue");
    let marker = encode_marker(
        &token(),
        RenderBoundaryEvent::PostRedisplay {
            boundary_id: BoundaryId::new(1),
        },
    );
    let mut redraw = marker.clone();
    redraw.extend_from_slice(b"redraw");
    handle
        .child_output(ChildOutputBatch {
            read_cycle: 2,
            bytes: redraw,
            drain: DrainState::DrainedToEagain,
        })
        .expect("child output should queue");
    assert!(
        handle
            .commit_latest(frame_request())
            .expect("frame should queue")
    );
    handle.barrier().expect("actor should reach the barrier");
    handle
        .restore_and_exit()
        .expect("restore should be requested");
    let exit = join
        .join()
        .expect("actor should not panic")
        .expect("actor should exit cleanly");
    assert!(exit.writer.starts_with(b"prompt"));
    assert!(
        !exit
            .writer
            .windows(prompt_marker.len())
            .any(|window| window == prompt_marker)
    );
    assert!(
        !exit
            .writer
            .windows(marker.len())
            .any(|window| window == marker)
    );
    assert_eq!(exit.report.committed_frames, 1);
    assert_eq!(exit.report.consumed_boundaries, 2);
}

#[test]
fn expired_gate_and_stale_ticket_never_render() {
    let size = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
    let (handle, join) =
        spawn_with_writer(Vec::new(), token(), size, 3).expect("output actor thread should spawn");
    handle
        .confirm_cursor(super::super::CellPos::new(0, 0))
        .expect("cursor confirmation should queue");
    handle
        .arm_render_gate(RenderGateRequest {
            boundary_id: BoundaryId::new(1),
            buffer_revision: BufferRevision::new(1),
            deadline: Instant::now(),
        })
        .expect("render gate should queue");
    handle
        .commit_latest(frame_request())
        .expect("frame should queue");
    handle.barrier().expect("actor should reach the barrier");
    handle
        .restore_and_exit()
        .expect("restore should be requested");
    let exit = join
        .join()
        .expect("actor should not panic")
        .expect("actor should exit cleanly");
    assert_eq!(exit.report.committed_frames, 0);
    assert!(exit.report.rejected_frames >= 1);
}

#[test]
fn marker_only_drain_does_not_unlock_redisplay_gate() {
    let size = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
    let (handle, join) =
        spawn_with_writer(Vec::new(), token(), size, 3).expect("output actor thread should spawn");
    handle
        .arm_prompt_gate(BoundaryId::new(1))
        .expect("prompt gate should queue");
    let prompt_marker = encode_marker(
        &token(),
        RenderBoundaryEvent::PromptRendered {
            boundary_id: BoundaryId::new(1),
        },
    );
    let mut prompt = b"$ ".to_vec();
    prompt.extend_from_slice(&prompt_marker);
    handle
        .child_output(ChildOutputBatch {
            read_cycle: 1,
            bytes: prompt,
            drain: DrainState::DrainedToEagain,
        })
        .expect("prompt output should queue");
    handle
        .confirm_cursor(super::super::CellPos::new(0, 2))
        .expect("cursor confirmation should queue");
    handle
        .arm_render_gate(RenderGateRequest {
            boundary_id: BoundaryId::new(1),
            buffer_revision: BufferRevision::new(1),
            deadline: Instant::now() + Duration::from_secs(1),
        })
        .expect("render gate should queue");
    let marker = encode_marker(
        &token(),
        RenderBoundaryEvent::PostRedisplay {
            boundary_id: BoundaryId::new(1),
        },
    );
    handle
        .child_output(ChildOutputBatch {
            read_cycle: 2,
            bytes: marker,
            drain: DrainState::DrainedToEagain,
        })
        .expect("marker-only cycle should queue");
    handle.barrier().expect("actor should reach the barrier");
    assert!(matches!(
        handle.state().expect("state should be available").readiness,
        RenderReadiness::AwaitingRedisplay { .. }
    ));

    handle
        .child_output(ChildOutputBatch {
            read_cycle: 3,
            bytes: b"x".to_vec(),
            drain: DrainState::DrainedToEagain,
        })
        .expect("screen redraw should queue");
    handle.barrier().expect("actor should reach the barrier");
    assert!(matches!(
        handle.state().expect("state should be available").readiness,
        RenderReadiness::Ready {
            buffer_revision,
            ..
        } if buffer_revision == BufferRevision::new(1)
    ));
    handle
        .restore_and_exit()
        .expect("restore should be requested");
    join.join()
        .expect("actor should not panic")
        .expect("actor should exit cleanly");
}

#[test]
fn render_gate_deadline_wakes_an_idle_actor() {
    let size = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
    let (handle, join) =
        spawn_with_writer(Vec::new(), token(), size, 3).expect("output actor thread should spawn");
    handle
        .arm_render_gate(RenderGateRequest {
            boundary_id: BoundaryId::new(1),
            buffer_revision: BufferRevision::new(1),
            deadline: Instant::now() + Duration::from_millis(20),
        })
        .expect("render gate should queue");
    handle
        .commit_latest(frame_request())
        .expect("frame should queue");
    handle.barrier().expect("actor should reach the barrier");
    std::thread::sleep(Duration::from_millis(80));
    handle
        .restore_and_exit()
        .expect("restore should be requested");
    let exit = join
        .join()
        .expect("actor should not panic")
        .expect("actor should exit cleanly");
    assert_eq!(exit.report.committed_frames, 0);
    assert_eq!(exit.report.rejected_frames, 1);
}

#[test]
fn shutdown_flushes_partial_marker_before_restoring_terminal() {
    let size = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
    let (handle, join) =
        spawn_with_writer(Vec::new(), token(), size, 3).expect("output actor thread should spawn");
    let partial = b"visible\x1b]6973;hokan;1;";
    handle
        .child_output(ChildOutputBatch {
            read_cycle: 1,
            bytes: partial.to_vec(),
            drain: DrainState::DrainedToEagain,
        })
        .expect("partial child output should queue");
    handle.barrier().expect("actor should reach the barrier");
    handle
        .restore_and_exit()
        .expect("restore should be requested");
    let exit = join
        .join()
        .expect("actor should not panic")
        .expect("actor should exit cleanly");

    assert!(exit.writer.starts_with(partial));
    assert_eq!(exit.writer.get(partial.len()), Some(&0x18));
    assert_eq!(exit.report.child_bytes, partial.len() as u64);
}

#[test]
fn mailbox_priority_is_restore_child_hide_control_frame() {
    let mailbox = OutputMailbox::default();
    mailbox
        .push_control(ControlCommand::SetForeground(false))
        .expect("control should queue");
    mailbox
        .push_frame(frame_request())
        .expect("frame should queue");
    mailbox.push_hide().expect("hide should queue");
    mailbox
        .push_child(ChildOutputBatch {
            read_cycle: 1,
            bytes: b"child".to_vec(),
            drain: DrainState::DrainedToEagain,
        })
        .expect("child output should queue");
    assert!(matches!(
        mailbox
            .take(None)
            .expect("child command should be available"),
        ActorCommand::Child(_)
    ));
    assert!(matches!(
        mailbox
            .take(None)
            .expect("hide command should be available"),
        ActorCommand::Hide
    ));
    assert!(matches!(
        mailbox
            .take(None)
            .expect("control command should be available"),
        ActorCommand::Control(_)
    ));
}
