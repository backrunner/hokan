mod control;
mod engine;
mod history;
mod input;
mod render;
mod results;
mod state;
mod worker;

#[cfg(test)]
mod tests;

use std::{
    io::{IsTerminal, Read},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use control::*;
use engine::*;
use history::*;
use input::*;
use render::*;
use results::*;
use state::*;
use worker::*;

use crate::{
    config::{Config, ConfigPaths, ConfigWatcher},
    diagnostics::DebugLog,
    pty::{PtyChild, PtyReadPump, SignalBridge},
    shell::{ShellKind, ShellSession},
    terminal::{
        InputDecoder, OutputHandle, OutputJoin, TerminalQueryKind, TerminalReplyRouter,
        TerminalSize,
    },
};
use crossbeam_channel::{Receiver, select, unbounded};
use portable_pty::ExitStatus;

pub(super) const ESCAPE_TIMEOUT: Duration = Duration::from_millis(24);
pub(super) const TERMINAL_QUERY_TIMEOUT: Duration = Duration::from_millis(250);
const LOOP_TICK: Duration = Duration::from_millis(8);

#[derive(Clone, Copy, Debug, Default)]
pub struct SessionOptions {
    pub shell: Option<ShellKind>,
    pub login: bool,
}

pub fn run_session(options: SessionOptions) -> crate::Result<u8> {
    validate_terminal_session()?;
    if std::env::var_os("HOKAN_ACTIVE").is_some() {
        return Err(crate::Error::Runtime(
            "refusing to start a recursive Hokan session".into(),
        ));
    }

    let paths = ConfigPaths::discover()?;
    let mut config = Arc::new(Config::load(&paths.config_file)?);
    // Background self-update: a detached `hokan upgrade --auto` child with all
    // stdio set to null. On Unix stdio separation alone keeps the child alive
    // after the parent exits — no setsid, so no unsafe under
    // `#![forbid(unsafe_code)]`. A helper thread reaps the child so it cannot
    // linger as a zombie for the session's lifetime; the session never waits.
    if should_spawn_auto_update(&config, std::env::var_os("HOKAN_NO_AUTO_UPDATE").is_some()) {
        spawn_auto_update();
    }
    let watched_credential =
        crate::config::resolve_credential_path(&config.ai, &paths.credentials_file)
            .unwrap_or_else(|| paths.credentials_file.clone());
    let mut config_watcher = ConfigWatcher::new(
        paths.config_file.clone(),
        watched_credential,
        Instant::now(),
    );
    let shell = options
        .shell
        .or(config.core.shell)
        .map_or_else(ShellKind::detect, Ok)?;
    let login = options.login || config.core.login_shell;
    let terminal_size = current_terminal_size()?;
    let overlay_height = u16::try_from(config.ui.max_rows).unwrap_or(u16::MAX).max(1);
    let debug_log = DebugLog::from_config(&paths.state_directory, &config.logging)?;
    // Support diagnostics shortcut: `HOKAN_DEBUG_LOG=1` forces the bounded
    // JSONL debug log on without editing the config file.
    let debug_log = debug_log.or_else(|| {
        (std::env::var_os("HOKAN_DEBUG_LOG").is_some_and(|value| value != "0"))
            .then(|| {
                DebugLog::from_config(
                    &paths.state_directory,
                    &crate::config::LoggingConfig {
                        enabled: true,
                        ..config.logging.clone()
                    },
                )
                .ok()
                .flatten()
            })
            .flatten()
    });
    if let Some(log) = &debug_log {
        log.session_started(shell, terminal_size);
    }

    let shell_session = ShellSession::new(shell)?;
    let command = shell_session.command_builder(login)?;
    let (control_sender, control_receiver) = unbounded();
    let control_reader = shell_session.start_control_reader(control_sender)?;
    let mut pty = PtyChild::spawn(command, terminal_size)?;

    let token = shell_session.token();
    let (output_handle, output_join) =
        crate::terminal::spawn_stdout(token, terminal_size, overlay_height)
            .map_err(output_error)?;
    let mut output = OutputLease::new(output_handle, output_join);
    configure_overlay(output.handle(), &config)?;
    if let Some(log) = &debug_log {
        output
            .handle()
            .set_debug_log(Some(log.clone()))
            .map_err(output_error)?;
    }

    let pty_descriptor = pty.enable_nonblocking_reads()?;
    let pty_reader = pty.take_reader()?;
    let (pty_sender, pty_receiver) = unbounded();
    let pty_pump = PtyReadPump::start(
        pty_reader,
        pty_descriptor,
        output.handle().clone(),
        pty_sender,
    )?;
    let (signal_sender, signal_receiver) = unbounded();
    let signal_bridge = SignalBridge::start(signal_sender)?;
    let input_receiver = spawn_input_reader()?;

    let (history_store, history_index, history_policy, history_cursor) =
        load_history(&paths, &config, shell)?;
    let (engine, specs, commands, help) =
        build_engine(&paths, &config, Arc::clone(&history_index), None);
    let worker = ProviderWorker::start(engine, debug_log.clone())?;
    let (ai_sender, ai_receiver) = unbounded();
    let mut state = RuntimeState::new(
        shell,
        terminal_size,
        std::env::current_dir()?,
        shell.exact_buffer_sync(),
        history_cursor,
        overlay_height,
        paths.credentials_file.clone(),
        new_history_session_id()?,
        debug_log,
        commands,
        specs,
        help,
    );
    let mut decoder = InputDecoder::default();
    let mut reply_router = TerminalReplyRouter::default();

    let sync_query = reply_router.register(
        TerminalQueryKind::SynchronizedOutput,
        Instant::now(),
        TERMINAL_QUERY_TIMEOUT,
    )?;
    output
        .handle()
        .probe(sync_query.bytes)
        .map_err(output_error)?;

    let mut exit_status: Option<ExitStatus> = None;
    let mut terminating = false;
    let mut termination_started = None;
    let mut kill_sent = false;
    while exit_status.is_none() {
        select! {
            recv(input_receiver) -> message => {
                if let Ok(bytes) = message {
                    route_terminal_input(
                        &bytes,
                        &mut reply_router,
                        &mut decoder,
                        &mut state,
                        &mut pty,
                        &shell_session,
                        output.handle(),
                        &worker,
                        &config,
                        &ai_sender,
                    )?;
                }
            }
            recv(control_receiver) -> message => {
                if let Ok(message) = message {
                    handle_control_message(
                        message,
                        &mut state,
                        output.handle(),
                        &worker,
                        &history_store,
                        &history_index,
                        &history_policy,
                    )?;
                }
            }
            recv(pty_receiver) -> message => {
                if let Ok(message) = message {
                    handle_pty_event(message, &mut state, output.handle())?;
                }
            }
            recv(signal_receiver) -> message => {
                if let Ok(message) = message {
                    terminating |= handle_signal(message, &mut state, &mut pty, output.handle())?;
                }
            }
            recv(worker.results()) -> message => {
                if let Ok(result) = message {
                    handle_provider_result(result, &mut state, output.handle())?;
                }
            }
            recv(ai_receiver) -> message => {
                if let Ok(result) = message {
                    handle_ai_result(result, &mut state, output.handle())?;
                }
            }
            default(LOOP_TICK) => {}
        }

        let now = Instant::now();
        if decoder.has_pending_ambiguity() && state.escape_deadline.is_none() {
            state.escape_deadline = Some(now + ESCAPE_TIMEOUT);
        }
        if state
            .escape_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            state.escape_deadline = None;
            if let Some(event) = decoder.flush_ambiguous() {
                handle_input_event(
                    event,
                    &mut state,
                    &mut pty,
                    &shell_session,
                    output.handle(),
                    &worker,
                    &config,
                    &ai_sender,
                )?;
            }
        }
        let expired = reply_router.expire(now);
        for reply in expired.replies {
            if handle_terminal_reply(reply, output.handle())? {
                render_current(&mut state, output.handle())?;
            }
        }
        for event in decoder.feed(&expired.input) {
            handle_input_event(
                event,
                &mut state,
                &mut pty,
                &shell_session,
                output.handle(),
                &worker,
                &config,
                &ai_sender,
            )?;
        }
        maybe_probe_cursor(&mut state, &mut reply_router, output.handle())?;
        state.refresh_help_results(&worker)?;
        flush_scheduled_frame(&mut state, output.handle())?;
        flush_pending_history(&mut state, &history_store)?;
        handle_config_reload(
            &mut config_watcher,
            now,
            &mut config,
            &paths,
            &history_index,
            &worker,
            &mut state,
            output.handle(),
        )?;
        detect_foreground_process(&mut state, &pty, output.handle())?;
        exit_status = pty.try_wait()?;
        if terminating {
            let started = *termination_started.get_or_insert(now);
            if !kill_sent
                && exit_status.is_none()
                && now.saturating_duration_since(started) >= Duration::from_secs(1)
            {
                pty.kill()?;
                kill_sent = true;
            }
        }
    }

    flush_history_before_exit(&mut state, &history_store);
    pty.close_writer();
    state.cancel_ai();
    pty_pump.join()?;
    let _ = output.handle().barrier();
    output.finish()?;
    drop(signal_bridge);
    drop(control_reader);
    drop(worker);
    let exit_code = exit_status.map_or(1, |status| status.exit_code().min(255) as u8);
    let leave_requested = shell_session.leave_requested()?;
    if leave_requested {
        shell_session.record_integration_leave()?;
    }
    if let Some(log) = &state.debug_log {
        log.session_finished(exit_code);
    }
    Ok(if leave_requested { 0 } else { exit_code })
}

