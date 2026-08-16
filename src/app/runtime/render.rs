use std::time::Instant;

use super::{
    TERMINAL_QUERY_TIMEOUT,
    cursor_probe::{
        CursorProbeBackend, PendingTmuxCursor, TMUX_CURSOR_RETRY_DELAY, TmuxCursorProbe,
        TmuxCursorResult,
    },
    output_error,
    state::RuntimeState,
};
use crate::terminal::{
    FrameRequest, FrameTicket, OutputHandle, OverlayRow, OverlayView, RenderReadiness,
    SanitizedText, SurfaceKey, SyncOutputCapability, TerminalQueryKind, TerminalReply,
    TerminalReplyRouter, WidthPolicy,
};

pub(super) fn render_current(state: &mut RuntimeState, output: &OutputHandle) -> crate::Result<()> {
    if !state.overlay_visible
        && state.candidates.is_empty()
        && state.status.is_none()
        && state.pending_confirm.is_none()
    {
        state.repaint_pending = false;
        return Ok(());
    }
    if state.candidates.is_empty() && state.status.is_none() && state.pending_confirm.is_none() {
        state.overlay_visible = false;
        state.repaint_pending = false;
        output.hide_overlay().map_err(output_error)?;
        return Ok(());
    }
    let output_state = output.state().map_err(output_error)?;
    // The readiness proof is matched on the buffer revision only: the screen
    // model legitimately advances after a gate converges (probe bytes,
    // trailing redraw output), and the commit path already re-checks the
    // frame ticket against the CURRENT model revision. Requiring the proof's
    // screen revision to equal the live model would strand the overlay once
    // anything advances the model without arming a new gate.
    if output_state.foreground
        || output_state.alternate_screen
        || output_state.confidence == crate::terminal::AnchorConfidence::Unknown
        || !matches!(
            output_state.readiness,
            RenderReadiness::Ready { buffer_revision, .. } if buffer_revision == state.buffer.revision
        )
    {
        // The terminal is not ready for a frame (render gate, anchor probe,
        // or a foreground/alternate-screen child). Provider results often
        // land before the gate converges — remember the owed repaint so the
        // main loop retries it, otherwise a single edit-back (Tab fill)
        // would leave the refreshed list invisible until the next keypress.
        state.repaint_pending = true;
        // A gate whose redisplay convergence was lost (e.g. the marker raced
        // a hokan frame and was dropped) expires into `Unknown` and never
        // recovers on its own: re-anchor with a cursor probe, the same
        // recovery path used for an idle prompt without a gate. The anchor
        // confidence can ALSO be lost independently (a desynchronized byte
        // stream invalidates the screen model while the render gate still
        // converges), so arm the probe on either signal — gating on
        // readiness alone deadlocks exactly that case. Re-arm on every
        // retry: `allow_cursor_probe` only enables the probe once the
        // scanner is safe again, and this is the sole path that re-evaluates
        // it, so guarding on `need_cpr` would deadlock the recovery.
        if matches!(output_state.readiness, RenderReadiness::Unknown)
            || output_state.confidence == crate::terminal::AnchorConfidence::Unknown
        {
            output
                .allow_cursor_probe(state.buffer.revision)
                .map_err(output_error)?;
            state.need_cpr = true;
        }
        return Ok(());
    }
    // Never commit a frame whose rows belong to a superseded query. The last
    // committed frame stays on screen until fresh results replace it; its
    // rows remain safely activatable through `candidates_context` only while
    // the underlying buffer snapshot is unchanged.
    if state.pending_confirm.is_none()
        && state.candidates.first().is_some_and(|candidate| {
            Some(candidate.query_id) != state.context.as_ref().map(|context| context.query_id)
        })
    {
        return Ok(());
    }
    let Some(geometry) = output.prepare_surface().map_err(output_error)? else {
        state.repaint_pending = true;
        return Ok(());
    };
    state.frame_revision = state
        .frame_revision
        .checked_next()
        .ok_or_else(|| crate::Error::Runtime("frame revision exhausted".into()))?;
    let ticket = FrameTicket {
        buffer_revision: state.buffer.revision,
        frame_revision: state.frame_revision,
        screen_revision: output_state.screen_revision,
        screen_epoch: output_state.screen_epoch,
    };
    let view = if let Some(confirm) = &state.pending_confirm {
        // Danger confirmation: a single synthetic EXEC row with the full final
        // command and the joined risk reasons; the bottom border carries the
        // confirm/cancel hint instead of the usual status.
        let mut row = OverlayRow::new(
            0,
            "EXEC",
            &confirm.text,
            &confirm.reasons.join(" · "),
            confirm.risk,
        );
        row.danger = true;
        let mut view = OverlayView::with_rows(vec![row], None);
        view.status = Some(SanitizedText::new("Enter 确认执行 · Esc 取消"));
        view
    } else {
        let selected_index = state
            .selected
            .and_then(|selected| {
                state
                    .candidates
                    .iter()
                    .position(|candidate| candidate.id == selected)
            })
            .unwrap_or(0);
        let page_start = selected_index / state.page_size * state.page_size;
        let page_end = (page_start + state.page_size).min(state.candidates.len());
        let mut view = OverlayView::with_rows(
            state.candidates[page_start..page_end]
                .iter()
                .map(|candidate| {
                    let mut row = OverlayRow::new(
                        candidate.id.0,
                        candidate.source.label(),
                        &candidate.display.primary,
                        &candidate.display.description,
                        candidate.risk,
                    );
                    row.annotation = candidate
                        .display
                        .annotation
                        .as_deref()
                        .map(SanitizedText::new);
                    let word = candidate
                        .display
                        .primary
                        .split_whitespace()
                        .next()
                        .unwrap_or_default();
                    row.icon = Some(crate::terminal::icons::lookup_icon(word));
                    row
                })
                .collect(),
            state.selected.map(|id| id.0),
        );
        // Pagination is embedded in the top border; status replaces the key hints
        // in the bottom border.
        view.pagination = (state.candidates.len() > state.page_size)
            .then_some((selected_index.saturating_add(1), state.candidates.len()));
        view.status = state.status.as_deref().map(SanitizedText::new);
        let typed = state
            .buffer
            .text
            .get(..state.buffer.cursor)
            .unwrap_or_default();
        view.highlight = (!typed.is_empty()).then(|| SanitizedText::new(typed));
        view
    };
    let request = FrameRequest {
        ticket,
        key: SurfaceKey {
            screen_epoch: output_state.screen_epoch,
            rect: geometry.rect,
            theme_revision: state.theme_revision,
            width_policy: WidthPolicy::Auto,
        },
        geometry,
        view,
    };
    state
        .scheduler
        .submit(state.frame_revision, request.clone());
    if let Some((_, frame)) = state.scheduler.take_ready(Instant::now()) {
        output.commit_latest(frame).map_err(output_error)?;
    }
    state.overlay_visible = true;
    state.repaint_pending = false;
    Ok(())
}

