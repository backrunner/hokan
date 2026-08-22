use std::{sync::Arc, time::Instant};

use super::super::buffer::MirrorOutcome;
use super::{
    ESCAPE_TIMEOUT, current_terminal_size, output_error,
    render::{handle_terminal_reply, hide_overlay_if_query_suppressed, render_current},
    results::{
        AiResult, EnterResolution, SelectedActivation, resolve_enter, resolve_selected_activation,
        start_ai_request,
    },
    state::{PendingConfirm, RuntimeState, defer_selection, move_selection, selected_candidate},
    worker::ProviderWorker,
};
use crate::{
    completion::Activation,
    config::Config,
    pty::{PtyChild, PtyReadEvent, SignalEvent},
    shell::{ShellKind, ShellSession, accept_sequence, replacement_sequence},
    terminal::{
        InputDecoder, InputEvent, InputKind, OutputHandle, TerminalReplyRouter,
        input::{PASTE_END, PASTE_START},
    },
};
use crossbeam_channel::Sender;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActivationAttempt {
    Finished,
    Rejected,
}

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
        if handle_terminal_reply(reply, state, output)? {
            render_current(state, output)?;
        }
    }
    forward_terminal_input(
        &routed.input,
        decoder,
        state,
        pty,
        session,
        output,
        worker,
        config,
        ai_sender,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn forward_terminal_input(
    bytes: &[u8],
    decoder: &mut InputDecoder,
    state: &mut RuntimeState,
    pty: &mut PtyChild,
    session: &ShellSession,
    output: &OutputHandle,
    worker: &ProviderWorker,
    config: &Arc<Config>,
    ai_sender: &Sender<AiResult>,
) -> crate::Result<()> {
    if state.foreground_process {
        // The decoder may contain bytes read just after the command's Enter
        // but before the foreground transition was observed. They precede the
        // current batch and must be released first, without waiting for an
        // escape timeout or a bracketed-paste end marker.
        let buffered = decoder.take_buffered_raw();
        state.escape_deadline = None;
        if !buffered.is_empty() {
            pty.write_all(&buffered)?;
        }
        if !bytes.is_empty() {
            pty.write_all(bytes)?;
        }
        return Ok(());
    }

    let events = decoder.feed(bytes);
    if decoder.has_pending_ambiguity() {
        if !bytes.is_empty() || state.escape_deadline.is_none() {
            state.escape_deadline = Some(Instant::now() + ESCAPE_TIMEOUT);
        }
    } else {
        state.escape_deadline = None;
    }
    for event in events {
        handle_input_event(
            event, state, pty, session, output, worker, config, ai_sender,
        )?;
    }

    // `feed` can return an Enter event while retaining a trailing partial
    // escape or paste sequence from the same terminal read. Once that Enter
    // hands the PTY to the command, release the retained suffix immediately.
    if state.foreground_process {
        let buffered = decoder.take_buffered_raw();
        state.escape_deadline = None;
        if !buffered.is_empty() {
            pty.write_all(&buffered)?;
        }
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
    let child_bytes =
        input_bytes_for_shell(&event, session.supports_bracketed_paste(), state.editing);
    // A deferred Tab belongs only to the latest user input event. Shell
    // control messages may still catch up for bytes typed before it, but any
    // subsequent keypress cancels the one-shot activation intent.
    state.pending_accept = false;
    if !state.editing {
        // Input can already be queued in the outer PTY while the first prompt
        // event is still crossing the control FIFO.  Mark an Enter-triggered
        // command as foreground before forwarding it so a fast-starting TUI
        // cannot change terminal modes before the output actor snapshots the
        // shell baseline.
        if !state.foreground_process && matches!(event.kind, InputKind::Enter) {
            state.foreground_process = true;
            output.set_foreground_and_wait(true).map_err(output_error)?;
        }
        pty.write_all(child_bytes)?;
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
        if dismisses_empty_overlay(&event.kind, state.buffer.text.is_empty()) {
            state.overlay_visible = false;
            state.cancel_ai();
            output.hide_overlay().map_err(output_error)?;
            return Ok(());
        }
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
        // never executes. With no explicit selection it defaults to the
        // top-ranked candidate: `move_selection` both selects the first row
        // and records the intent, so the list refreshed after the fill
        // re-selects its first row automatically.
        if config.keys.accept.matches(&event.kind) {
            if state.selected.is_none() {
                if state.candidates.is_empty() {
                    return Ok(());
                }
                move_selection(state, 1);
            }
            if matches!(
                activate_selected(state, pty, session, output, worker, config, ai_sender)?,
                ActivationAttempt::Rejected
            ) {
                // Exact-sync shells can deliver the authoritative event for
                // the final typed byte just before this Tab is routed. Keep
                // the one-shot fill intent and apply it when that query's
                // candidates arrive unless a later keypress cancels it.
                state.pending_accept = true;
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
    } else if config.history.enabled
        && (config.keys.up.matches(&event.kind) || config.keys.down.matches(&event.kind))
    {
        // At an idle prompt Hokan owns the arrow key before it reaches the
        // child shell. This opens the private history list without installing
        // or replacing any zle/readline/fish bindings. Shell mappings remain
        // untouched, and non-editing input still passes through above.
        let delta = if config.keys.up.matches(&event.kind) {
            -1
        } else {
            1
        };
        state.history_only = true;
        state.schedule_query(worker)?;
        defer_selection(state, delta);
        arm_hidden_overlay_query(state, output)?;
        return Ok(());
    } else if config.keys.history.matches(&event.kind) || config.keys.toggle.matches(&event.kind) {
        state.history_only = config.keys.history.matches(&event.kind);
        state.schedule_query(worker)?;
        arm_hidden_overlay_query(state, output)?;
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
        state.foreground_process = true;
        state.overlay_visible = false;
        state.cancel_ai();
        output.hide_overlay().map_err(output_error)?;
        output.set_foreground_and_wait(true).map_err(output_error)?;
        pty.write_all(child_bytes)?;
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

    pty.write_all(child_bytes)?;

    if state.shell.exact_buffer_sync() {
        if matches!(event.kind, InputKind::Raw | InputKind::PasteFragment { .. }) {
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

fn dismisses_empty_overlay(kind: &InputKind, buffer_is_empty: bool) -> bool {
    buffer_is_empty && matches!(kind, InputKind::Backspace)
}

fn input_bytes_for_shell(
    event: &InputEvent,
    supports_bracketed_paste: bool,
    editing: bool,
) -> &[u8] {
    if !editing || supports_bracketed_paste {
        return &event.raw;
    }
    match &event.kind {
        InputKind::Paste(payload) => payload,
        InputKind::PasteFragment {
            strip_start,
            strip_end,
        } => {
            let start = if *strip_start && event.raw.starts_with(PASTE_START) {
                PASTE_START.len()
            } else {
                0
            };
            let end = if *strip_end && event.raw.ends_with(PASTE_END) {
                event.raw.len() - PASTE_END.len()
            } else {
                event.raw.len()
            };
            &event.raw[start.min(end)..end]
        }
        _ => &event.raw,
    }
}

fn arm_hidden_overlay_query(state: &mut RuntimeState, output: &OutputHandle) -> crate::Result<()> {
    if state.provider_pending {
        // Keys consumed by Hokan write nothing to the shell, so no redisplay
        // follows and no render gate opens on an idle prompt. Re-anchor with a
        // cursor probe so the provider result frame can be admitted.
        output
            .allow_cursor_probe(state.buffer.revision)
            .map_err(output_error)?;
        state.need_cpr = true;
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
) -> crate::Result<ActivationAttempt> {
    let (activation, context) = match resolve_selected_activation(state)? {
        SelectedActivation::None => return Ok(ActivationAttempt::Finished),
        SelectedActivation::Ready {
            activation,
            context,
        } => (activation, context),
        SelectedActivation::Rejected => {
            state.schedule_query(worker)?;
            state.status = Some("HK-CMP-STALE selection expired; candidates refreshed".into());
            render_current(state, output)?;
            return Ok(ActivationAttempt::Rejected);
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
    Ok(ActivationAttempt::Finished)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn retry_pending_accept(
    state: &mut RuntimeState,
    pty: &mut PtyChild,
    session: &ShellSession,
    output: &OutputHandle,
    worker: &ProviderWorker,
    config: &Arc<Config>,
    ai_sender: &Sender<AiResult>,
) -> crate::Result<()> {
    if !state.pending_accept {
        return Ok(());
    }
    state.pending_accept = false;
    if state.candidates.is_empty() {
        if state.provider_pending {
            state.pending_accept = true;
        }
        return Ok(());
    }
    if selected_candidate(state).is_none() {
        state.selected = None;
        move_selection(state, 1);
    }
    if matches!(
        activate_selected(state, pty, session, output, worker, config, ai_sender)?,
        ActivationAttempt::Rejected
    ) {
        state.pending_accept = true;
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
            activate_selected(state, pty, session, output, worker, config, ai_sender).map(|_| ())
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
    state.foreground_process = true;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::input::MAX_PASTE_BYTES;

    fn paste_event(payload: &[u8]) -> InputEvent {
        let mut raw = b"\x1b[200~".to_vec();
        raw.extend_from_slice(payload);
        raw.extend_from_slice(b"\x1b[201~");
        InputEvent {
            kind: InputKind::Paste(payload.to_vec()),
            raw,
        }
    }

    #[test]
    fn bash_readline_receives_only_the_paste_payload() {
        let event = paste_event("粘贴中文🙂e\u{301}👩‍💻".as_bytes());
        let InputKind::Paste(payload) = &event.kind else {
            panic!("fixture should be a paste event");
        };
        assert_eq!(input_bytes_for_shell(&event, false, true), payload);
    }

    #[test]
    fn backspace_dismisses_overlay_only_for_an_empty_buffer() {
        assert!(dismisses_empty_overlay(&InputKind::Backspace, true));
        assert!(!dismisses_empty_overlay(&InputKind::Backspace, false));
        assert!(!dismisses_empty_overlay(&InputKind::Escape, true));
    }

    #[test]
    fn bracketed_paste_stays_raw_for_native_editors_and_foreground_programs() {
        let event = paste_event(b"first\nsecond");
        assert_eq!(input_bytes_for_shell(&event, true, true), event.raw);
        assert_eq!(input_bytes_for_shell(&event, false, false), event.raw);
    }

    #[test]
    fn legacy_readline_strips_only_oversized_paste_boundaries() {
        let first = InputEvent {
            kind: InputKind::PasteFragment {
                strip_start: true,
                strip_end: false,
            },
            raw: b"\x1b[200~first".to_vec(),
        };
        assert_eq!(input_bytes_for_shell(&first, false, true), b"first");

        let middle = InputEvent {
            kind: InputKind::PasteFragment {
                strip_start: false,
                strip_end: false,
            },
            raw: b"\x1b[200~literal\x1b[201~".to_vec(),
        };
        assert_eq!(input_bytes_for_shell(&middle, false, true), middle.raw);

        let last = InputEvent {
            kind: InputKind::PasteFragment {
                strip_start: false,
                strip_end: true,
            },
            raw: b"last\x1b[201~".to_vec(),
        };
        assert_eq!(input_bytes_for_shell(&last, false, true), b"last");
    }

    #[test]
    fn oversized_paste_round_trips_for_legacy_and_native_editors() {
        let payload = vec![b'x'; MAX_PASTE_BYTES + 64 * 1024];
        let mut raw = PASTE_START.to_vec();
        raw.extend_from_slice(&payload);
        raw.extend_from_slice(PASTE_END);

        let mut decoder = InputDecoder::default();
        let mut events = Vec::new();
        for chunk in raw.chunks(16 * 1024) {
            events.extend(decoder.feed(chunk));
        }
        assert!(events.len() > 1, "fixture should exercise streaming");

        let legacy: Vec<_> = events
            .iter()
            .flat_map(|event| input_bytes_for_shell(event, false, true).iter().copied())
            .collect();
        assert_eq!(legacy, payload);

        let native: Vec<_> = events
            .iter()
            .flat_map(|event| input_bytes_for_shell(event, true, true).iter().copied())
            .collect();
        assert_eq!(native, raw);
    }
}