/// Whether session start should spawn the detached `upgrade --auto` child:
/// auto-update must be enabled in the config and not opted out via
/// `HOKAN_NO_AUTO_UPDATE` (the env check is injected so tests stay pure).
fn should_spawn_auto_update(config: &Config, no_auto_update_env: bool) -> bool {
    config.update.enabled && !no_auto_update_env
}

/// Spawns `current_exe upgrade --auto` fully detached and never waits on it.
/// Spawn failure is silent by design: updates must never disturb a session.
fn spawn_auto_update() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let spawned = std::process::Command::new(exe)
        .arg("upgrade")
        .arg("--auto")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Ok(mut child) = spawned {
        // Reap on a helper thread so the short-lived child cannot linger as a
        // zombie; the session loop itself never blocks on it.
        let _ = thread::Builder::new()
            .name("hokan-auto-update".into())
            .spawn(move || {
                let _ = child.wait();
            });
    }
}

fn validate_terminal_session() -> crate::Result<()> {
    if !std::io::stdin().is_terminal() || !crate::terminal::process_stdout_is_terminal() {
        return Err(crate::Error::Runtime(
            "hokan requires terminal stdin and stdout".into(),
        ));
    }
    if std::env::var("TERM").ok().as_deref() == Some("dumb") {
        return Err(crate::Error::Runtime(
            "TERM=dumb does not support the Hokan overlay".into(),
        ));
    }
    Ok(())
}