pub(super) fn flush_scheduled_frame(
    state: &mut RuntimeState,
    output: &OutputHandle,
) -> crate::Result<()> {
    if state.repaint_pending {
        render_current(state, output)?;
    }
    if let Some((_, frame)) = state.scheduler.take_ready(Instant::now()) {
        output.commit_latest(frame).map_err(output_error)?;
    }
    Ok(())
}

/// When `schedule_query` suppresses completion (empty trimmed buffer without
/// history focus, uncertain sync, or a bare executable awaiting its first
/// argument), no provider result will ever arrive to clear the overlay —
/// hide it here so stale rows cannot linger on screen.
pub(super) fn hide_overlay_if_query_suppressed(
    state: &mut RuntimeState,
    output: &OutputHandle,
) -> crate::Result<()> {
    if !state.provider_pending && state.candidates.is_empty() && state.status.is_none() {
        state.overlay_visible = false;
        output.hide_overlay().map_err(output_error)?;
    }
    Ok(())
}

pub(super) fn handle_terminal_reply(
    reply: TerminalReply,
    state: &mut RuntimeState,
    output: &OutputHandle,
) -> crate::Result<bool> {
    match reply {
        TerminalReply::CursorPosition { position, .. } => {
            output.confirm_cursor(position).map_err(output_error)?;
            return Ok(true);
        }
        TerminalReply::SynchronizedOutput { capability, .. } => {
            output
                .set_sync_capability(capability)
                .map_err(output_error)?;
        }
        TerminalReply::Timeout {
            kind: TerminalQueryKind::SynchronizedOutput,
            ..
        } => {
            output
                .set_sync_capability(SyncOutputCapability::UnsupportedFallback)
                .map_err(output_error)?;
        }
        TerminalReply::Timeout {
            kind: TerminalQueryKind::CursorPositionPrivate,
            ..
        } => {
            // Standard CPR cannot be a fallback: `CSI 1;modifier R` is also
            // the xterm/Kitty encoding for modified F3. A terminal which does
            // not answer DECXCPR therefore fails closed instead of making
            // keyboard input indistinguishable from protocol traffic.
            state.cursor_probe_backend = CursorProbeBackend::Unavailable;
            state.pending_tmux_cursor = None;
            state.tmux_cursor_retry_at = None;
            state.need_cpr = false;
            output.invalidate_anchor().map_err(output_error)?;
        }
    }
    Ok(false)
}

