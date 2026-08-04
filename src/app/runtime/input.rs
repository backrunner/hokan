use std::{sync::Arc, time::Instant};

use super::super::buffer::MirrorOutcome;
use super::{
    ESCAPE_TIMEOUT, current_terminal_size, output_error,
    render::{handle_terminal_reply, hide_overlay_if_query_suppressed, render_current},
    results::{
        AiResult, EnterResolution, SelectedActivation, resolve_enter, resolve_selected_activation,
        start_ai_request,
    },
    state::{PendingConfirm, RuntimeState, move_selection, selected_candidate},
    worker::ProviderWorker,
};
use crate::{
    completion::Activation,
    config::Config,
    pty::{PtyChild, PtyReadEvent, SignalEvent},
    shell::{ShellKind, ShellSession, accept_sequence, replacement_sequence},
    terminal::{InputDecoder, InputEvent, InputKind, OutputHandle, TerminalReplyRouter},
};
use crossbeam_channel::Sender;

#[allow(clippy::too_many_arguments)]
pub(super) fn route_terminal_input(
    bytes: &[u8],
    router: &mut TerminalReplyRouter,
    decoder: &mut InputDecoder,
    state: &mut RuntimeState,
    pty: &mut PtyChild,
    session: &ShellSession,
    output: &OutputHandle,
    worker: &ProviderWorker,
    config: &Arc<Config>,
    ai_sender: &Sender<AiResult>,
) -> crate::Result<()> {
    let routed = router.route(bytes, Instant::now());
    for reply in routed.replies {
        if handle_terminal_reply(reply, output)? {
            render_current(state, output)?;
        }
    }
    let events = decoder.feed(&routed.input);
    state.escape_deadline = decoder
        .has_pending_ambiguity()
        .then(|| Instant::now() + ESCAPE_TIMEOUT);
    for event in events {
        handle_input_event(
            event, state, pty, session, output, worker, config, ai_sender,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn handle_input_event(
    event: InputEvent,
    state: &mut RuntimeState,
    pty: &mut PtyChild,
    session: &ShellSession,
    output: &OutputHandle,
    worker: &ProviderWorker,
    config: &Arc<Config>,
    ai_sender: &Sender<AiResult>,
) -> crate::Result<()> {
    if !state.editing {
        pty.write_all(&event.raw)?;
        return Ok(());
    }

    // Any further keypress invalidates a pending re-selection intent; the
    // overlay navigation keys below re-establish it via `move_selection`.
    state.selection_intent = None;

    // Danger-confirmation routing takes precedence over the normal overlay
    // block: Enter proceeds with the pending execution, Esc cancels back to
    // the candidate list, and any other key drops the confirmation and is
    // then processed normally.
    if state.pending_confirm.is_some() {
        if config.keys.activate.matches(&event.kind) {
            let confirm = state
                .pending_confirm
                .take()
                .ok_or_else(|| crate::Error::Runtime("pending confirmation disappeared".into()))?;
            return execute_text(state, pty, session, output, confirm.text);
        }
        if config.keys.dismiss.matches(&event.kind) {
            state.pending_confirm = None;
            return render_current(state, output);
        }
        state.pending_confirm = None;
    }

    if state.overlay_visible {
        if config.keys.up.matches(&event.kind) {
            move_selection(state, -1);
            return render_current(state, output);
        }
        if config.keys.down.matches(&event.kind) {
            move_selection(state, 1);
            return render_current(state, output);
        }
        if config.keys.page_up.matches(&event.kind) {
            move_selection(state, -(state.page_size as isize));
            return render_current(state, output);
        }
        if config.keys.page_down.matches(&event.kind) {
            move_selection(state, state.page_size as isize);
            return render_current(state, output);
        }
        // The activate key (Enter) is two-state. With an explicit selection it
        // activates that candidate: runnable candidates are EXECUTED (with a
        // confirmation step when dangerous), everything else degrades to the
        // Tab behavior below. With no selection it falls through to the
        // generic Enter branch, which submits the typed buffer unchanged.
        if config.keys.activate.matches(&event.kind) && state.selected.is_some() {
            return enter_with_selection(state, pty, session, output, worker, config, ai_sender);
        }
        // The accept key (Tab) always activates as an edit-back fill — it
        // never executes.
        if config.keys.accept.matches(&event.kind) {
            if state.selected.is_some() {
                return activate_selected(state, pty, session, output, worker, config, ai_sender);
            }
            return Ok(());
        }
        if config.keys.dismiss.matches(&event.kind) {
            state.overlay_visible = false;
            state.cancel_ai();
            output.hide_overlay().map_err(output_error)?;
            return Ok(());
        }
        if config.keys.toggle.matches(&event.kind) {
            state.overlay_visible = false;
            state.cancel_ai();
            output.hide_overlay().map_err(output_error)?;
            return Ok(());
        }
        if config.keys.history.matches(&event.kind) {
            state.history_only = !state.history_only;
            state.schedule_query(worker)?;
            hide_overlay_if_query_suppressed(state, output)?;
            return Ok(());
        }
    } else if config.keys.history.matches(&event.kind) || config.keys.toggle.matches(&event.kind) {
        state.history_only = config.keys.history.matches(&event.kind);
        state.schedule_query(worker)?;
        if state.provider_pending {
            // These keys write nothing to the shell, so no redisplay follows
            // and no render gate opens on an idle prompt — re-anchor with a
            // cursor probe (same path as `pending_reanchor`) so the result
            // frame can actually be admitted.
            output
                .allow_cursor_probe(state.buffer.revision)
                .map_err(output_error)?;
            state.need_cpr = true;
        }
        return Ok(());
    }

    if state.ai_query_id().is_some() {
        state.cancel_ai();
        state.overlay_visible = false;
        output.hide_overlay().map_err(output_error)?;
    }

    if matches!(event.kind, InputKind::Enter) {
        state.pending_command = Some(state.buffer.text.clone());
        state.editing = false;
        state.overlay_visible = false;
        state.cancel_ai();
        output.hide_overlay().map_err(output_error)?;
        output.set_foreground_and_wait(true).map_err(output_error)?;
        pty.write_all(&event.raw)?;
        return Ok(());
    }

    if matches!(event.kind, InputKind::CtrlL) {
        state.cancel_ai();
        state.pending_reanchor = true;
        state.need_cpr = false;
        state.overlay_visible = false;
        output.invalidate_anchor().map_err(output_error)?;
    }

    // Any key that reaches this point edits (or moves within) the shell
    // buffer rather than navigating the overlay. Drop the selection right
    // away: for exact-sync shells the authoritative buffer update only
    // arrives later via the control channel, and a stale selection must
    // never turn a following Enter into a candidate execution.
    state.selected = None;

    pty.write_all(&event.raw)?;

    if state.shell.exact_buffer_sync() {
        if matches!(event.kind, InputKind::Raw) {
            state.buffer.mark_uncertain();
            output.hide_overlay().map_err(output_error)?;
        }
        return Ok(());
    }

    match state.buffer.apply_mirrored(&event.kind)? {
        MirrorOutcome::Changed => {
            state.pending_mirror_revision = Some(state.buffer.revision);
            state.schedule_query(worker)?;
            hide_overlay_if_query_suppressed(state, output)?;
        }
        MirrorOutcome::Uncertain => {
            state.overlay_visible = false;
            output.hide_overlay().map_err(output_error)?;
        }
        MirrorOutcome::Submitted | MirrorOutcome::Unchanged => {}
    }
    Ok(())
}

fn activate_selected(
    state: &mut RuntimeState,
    pty: &mut PtyChild,
    session: &ShellSession,
    output: &OutputHandle,
    worker: &ProviderWorker,
    config: &Arc<Config>,
    ai_sender: &Sender<AiResult>,
) -> crate::Result<()> {
    let (activation, context) = match resolve_selected_activation(state)? {
        SelectedActivation::None => return Ok(()),
        SelectedActivation::Ready {
            activation,
            context,
        } => (activation, context),
        SelectedActivation::Rejected => {
            state.schedule_query(worker)?;
            state.status = Some("HK-CMP-STALE selection expired; candidates refreshed".into());
            return render_current(state, output);
        }
    };
    match activation {
        Activation::ReplaceBuffer { text, cursor } => {
            // Edit-back (rewriting the shell buffer) is reachable only via an
            // explicit key: Tab always fills, and Enter on a selection fills
            // for non-runnable candidates. Runnable candidates selected with
            // Enter go through `execute_text` instead — execution always
            // submits text the user saw in full (they typed it, or they
            // explicitly selected it and confirmed when it is dangerous).
            output.hide_overlay().map_err(output_error)?;
            replace_shell_buffer(state.shell, &text, cursor, pty, session)?;
            state.overlay_visible = false;
            if !state.shell.exact_buffer_sync() {
                state.buffer.replace_mirrored(text, cursor)?;
                state.pending_mirror_revision = Some(state.buffer.revision);
                state.schedule_query(worker)?;
            }
        }
        Activation::RequestAi => {
            start_ai_request(state, &context, config, ai_sender, output)?;
        }
        Activation::ConfigureAi => {
            state.cancel_ai();
            let text = "hokan ai setup".to_owned();
            // Same explicit edit-back contract as ReplaceBuffer above.
            output.hide_overlay().map_err(output_error)?;
            replace_shell_buffer(state.shell, &text, text.len(), pty, session)?;
            state.overlay_visible = false;
            if !state.shell.exact_buffer_sync() {
                let cursor = text.len();
                state.buffer.replace_mirrored(text, cursor)?;
                state.pending_mirror_revision = Some(state.buffer.revision);
                state.schedule_query(worker)?;
            }
        }
        Activation::Retry => {
            start_ai_request(state, &context, config, ai_sender, output)?;
        }
        Activation::None => {}
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn enter_with_selection(
    state: &mut RuntimeState,
    pty: &mut PtyChild,
    session: &ShellSession,
    output: &OutputHandle,
    worker: &ProviderWorker,
    config: &Arc<Config>,
    ai_sender: &Sender<AiResult>,
) -> crate::Result<()> {
    let Some(candidate) = selected_candidate(state).cloned() else {
        return Ok(());
    };
    let activation = match resolve_selected_activation(state)? {
        SelectedActivation::None => return Ok(()),
        SelectedActivation::Ready { activation, .. } => activation,
        SelectedActivation::Rejected => {
            state.schedule_query(worker)?;
            state.status = Some("HK-CMP-STALE selection expired; candidates refreshed".into());
            return render_current(state, output);
        }
    };
    match resolve_enter(&candidate, &activation) {
        EnterResolution::Fill => {
            activate_selected(state, pty, session, output, worker, config, ai_sender)
        }
        EnterResolution::Execute(text) => execute_text(state, pty, session, output, text),
        EnterResolution::Confirm {
            text,
            risk,
            reasons,
        } => {
            state.pending_confirm = Some(PendingConfirm {
                text,
                risk,
                reasons,
            });
            render_current(state, output)
        }
    }
}

/// Submit `text` as the shell's next command line: rewrite the buffer to
/// exactly `text` and accept it in the same step. Bookkeeping mirrors the
/// typed Enter branch so END/history recording still works.
fn execute_text(
    state: &mut RuntimeState,
    pty: &mut PtyChild,
    session: &ShellSession,
    output: &OutputHandle,
    text: String,
) -> crate::Result<()> {
    state.pending_command = Some(text.clone());
    state.editing = false;
    state.overlay_visible = false;
    state.pending_confirm = None;
    state.cancel_ai();
    output.hide_overlay().map_err(output_error)?;
    output.set_foreground_and_wait(true).map_err(output_error)?;
    match state.shell {
        ShellKind::Zsh => {
            session.write_edit(&text, text.len())?;
            if let Some(sequence) = accept_sequence(state.shell) {
                pty.write_all(sequence)?;
            }
        }
        ShellKind::Bash => {
            // Keystroke replay (same as the fill path) plus a literal Enter.
            pty.write_all(b"\x01\x0b")?;
            pty.write_all(text.as_bytes())?;
            pty.write_all(b"\r")?;
        }
        ShellKind::Fish => {
            // PTY input is FIFO-ordered: the widget replaces the commandline,
            // then the appended carriage return executes it.
            session.write_edit(&text, text.len())?;
            pty.write_all(replacement_sequence(state.shell))?;
            pty.write_all(b"\r")?;
        }
    }
    Ok(())
}

fn replace_shell_buffer(
    shell: ShellKind,
    text: &str,
    cursor: usize,
    pty: &mut PtyChild,
    session: &ShellSession,
) -> crate::Result<()> {
    if cursor > text.len() || !text.is_char_boundary(cursor) || text.chars().any(char::is_control) {
        return Err(crate::Error::Shell(
            "replacement text or cursor is unsafe for shell editing".into(),
        ));
    }
    if shell == ShellKind::Bash {
        pty.write_all(b"\x01\x0b")?;
        pty.write_all(text.as_bytes())?;
        let trailing = text[cursor..].chars().count();
        for _ in 0..trailing {
            pty.write_all(b"\x1b[D")?;
        }
    } else {
        session.write_edit(text, cursor)?;
        pty.write_all(replacement_sequence(shell))?;
    }
    Ok(())
}

pub(super) fn handle_pty_event(
    event: PtyReadEvent,
    state: &mut RuntimeState,
    output: &OutputHandle,
) -> crate::Result<()> {
    match event {
        PtyReadEvent::Activity { drained, .. } => {
            if drained && let Some(revision) = state.pending_mirror_revision.take() {
                output.unlock_mirrored(revision).map_err(output_error)?;
            }
            if drained && state.pending_reanchor {
                output
                    .allow_cursor_probe(state.buffer.revision)
                    .map_err(output_error)?;
                state.pending_reanchor = false;
                state.need_cpr = true;
            }
            render_current(state, output)?;
        }
        PtyReadEvent::Eof => {}
        PtyReadEvent::Failed(message) => return Err(crate::Error::Pty(message)),
    }
    Ok(())
}

pub(super) fn handle_signal(
    event: SignalEvent,
    state: &mut RuntimeState,
    pty: &mut PtyChild,
    output: &OutputHandle,
) -> crate::Result<bool> {
    match event {
        SignalEvent::Resize => {
            state.cancel_ai();
            let size = current_terminal_size()?;
            state.update_terminal_size(size);
            state.overlay_visible = false;
            state.pending_confirm = None;
            state.pending_reanchor = state.editing;
            pty.resize(size)?;
            output.resize(size).map_err(output_error)?;
            Ok(false)
        }
        SignalEvent::Interrupt => {
            state.cancel_ai();
            pty.signal_foreground(nix::sys::signal::Signal::SIGINT)?;
            Ok(false)
        }
        SignalEvent::Suspend => {
            state.cancel_ai();
            output.restore_for_suspend().map_err(output_error)?;
            state.overlay_visible = false;
            state.pending_confirm = None;
            state.suspended = true;
            signal_hook::low_level::emulate_default_handler(signal_hook::consts::SIGTSTP)?;
            Ok(false)
        }
        SignalEvent::Continue => {
            let size = current_terminal_size()?;
            state.update_terminal_size(size);
            state.overlay_visible = false;
            state.need_cpr = false;
            state.pending_reanchor = state.editing;
            state.suspended = false;
            output.resume_after_continue(size).map_err(output_error)?;
            pty.resize(size)?;
            Ok(false)
        }
        SignalEvent::Terminate(signal) => {
            state.cancel_ai();
            state.pending_confirm = None;
            output.hide_overlay().map_err(output_error)?;
            let signal = nix::sys::signal::Signal::try_from(signal)
                .unwrap_or(nix::sys::signal::Signal::SIGTERM);
            pty.signal_foreground(signal)?;
            Ok(true)
        }
    }
}

pub(super) fn detect_foreground_process(
    state: &mut RuntimeState,
    pty: &PtyChild,
    output: &OutputHandle,
) -> crate::Result<()> {
    if !state.foreground_process && pty.shell_is_foreground() == Some(false) {
        state.foreground_process = true;
        state.editing = false;
        state.overlay_visible = false;
        state.pending_confirm = None;
        state.cancel_ai();
        output.hide_overlay().map_err(output_error)?;
        output.set_foreground(true).map_err(output_error)?;
    }
    Ok(())
}