pub(super) fn current_terminal_size() -> crate::Result<TerminalSize> {
    let (cols, rows) = crossterm::terminal::size()?;
    TerminalSize::new(rows, cols)
}

fn spawn_input_reader() -> crate::Result<Receiver<Vec<u8>>> {
    let (sender, receiver) = unbounded();
    thread::Builder::new()
        .name("hokan-stdin".into())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut bytes = vec![0_u8; 16 * 1024];
            loop {
                match stdin.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(count) => {
                        if sender.send(bytes[..count].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })?;
    Ok(receiver)
}

pub(super) fn output_error(error: crate::terminal::OutputError) -> crate::Error {
    crate::Error::Runtime(error.to_string())
}

struct OutputLease {
    handle: OutputHandle,
    join: Option<OutputJoin<std::io::Stdout>>,
}

impl OutputLease {
    const fn new(handle: OutputHandle, join: OutputJoin<std::io::Stdout>) -> Self {
        Self {
            handle,
            join: Some(join),
        }
    }

    const fn handle(&self) -> &OutputHandle {
        &self.handle
    }

    fn finish(&mut self) -> crate::Result<()> {
        self.handle.restore_and_exit().map_err(output_error)?;
        if let Some(join) = self.join.take() {
            join.join()
                .map_err(|_| crate::Error::Runtime("output actor panicked".into()))?
                .map_err(output_error)?;
        }
        Ok(())
    }
}

impl Drop for OutputLease {
    fn drop(&mut self) {
        let _ = self.handle.restore_and_exit();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