pub(super) fn maybe_probe_cursor(
    state: &mut RuntimeState,
    router: &mut TerminalReplyRouter,
    tmux_probe: Option<&TmuxCursorProbe>,
    output: &OutputHandle,
) -> crate::Result<()> {
    if !state.need_cpr {
        return Ok(());
    }
    let output_state = output.state().map_err(output_error)?;
    if !output_state.cursor_probe_ready || output_state.foreground || output_state.alternate_screen
    {
        return Ok(());
    }
    match state.cursor_probe_backend {
        CursorProbeBackend::TerminalPrivate => {
            if router.has_outstanding() {
                return Ok(());
            }
            let query = router.register(
                TerminalQueryKind::CursorPositionPrivate,
                Instant::now(),
                TERMINAL_QUERY_TIMEOUT,
            )?;
            output.probe(query.bytes).map_err(output_error)?;
            state.need_cpr = false;
        }
        CursorProbeBackend::Tmux => {
            if state.pending_tmux_cursor.is_some() {
                return Ok(());
            }
            let now = Instant::now();
            if state
                .tmux_cursor_retry_at
                .is_some_and(|retry_at| now < retry_at)
            {
                return Ok(());
            }
            state.tmux_cursor_retry_at = None;
            let Some(tmux_probe) = tmux_probe else {
                state.cursor_probe_backend = CursorProbeBackend::TerminalPrivate;
                return Ok(());
            };
            let generation = state
                .cursor_probe_generation
                .checked_add(1)
                .ok_or_else(|| {
                    crate::Error::TerminalProtocol("cursor probe id exhausted".into())
                })?;
            if !tmux_probe.schedule(generation, state.terminal_size) {
                state.cursor_probe_backend = CursorProbeBackend::TerminalPrivate;
                return Ok(());
            }
            state.cursor_probe_generation = generation;
            state.pending_tmux_cursor = Some(PendingTmuxCursor {
                generation,
                buffer_revision: state.buffer.revision,
                screen_revision: output_state.screen_revision,
                screen_epoch: output_state.screen_epoch,
                terminal_size: state.terminal_size,
            });
            state.need_cpr = false;
        }
        CursorProbeBackend::Unavailable => state.need_cpr = false,
    }
    Ok(())
}

pub(super) fn handle_tmux_cursor_result(
    result: TmuxCursorResult,
    state: &mut RuntimeState,
    output: &OutputHandle,
) -> crate::Result<bool> {
    let Some(pending) = state.pending_tmux_cursor.take() else {
        return Ok(false);
    };
    if pending.generation != result.generation {
        state.pending_tmux_cursor = Some(pending);
        return Ok(false);
    }
    let output_state = output.state().map_err(output_error)?;
    let still_current = state.cursor_probe_backend == CursorProbeBackend::Tmux
        && state.editing
        && !state.foreground_process
        && state.buffer.revision == pending.buffer_revision
        && state.terminal_size == pending.terminal_size
        && output_state.screen_revision == pending.screen_revision
        && output_state.screen_epoch == pending.screen_epoch
        && output_state.cursor_probe_ready
        && !output_state.foreground
        && !output_state.alternate_screen;
    if !still_current {
        if state.editing && !state.foreground_process {
            state.need_cpr = true;
            state.tmux_cursor_retry_at = Some(Instant::now() + TMUX_CURSOR_RETRY_DELAY);
        }
        return Ok(false);
    }

    let Some(position) = result.position else {
        // A stale/injected TMUX environment or unavailable tmux binary must
        // not disable a terminal which can answer DECXCPR directly. Try the
        // keyboard-safe terminal probe once before declaring anchoring
        // unavailable; standard CPR is never used.
        state.cursor_probe_backend = CursorProbeBackend::TerminalPrivate;
        state.tmux_cursor_retry_at = None;
        state.need_cpr = true;
        return Ok(false);
    };

    state.tmux_cursor_retry_at = None;
    output.confirm_cursor(position).map_err(output_error)?;
    Ok(true)
}
