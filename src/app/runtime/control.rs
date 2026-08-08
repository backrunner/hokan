use std::{
    sync::{Arc, RwLock},
    time::Instant,
};

use super::{
    TERMINAL_QUERY_TIMEOUT,
    engine::build_engine,
    history::{history_control_ignores_space, record_history, sync_history},
    output_error,
    render::{hide_overlay_if_query_suppressed, render_current},
    state::RuntimeState,
    worker::ProviderWorker,
};
use crate::{
    completion::SyncQuality,
    config::{Config, ConfigPaths, ConfigReload, ConfigWatcher},
    history::{HistoryIndex, HistoryPolicy, HistoryStore},
    shell::{ControlMessage, ShellEvent},
    terminal::{OutputHandle, RenderGateRequest},
};

pub(super) fn handle_control_message(
    message: ControlMessage,
    state: &mut RuntimeState,
    output: &OutputHandle,
    worker: &ProviderWorker,
    store: &HistoryStore,
    history: &Arc<RwLock<HistoryIndex>>,
    policy: &HistoryPolicy,
) -> crate::Result<()> {
    match message {
        ControlMessage::Event(ShellEvent::PathChanged { path }) => {
            if state.commands.refresh_from_path(Some(path.as_os_str()))
                && state.editing
                && state.buffer.sync != SyncQuality::Uncertain
                && (!state.buffer.text.trim().is_empty() || state.history_only)
            {
                state.schedule_query(worker)?;
            }
        }
        ControlMessage::Event(ShellEvent::Prompt {
            boundary_id,
            cwd,
            history_control,
        }) => {
            state.cancel_ai();
            state.ignore_leading_space_history = history_control
                .as_deref()
                .is_some_and(history_control_ignores_space);
            if let Some(command) = state.pending_command.take() {
                if !state.ignore_leading_space_history || !command.starts_with(char::is_whitespace)
                {
                    record_history(command, None, state, store, history, policy)?;
                }
            } else {
                sync_history(state, store, history, policy)?;
            }
            state.cwd = cwd;
            state.editing = true;
            state.history_only = false;
            state.need_cpr = true;
            state
                .buffer
                .reset_prompt(if state.shell.exact_buffer_sync() {
                    SyncQuality::Exact
                } else {
                    SyncQuality::Mirrored
                })?;
            state.context = None;
            state.candidates.clear();
            state.selected = None;
            state.selection_intent = None;
            state.provider_pending = false;
            state.overlay_visible = false;
            state.pending_confirm = None;
            state.foreground_process = false;
            state.pending_reanchor = false;
            output.set_foreground(false).map_err(output_error)?;
            output.arm_prompt_gate(boundary_id).map_err(output_error)?;
        }
        ControlMessage::Event(ShellEvent::Buffer {
            redisplay_id,
            cursor,
            text,
        }) => {
            if state.buffer.set_exact(text, cursor)? {
                output
                    .arm_render_gate(RenderGateRequest {
                        boundary_id: redisplay_id,
                        buffer_revision: state.buffer.revision,
                        deadline: Instant::now() + TERMINAL_QUERY_TIMEOUT,
                    })
                    .map_err(output_error)?;
                state.schedule_query(worker)?;
                hide_overlay_if_query_suppressed(state, output)?;
            }
        }
        ControlMessage::Event(ShellEvent::CommandStart { command }) => {
            state.cancel_ai();
            state.pending_command = Some(command);
            state.editing = false;
            state.overlay_visible = false;
            state.pending_confirm = None;
            state.selection_intent = None;
            state.foreground_process = true;
            output.hide_overlay().map_err(output_error)?;
            output.set_foreground(true).map_err(output_error)?;
        }
        ControlMessage::Event(ShellEvent::CommandEnd {
            exit_code,
            cwd,
            command,
        }) => {
            state.cancel_ai();
            state.cwd = cwd;
            state.pending_command = None;
            record_history(command, Some(exit_code), state, store, history, policy)?;
        }
        ControlMessage::Diagnostic(diagnostic) => {
            state.status = Some(format!("{} {}", diagnostic.code, diagnostic.message));
        }
        ControlMessage::BufferUncertain => {
            state.cancel_ai();
            state.buffer.mark_uncertain();
            state.overlay_visible = false;
            state.pending_confirm = None;
            state.selection_intent = None;
            output.hide_overlay().map_err(output_error)?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn handle_config_reload(
    watcher: &mut ConfigWatcher,
    now: Instant,
    config: &mut Arc<Config>,
    paths: &ConfigPaths,
    history: &Arc<RwLock<HistoryIndex>>,
    worker: &ProviderWorker,
    state: &mut RuntimeState,
    output: &OutputHandle,
) -> crate::Result<()> {
    match watcher.poll(now) {
        ConfigReload::Unchanged => return Ok(()),
        ConfigReload::Invalid(error) => {
            if let Some(log) = &state.debug_log {
                log.config_reload("invalid", None);
            }
            state.status = Some(format!(
                "HK-CFG-RELOAD invalid config; keeping last known good: {error}"
            ));
            return render_current(state, output);
        }
        ConfigReload::Loaded(loaded) => {
            if let Some(log) = &state.debug_log {
                log.config_reload("loaded", None);
            }
            watcher.watch_credential_path(
                crate::config::resolve_credential_path(&loaded.ai, &paths.credentials_file)
                    .unwrap_or_else(|| paths.credentials_file.clone()),
            );
            let (live, restart_required) = merge_live_config(config, *loaded);
            let live = Arc::new(live);
            let (engine, specs, _, help, aliases) = build_engine(
                paths,
                &live,
                Arc::clone(history),
                Some(Arc::clone(&state.commands)),
            );
            worker.replace_engine(engine)?;
            state.specs = specs;
            state.aliases = aliases;
            state.help_revision = help.revision();
            state.help = help;
            state.cancel_ai();
            state.overlay_visible = false;
            state.pending_confirm = None;
            state.update_overlay_height(u16::try_from(live.ui.max_rows).unwrap_or(u16::MAX).max(1));
            configure_overlay(output, &live)?;
            *config = live;
            if state.editing
                && state.buffer.sync != SyncQuality::Uncertain
                && (!state.buffer.text.trim().is_empty() || state.history_only)
            {
                state.schedule_query(worker)?;
            }
            state.status = Some(if restart_required.is_empty() {
                "HK-CFG-RELOAD applied provider and UI configuration".into()
            } else {
                format!(
                    "HK-CFG-RESTART restart required for {} changes",
                    restart_required.join(", ")
                )
            });
            render_current(state, output)?;
        }
    }
    Ok(())
}

pub(super) fn merge_live_config(
    current: &Config,
    mut loaded: Config,
) -> (Config, Vec<&'static str>) {
    let mut restart_required = Vec::new();
    if loaded.core != current.core {
        loaded.core = current.core.clone();
        restart_required.push("core");
    }
    if loaded.history != current.history {
        loaded.history = current.history.clone();
        restart_required.push("history");
    }
    if loaded.logging != current.logging {
        loaded.logging = current.logging.clone();
        restart_required.push("logging");
    }
    (loaded, restart_required)
}

pub(super) fn configure_overlay(output: &OutputHandle, config: &Config) -> crate::Result<()> {
    let color = match config.ui.color.as_str() {
        "always" => true,
        "never" => false,
        _ => std::env::var_os("NO_COLOR").is_none(),
    };
    output
        .configure_overlay(
            u16::try_from(config.ui.max_rows).unwrap_or(u16::MAX),
            u16::try_from(config.ui.max_width).unwrap_or(u16::MAX),
            color,
            config.ui.nerd_fonts,
        )
        .map_err(output_error)
}
