use std::{
    cell::Cell,
    collections::{HashSet, VecDeque},
    io::{IsTerminal, Read, Seek, SeekFrom},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use super::buffer::{EditableBuffer, MirrorOutcome};
use crate::{
    ai::{AiClient, build_context},
    completion::{
        Activation, BufferSnapshot, Candidate, CandidateAction, CandidateSource, Completeness,
        CompletionContext, CompletionEngine, ProviderOutput, SyncQuality, activate_candidate,
        rank_and_dedupe, stricter_risk,
    },
    config::{Config, ConfigPaths, ConfigReload, ConfigWatcher},
    diagnostics::DebugLog,
    history::{
        HistoryCursor, HistoryEventV1, HistoryIndex, HistoryPolicy, HistoryStore,
        default_history_path, parse_history,
    },
    platform::CommandPathCache,
    project::ProjectCache,
    providers::{
        AiActionProvider, CommandSpecProvider, FilesystemProvider, HistoryProvider,
        NetworkInterfaceProvider, PathCommandProvider, ProcessProvider, ProjectProvider,
        ai_result_candidates,
    },
    pty::{PtyChild, PtyReadEvent, PtyReadPump, SignalBridge, SignalEvent},
    safety::classify_command,
    shell::{
        ControlMessage, ShellEvent, ShellKind, ShellSession, accept_sequence, replacement_sequence,
    },
    specs::SpecRegistry,
    terminal::{
        BufferRevision, FrameRequest, FrameRevision, FrameTicket, InputDecoder, InputEvent,
        InputKind, LatestFrameScheduler, OutputHandle, OutputJoin, OverlayRow, OverlayView,
        QueryId, RenderGateRequest, RenderReadiness, SanitizedText, SurfaceKey,
        SyncOutputCapability, TerminalQueryKind, TerminalReply, TerminalReplyRouter, TerminalSize,
        WidthPolicy,
    },
};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select, unbounded};
use nix::fcntl::OFlag;
use portable_pty::ExitStatus;
use tokio_util::sync::CancellationToken;

const ESCAPE_TIMEOUT: Duration = Duration::from_millis(24);
const TERMINAL_QUERY_TIMEOUT: Duration = Duration::from_millis(250);
const LOOP_TICK: Duration = Duration::from_millis(8);
const HISTORY_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const HISTORY_COMPACTION_THRESHOLD_BYTES: u64 = 4 * 1024 * 1024;
const SHELL_HISTORY_STARTUP_MAX_BYTES: u64 = 8 * 1024 * 1024;

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
    let (engine, _specs, _commands) = build_engine(&paths, &config, Arc::clone(&history_index));
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
    if let Some(log) = &state.debug_log {
        log.session_finished(exit_code);
    }
    Ok(exit_code)
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

fn current_terminal_size() -> crate::Result<TerminalSize> {
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

fn load_history(
    paths: &ConfigPaths,
    config: &Config,
    shell: ShellKind,
) -> crate::Result<(
    HistoryStore,
    Arc<RwLock<HistoryIndex>>,
    HistoryPolicy,
    HistoryCursor,
)> {
    let store = HistoryStore::open(&paths.state_directory)?;
    let policy = HistoryPolicy::new(config.history.max_command_bytes, &config.history.exclude)?;
    let mut index = HistoryIndex::default();
    let (mut report, mut cursor) = store.read_with_cursor()?;
    if report.snapshot_corrupt || report.corrupt_offset.is_some() {
        store.quarantine_corrupt()?;
        (report, cursor) = store.read_with_cursor()?;
    }
    if report.torn_tail {
        store.repair_torn_tail()?;
        (report, cursor) = store.read_with_cursor()?;
    }
    for event in report.events {
        index.ingest_weighted(
            &event.command,
            event.timestamp_ms,
            event.shell,
            event.cwd.as_deref(),
            event.occurrences,
            event.exit_code,
            &policy,
        );
    }
    if config.history.enabled
        && let Some(path) = default_history_path(shell)
        && let Ok(bytes) = read_history_tail(&path, SHELL_HISTORY_STARTUP_MAX_BYTES)
    {
        let now = crate::history_now_ms();
        // Ingest in chronological order so the transition bigram learns the
        // real command sequences; the timestamp assignment matches the old
        // newest-first enumeration exactly.
        let imported = parse_history(shell, &bytes);
        let total = imported.len();
        for (offset, imported) in imported.into_iter().enumerate() {
            let timestamp = imported
                .timestamp_ms
                .unwrap_or_else(|| now.saturating_sub((total - 1 - offset) as i64));
            index.ingest(&imported.command, timestamp, shell, None, None, &policy);
        }
    }
    Ok((store, Arc::new(RwLock::new(index)), policy, cursor))
}

fn read_history_tail(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    if max_bytes == 0 {
        return Ok(Vec::new());
    }
    let path = std::fs::canonicalize(path)?;
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits());
    let mut file = options.open(&path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not a regular history file", path.display()),
        ));
    }
    let length = metadata.len();
    let logical_start = length.saturating_sub(max_bytes);
    let read_start = logical_start.saturating_sub(u64::from(logical_start > 0));
    file.seek(SeekFrom::Start(read_start))?;
    let read_limit = max_bytes.saturating_add(u64::from(logical_start > 0));
    let capacity = usize::try_from(read_limit.min(length)).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    Read::by_ref(&mut file)
        .take(read_limit)
        .read_to_end(&mut bytes)?;

    if logical_start > 0 {
        if bytes.first() == Some(&b'\n') {
            bytes.remove(0);
        } else if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            bytes.drain(..=newline);
        } else {
            bytes.clear();
        }
    }
    Ok(bytes)
}

fn build_engine(
    paths: &ConfigPaths,
    config: &Arc<Config>,
    history: Arc<RwLock<HistoryIndex>>,
) -> (
    Arc<CompletionEngine>,
    Arc<SpecRegistry>,
    Arc<CommandPathCache>,
) {
    let specs = Arc::new(SpecRegistry::load(Some(&paths.specs_directory)));
    let commands = Arc::new(CommandPathCache::from_environment());
    let projects = Arc::new(ProjectCache::default());
    let mut engine = CompletionEngine::new(config.completion.max_candidates, config.ui.max_rows)
        .with_local_timeout(Duration::from_millis(config.completion.local_timeout_ms));
    if config.history.enabled {
        engine.register(HistoryProvider::new(history));
    }
    engine.register(CommandSpecProvider::new(
        Arc::clone(&specs),
        Arc::clone(&commands),
    ));
    engine.register(ProjectProvider::new(projects, Arc::clone(&commands)));
    engine.register(FilesystemProvider::new(config.ui.show_hidden));
    engine.register(PathCommandProvider::new(Arc::clone(&commands)));
    engine.register(ProcessProvider);
    engine.register(NetworkInterfaceProvider);
    engine.register(AiActionProvider::new(
        Arc::new(config.ai.clone()),
        crate::config::credential_available(&config.ai, &paths.credentials_file),
        Arc::clone(&commands),
        Arc::clone(&specs),
    ));
    (Arc::new(engine), specs, commands)
}

#[allow(clippy::too_many_arguments)]
fn handle_config_reload(
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
            let (engine, _, _) = build_engine(paths, &live, Arc::clone(history));
            worker.replace_engine(engine)?;
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

fn merge_live_config(current: &Config, mut loaded: Config) -> (Config, Vec<&'static str>) {
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

fn configure_overlay(output: &OutputHandle, config: &Config) -> crate::Result<()> {
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

struct RuntimeState {
    shell: ShellKind,
    terminal_size: TerminalSize,
    cwd: PathBuf,
    buffer: EditableBuffer,
    query_id: QueryId,
    context: Option<Arc<CompletionContext>>,
    candidates: Vec<Candidate>,
    selected: Option<crate::completion::CandidateId>,
    selection_intent: Option<SelectionIntent>,
    history_only: bool,
    provider_pending: bool,
    overlay_visible: bool,
    frame_revision: FrameRevision,
    editing: bool,
    pending_mirror_revision: Option<BufferRevision>,
    status: Option<String>,
    escape_deadline: Option<Instant>,
    need_cpr: bool,
    pending_command: Option<String>,
    /// Last command recorded as executed in this session; feeds the
    /// transition bigram signal on the next completion query.
    previous_command: Option<String>,
    workspace_probe: crate::project::WorkspaceProbe,
    pending_confirm: Option<PendingConfirm>,
    ai_query: Option<ActiveAiRequest>,
    foreground_process: bool,
    suspended: bool,
    pending_reanchor: bool,
    history_cursor: HistoryCursor,
    pending_history: VecDeque<HistoryEventV1>,
    local_history_ids: HashSet<String>,
    history_retry_at: Instant,
    history_session_id: String,
    history_sequence: u64,
    history_compaction: Arc<AtomicBool>,
    ignore_leading_space_history: bool,
    page_size: usize,
    max_overlay_height: u16,
    credentials_file: PathBuf,
    scheduler: LatestFrameScheduler<FrameRequest>,
    theme_revision: u64,
    debug_log: Option<DebugLog>,
}

impl RuntimeState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        shell: ShellKind,
        terminal_size: TerminalSize,
        cwd: PathBuf,
        exact: bool,
        history_cursor: HistoryCursor,
        max_overlay_height: u16,
        credentials_file: PathBuf,
        history_session_id: String,
        debug_log: Option<DebugLog>,
    ) -> Self {
        Self {
            shell,
            terminal_size,
            cwd,
            buffer: EditableBuffer::new(if exact {
                SyncQuality::Exact
            } else {
                SyncQuality::Mirrored
            }),
            query_id: QueryId::ZERO,
            context: None,
            candidates: Vec::new(),
            selected: None,
            selection_intent: None,
            history_only: false,
            provider_pending: false,
            overlay_visible: false,
            frame_revision: FrameRevision::ZERO,
            editing: false,
            pending_mirror_revision: None,
            status: None,
            escape_deadline: None,
            need_cpr: false,
            pending_command: None,
            previous_command: None,
            workspace_probe: crate::project::WorkspaceProbe::default(),
            pending_confirm: None,
            ai_query: None,
            foreground_process: false,
            suspended: false,
            pending_reanchor: false,
            history_cursor,
            pending_history: VecDeque::new(),
            local_history_ids: HashSet::new(),
            history_retry_at: Instant::now(),
            history_session_id,
            history_sequence: 0,
            history_compaction: Arc::new(AtomicBool::new(false)),
            ignore_leading_space_history: false,
            page_size: visible_page_size(max_overlay_height, terminal_size),
            max_overlay_height,
            credentials_file,
            scheduler: LatestFrameScheduler::new(60),
            theme_revision: 1,
            debug_log,
        }
    }

    fn update_terminal_size(&mut self, size: TerminalSize) {
        self.terminal_size = size;
        self.page_size = visible_page_size(self.max_overlay_height, size);
    }

    fn update_overlay_height(&mut self, max_overlay_height: u16) {
        self.max_overlay_height = max_overlay_height.max(1);
        self.page_size = visible_page_size(self.max_overlay_height, self.terminal_size);
        self.theme_revision = self.theme_revision.saturating_add(1);
    }

    fn snapshot(&self) -> crate::Result<BufferSnapshot> {
        BufferSnapshot::new(
            Arc::<str>::from(self.buffer.text.as_str()),
            self.buffer.cursor,
            self.buffer.revision,
            self.buffer.sync,
        )
    }

    fn schedule_query(&mut self, worker: &ProviderWorker) -> crate::Result<()> {
        self.cancel_ai();
        self.status = None;
        self.pending_confirm = None;
        if self.buffer.sync == SyncQuality::Uncertain
            || (self.buffer.text.trim().is_empty() && !self.history_only)
        {
            self.context = None;
            self.candidates.clear();
            self.selected = None;
            self.overlay_visible = false;
            self.provider_pending = false;
            return Ok(());
        }
        // The previous candidates stay visible and routable while the new
        // query is in flight: queued buffer events arrive in bursts, and
        // clearing here would flap the overlay closed mid-burst — leaking
        // navigation keys to the shell and wiping selections made against
        // the still-visible list. Stale rows lose activation eligibility
        // through `resolve_selected_activation`, and the next provider
        // result replaces them.
        self.query_id = self
            .query_id
            .checked_next()
            .ok_or_else(|| crate::Error::Runtime("query id exhausted".into()))?;
        let context = Arc::new(
            CompletionContext::new(
                self.query_id,
                self.shell,
                self.cwd.clone(),
                self.snapshot()?,
            )?
            .with_previous_command(self.previous_command.clone())
            .with_workspace(self.workspace_probe.markers(&self.cwd)),
        );
        self.context = Some(Arc::clone(&context));
        self.provider_pending = true;
        worker.schedule(context)
    }

    fn cancel_ai(&mut self) {
        if let Some(request) = self.ai_query.take() {
            request.cancel.cancel();
            if let Some(log) = &self.debug_log {
                log.ai_event("cancelled");
            }
        }
    }

    fn ai_query_id(&self) -> Option<QueryId> {
        self.ai_query.as_ref().map(|request| request.query_id)
    }

    fn next_history_event_id(&mut self) -> crate::Result<String> {
        self.history_sequence = self
            .history_sequence
            .checked_add(1)
            .ok_or_else(|| crate::Error::History("session history sequence exhausted".into()))?;
        Ok(format!(
            "{}:{:016x}",
            self.history_session_id, self.history_sequence
        ))
    }
}

struct ActiveAiRequest {
    query_id: QueryId,
    cancel: CancellationToken,
}

/// A dangerous candidate execution awaiting explicit confirmation: the full
/// final command text plus the effective risk that triggered the prompt.
struct PendingConfirm {
    text: String,
    risk: crate::terminal::RiskLevel,
    reasons: Vec<String>,
}

/// Number of candidate rows visible per page: the overlay box height minus
/// its two border rows (pagination lives in the top border, status/hints in
/// the bottom border, so no item row is reserved).
fn visible_page_size(max_overlay_height: u16, terminal_size: TerminalSize) -> usize {
    let height = max_overlay_height
        .min(terminal_size.rows.saturating_sub(1))
        .max(1);
    usize::from(height.saturating_sub(2).max(1))
}

struct ProviderResult {
    context: Arc<CompletionContext>,
    output: ProviderOutput,
    final_batch: bool,
}

struct ProviderWorker {
    sender: Option<Sender<Arc<CompletionContext>>>,
    pending: Receiver<Arc<CompletionContext>>,
    results: Receiver<ProviderResult>,
    latest_query: Arc<AtomicU64>,
    engine: Arc<RwLock<Arc<CompletionEngine>>>,
    join: Option<JoinHandle<()>>,
}

impl ProviderWorker {
    fn start(engine: Arc<CompletionEngine>, debug_log: Option<DebugLog>) -> crate::Result<Self> {
        let (sender, receiver) = bounded::<Arc<CompletionContext>>(1);
        let pending = receiver.clone();
        let (result_sender, results) = unbounded();
        let latest_query = Arc::new(AtomicU64::new(0));
        let worker_latest_query = Arc::clone(&latest_query);
        let engine = Arc::new(RwLock::new(engine));
        let worker_engine = Arc::clone(&engine);
        let join = thread::Builder::new()
            .name("hokan-providers".into())
            .spawn(move || {
                while let Ok(context) = receiver.recv() {
                    let engine = match worker_engine.read() {
                        Ok(engine) => Arc::clone(&engine),
                        Err(_) => break,
                    };
                    let query_id = context.query_id.get();
                    let disconnected = Cell::new(false);
                    engine.complete_incremental_with_metrics(
                        &context,
                        |output, final_batch| {
                            if result_sender
                                .send(ProviderResult {
                                    context: Arc::clone(&context),
                                    output,
                                    final_batch,
                                })
                                .is_err()
                            {
                                disconnected.set(true);
                            }
                        },
                        || {
                            disconnected.get()
                                || worker_latest_query.load(Ordering::Acquire) != query_id
                        },
                        |metric| {
                            if let Some(log) = &debug_log {
                                log.provider_finished(
                                    metric.provider,
                                    metric.duration,
                                    metric.candidate_count,
                                    metric.cancelled,
                                );
                            }
                        },
                    );
                    if disconnected.get() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            pending,
            results,
            latest_query,
            engine,
            join: Some(join),
        })
    }

    fn schedule(&self, context: Arc<CompletionContext>) -> crate::Result<()> {
        let Some(sender) = self.sender.as_ref() else {
            return Err(crate::Error::Runtime("provider worker is closed".into()));
        };
        self.latest_query
            .store(context.query_id.get(), Ordering::Release);
        match sender.try_send(context) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(context)) => {
                let _ = self.pending.try_recv();
                sender
                    .try_send(context)
                    .map_err(|_| crate::Error::Runtime("provider worker is unavailable".into()))
            }
            Err(TrySendError::Disconnected(_)) => {
                Err(crate::Error::Runtime("provider worker is closed".into()))
            }
        }
    }

    const fn results(&self) -> &Receiver<ProviderResult> {
        &self.results
    }

    fn replace_engine(&self, engine: Arc<CompletionEngine>) -> crate::Result<()> {
        *self
            .engine
            .write()
            .map_err(|_| crate::Error::Runtime("provider engine was poisoned".into()))? = engine;
        Ok(())
    }
}

impl Drop for ProviderWorker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct AiResult {
    query_id: QueryId,
    result: Result<Vec<crate::ai::AiCommand>, crate::ai::AiClientError>,
}

/// A navigation the user made against the overlay, kept across query changes.
///
/// Buffer events queued behind user input each create a new query whose
/// candidates carry fresh ids, so the old id-based carry-over cannot survive
/// the catch-up: without this, an Up/Down press that landed while the app was
/// catching up would be silently lost. The intent is re-applied to fresh
/// provider results by content identity first (the same command keeps its
/// selection), falling back to the navigation delta against the fresh list
/// (Down from nothing selects the first row again). Any further real keypress
/// clears it, so typing after selecting keeps the existing "selection
/// cleared" behavior.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SelectionIntent {
    delta: isize,
    key: SelectionKey,
}

/// Content identity of a candidate row: two queries for different buffer
/// revisions agree on it while per-query candidate ids never match.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SelectionKey {
    source: crate::completion::CandidateSource,
    primary: String,
    replacement: Option<String>,
}

impl SelectionKey {
    fn of(candidate: &Candidate) -> Self {
        Self {
            source: candidate.source,
            primary: candidate.display.primary.clone(),
            replacement: candidate.edit.as_ref().map(|edit| edit.replacement.clone()),
        }
    }

    fn matches(&self, candidate: &Candidate) -> bool {
        self.source == candidate.source
            && self.primary == candidate.display.primary
            && self.replacement.as_ref() == candidate.edit.as_ref().map(|edit| &edit.replacement)
    }
}

#[allow(clippy::too_many_arguments)]
fn route_terminal_input(
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
fn handle_input_event(
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
            let text = "hokan config ai".to_owned();
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

enum SelectedActivation {
    None,
    Ready {
        activation: Activation,
        context: Arc<CompletionContext>,
    },
    Rejected,
}

/// Outcome of pressing the activate key (Enter) with a selection.
enum EnterResolution {
    /// Non-runnable candidates degrade to the Tab behavior: edit-back fill
    /// for insertions, the action itself for AI/configure/retry rows — never
    /// a shell execution.
    Fill,
    /// Runnable and safe enough: execute immediately.
    Execute(String),
    /// Runnable but dangerous: ask for confirmation first.
    Confirm {
        text: String,
        risk: crate::terminal::RiskLevel,
        reasons: Vec<String>,
    },
}

fn resolve_enter(candidate: &Candidate, activation: &Activation) -> EnterResolution {
    let executable = matches!(candidate.action, CandidateAction::Insert)
        && matches!(candidate.completeness, Completeness::Runnable);
    if !executable {
        return EnterResolution::Fill;
    }
    let Activation::ReplaceBuffer { text, .. } = activation else {
        return EnterResolution::Fill;
    };
    let assessed = classify_command(text);
    let risk = stricter_risk(candidate.risk, assessed.level);
    if matches!(
        risk,
        crate::terminal::RiskLevel::High | crate::terminal::RiskLevel::Unknown
    ) {
        EnterResolution::Confirm {
            text: text.clone(),
            risk,
            reasons: assessed
                .reasons
                .iter()
                .map(|reason| reason.describe().to_owned())
                .collect(),
        }
    } else {
        EnterResolution::Execute(text.clone())
    }
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

fn resolve_selected_activation(state: &RuntimeState) -> crate::Result<SelectedActivation> {
    let Some(candidate) = selected_candidate(state) else {
        return Ok(SelectedActivation::None);
    };
    let Some(context) = state.context.as_ref().cloned() else {
        return Ok(SelectedActivation::None);
    };
    Ok(
        match activate_candidate(candidate, &context, &state.snapshot()?) {
            Ok(activation) => SelectedActivation::Ready {
                activation,
                context,
            },
            Err(_) => SelectedActivation::Rejected,
        },
    )
}

fn start_ai_request(
    state: &mut RuntimeState,
    context: &Arc<CompletionContext>,
    config: &Arc<Config>,
    ai_sender: &Sender<AiResult>,
    output: &OutputHandle,
) -> crate::Result<()> {
    state.cancel_ai();
    let ai_context = build_context(
        &context.buffer.text,
        &config.ai.trigger_prefix,
        state.shell,
        &state.cwd,
        config.ai.send_cwd_basename,
    );
    let query_id = context.query_id;
    let ai_config = config.ai.clone();
    let credential_path = state.credentials_file.clone();
    let sender = ai_sender.clone();
    let cancel = CancellationToken::new();
    let request_cancel = cancel.clone();
    thread::Builder::new()
        .name("hokan-ai".into())
        .spawn(move || {
            let result = AiClient::new(&ai_config, &credential_path).and_then(|client| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| crate::ai::AiClientError::Configuration)?
                    .block_on(client.request(&ai_context, &request_cancel))
            });
            let _ = sender.send(AiResult { query_id, result });
        })?;
    state.ai_query = Some(ActiveAiRequest { query_id, cancel });
    if let Some(log) = &state.debug_log {
        log.ai_event("started");
    }
    state.candidates.clear();
    state.selected = None;
    state.status = Some("HK-AI-WAIT requesting commands; Esc cancels".into());
    render_current(state, output)
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

fn handle_control_message(
    message: ControlMessage,
    state: &mut RuntimeState,
    output: &OutputHandle,
    worker: &ProviderWorker,
    store: &HistoryStore,
    history: &Arc<RwLock<HistoryIndex>>,
    policy: &HistoryPolicy,
) -> crate::Result<()> {
    match message {
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

fn record_history(
    command: String,
    exit_code: Option<i32>,
    state: &mut RuntimeState,
    store: &HistoryStore,
    history: &Arc<RwLock<HistoryIndex>>,
    policy: &HistoryPolicy,
) -> crate::Result<()> {
    let timestamp_ms = crate::history_now_ms();
    state.previous_command = Some(command.clone());
    if !policy.allows(&command) {
        return Ok(());
    }
    let event = HistoryEventV1 {
        event_id: Some(state.next_history_event_id()?),
        timestamp_ms,
        command: command.clone(),
        cwd: Some(state.cwd.clone()),
        shell: state.shell,
        exit_code,
        imported: false,
        occurrences: 1,
    };
    {
        let mut index = history
            .write()
            .map_err(|_| crate::Error::History("history index was poisoned".into()))?;
        index.ingest_weighted(
            &event.command,
            event.timestamp_ms,
            event.shell,
            event.cwd.as_deref(),
            event.occurrences,
            event.exit_code,
            policy,
        );
    }
    if let Some(event_id) = &event.event_id {
        state.local_history_ids.insert(event_id.clone());
    }
    state.pending_history.push_back(event);
    flush_pending_history(state, store)?;
    sync_history(state, store, history, policy)
}

fn sync_history(
    state: &mut RuntimeState,
    store: &HistoryStore,
    history: &Arc<RwLock<HistoryIndex>>,
    policy: &HistoryPolicy,
) -> crate::Result<()> {
    flush_pending_history(state, store)?;
    let Some(mut delta) = store.try_read_since(state.history_cursor)? else {
        state.history_retry_at = Instant::now() + HISTORY_RETRY_INTERVAL;
        return Ok(());
    };
    if delta.snapshot_corrupt || delta.corrupt_offset.is_some() {
        store.quarantine_corrupt()?;
        let (report, cursor) = store.read_with_cursor()?;
        delta.events = report.events;
        delta.cursor = cursor;
        delta.reset = true;
        delta.torn_tail = report.torn_tail;
    }
    if delta.torn_tail {
        store.repair_torn_tail()?;
    }
    let mut index = history
        .write()
        .map_err(|_| crate::Error::History("history index was poisoned".into()))?;
    if delta.reset {
        index.merge_events_absolute(&delta.events, policy);
        if state.pending_history.is_empty() {
            state.local_history_ids.clear();
        }
    } else {
        for event in &delta.events {
            if event
                .event_id
                .as_ref()
                .is_some_and(|event_id| state.local_history_ids.remove(event_id))
            {
                continue;
            }
            index.ingest_weighted(
                &event.command,
                event.timestamp_ms,
                event.shell,
                event.cwd.as_deref(),
                event.occurrences,
                event.exit_code,
                policy,
            );
        }
    }
    state.history_cursor = delta.cursor;
    Ok(())
}

fn flush_pending_history(state: &mut RuntimeState, store: &HistoryStore) -> crate::Result<()> {
    if state.pending_history.is_empty() || Instant::now() < state.history_retry_at {
        return Ok(());
    }
    maybe_start_history_compaction(state, store)?;
    let batch: Vec<_> = state.pending_history.iter().cloned().collect();
    match store.try_append_many(&batch) {
        Ok(true) => {
            state.pending_history.clear();
            state.history_retry_at = Instant::now();
            maybe_start_history_compaction(state, store)?;
        }
        Ok(false) => {
            state.history_retry_at = Instant::now() + HISTORY_RETRY_INTERVAL;
        }
        Err(error) => {
            state.history_retry_at = Instant::now() + HISTORY_RETRY_INTERVAL;
            state.status = Some(format!("HK-HIS-WRITE queued for retry: {error}"));
        }
    }
    Ok(())
}

fn maybe_start_history_compaction(
    state: &mut RuntimeState,
    store: &HistoryStore,
) -> crate::Result<()> {
    if !store.needs_compaction(HISTORY_COMPACTION_THRESHOLD_BYTES)
        || state.history_compaction.swap(true, Ordering::AcqRel)
    {
        return Ok(());
    }
    let store = store.clone();
    let active = Arc::clone(&state.history_compaction);
    if let Err(error) = thread::Builder::new()
        .name("hokan-history-compact".into())
        .spawn(move || {
            let _ = store.compact();
            active.store(false, Ordering::Release);
        })
    {
        state.history_compaction.store(false, Ordering::Release);
        return Err(error.into());
    }
    Ok(())
}

fn flush_history_before_exit(state: &mut RuntimeState, store: &HistoryStore) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while !state.pending_history.is_empty() && Instant::now() < deadline {
        state.history_retry_at = Instant::now();
        let _ = flush_pending_history(state, store);
        if !state.pending_history.is_empty() {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn new_history_session_id() -> crate::Result<String> {
    let mut bytes = [0_u8; 12];
    getrandom::fill(&mut bytes)
        .map_err(|error| crate::Error::History(format!("random session id failed: {error}")))?;
    let mut id = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut id, "{byte:02x}").map_err(|error| crate::Error::History(error.to_string()))?;
    }
    Ok(id)
}

fn history_control_ignores_space(value: &str) -> bool {
    value
        .split(':')
        .any(|setting| matches!(setting, "ignorespace" | "ignoreboth"))
}

fn handle_pty_event(
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

fn handle_signal(
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

fn handle_provider_result(
    mut result: ProviderResult,
    state: &mut RuntimeState,
    output: &OutputHandle,
) -> crate::Result<()> {
    let Some(current) = state.context.as_ref() else {
        return Ok(());
    };
    if result.context.query_id != current.query_id
        || result.context.buffer.revision != state.buffer.revision
        || result.context.buffer.hash != state.snapshot()?.hash
    {
        return Ok(());
    }
    if state.history_only {
        result
            .output
            .candidates
            .retain(|candidate| candidate.source == CandidateSource::History);
    }
    // No implicit selection: the first row is never pre-selected, but a
    // selection the user already made survives batches while the candidate
    // id is still present. When queued buffer events moved the query on and
    // the user's navigation never reached the screen, re-apply the last
    // navigation intent against the fresh list instead of silently losing
    // the keypress: same content keeps its row, otherwise the delta lands
    // where it would have on the new list.
    let previous = state.selected;
    state.candidates = result.output.candidates;
    state.selected = previous
        .filter(|id| state.candidates.iter().any(|candidate| candidate.id == *id))
        .or_else(|| {
            let intent = state.selection_intent.as_ref()?;
            state
                .candidates
                .iter()
                .find(|candidate| intent.key.matches(candidate))
                .map(|candidate| candidate.id)
                .or_else(|| {
                    (!state.candidates.is_empty()).then(|| {
                        let index =
                            landing_row(state.candidates.len(), state.page_size, intent.delta);
                        state.candidates[index].id
                    })
                })
        });
    state.provider_pending = !result.final_batch;
    state.status = result
        .output
        .diagnostics
        .first()
        .map(|diagnostic| format!("{} {}", diagnostic.code, diagnostic.message));
    if state.candidates.is_empty() && state.status.is_none() {
        state.overlay_visible = false;
        output.hide_overlay().map_err(output_error)?;
    } else {
        render_current(state, output)?;
    }
    Ok(())
}

fn handle_ai_result(
    result: AiResult,
    state: &mut RuntimeState,
    output: &OutputHandle,
) -> crate::Result<()> {
    if state.ai_query_id() != Some(result.query_id) {
        return Ok(());
    }
    let Some(context) = state.context.as_ref() else {
        return Ok(());
    };
    let _ = state.ai_query.take();
    state.selection_intent = None;
    match result.result {
        Ok(commands) => {
            if let Some(log) = &state.debug_log {
                log.ai_event("succeeded");
            }
            state.candidates = rank_and_dedupe(context, ai_result_candidates(context, commands), 5);
            state.selected = None;
            state.status = None;
        }
        Err(error) => {
            if let Some(log) = &state.debug_log {
                log.ai_event(error.code());
            }
            let configure = matches!(
                error,
                crate::ai::AiClientError::Configuration
                    | crate::ai::AiClientError::MissingCredential
                    | crate::ai::AiClientError::CredentialRejected
                    | crate::ai::AiClientError::Unauthorized
            );
            let message = format!("{} {error}", error.code());
            let candidate = crate::providers::ai_error_candidate(context, &message, configure);
            state.selected = None;
            state.candidates = vec![candidate];
            state.status = Some(message);
        }
    }
    render_current(state, output)
}

fn handle_terminal_reply(reply: TerminalReply, output: &OutputHandle) -> crate::Result<bool> {
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
            kind: TerminalQueryKind::CursorPosition,
            ..
        } => {
            output.invalidate_anchor().map_err(output_error)?;
        }
    }
    Ok(false)
}

fn maybe_probe_cursor(
    state: &mut RuntimeState,
    router: &mut TerminalReplyRouter,
    output: &OutputHandle,
) -> crate::Result<()> {
    if !state.need_cpr || router.has_outstanding() {
        return Ok(());
    }
    let output_state = output.state().map_err(output_error)?;
    if !output_state.cursor_probe_ready || output_state.foreground || output_state.alternate_screen
    {
        return Ok(());
    }
    let query = router.register(
        TerminalQueryKind::CursorPosition,
        Instant::now(),
        TERMINAL_QUERY_TIMEOUT,
    )?;
    output.probe(query.bytes).map_err(output_error)?;
    state.need_cpr = false;
    Ok(())
}

fn flush_scheduled_frame(state: &mut RuntimeState, output: &OutputHandle) -> crate::Result<()> {
    if let Some((_, frame)) = state.scheduler.take_ready(Instant::now()) {
        output.commit_latest(frame).map_err(output_error)?;
    }
    Ok(())
}

/// When `schedule_query` suppresses completion (empty trimmed buffer without
/// history focus, or uncertain sync), no provider result will ever arrive to
/// clear the overlay — hide it here so stale rows cannot linger on screen.
fn hide_overlay_if_query_suppressed(
    state: &mut RuntimeState,
    output: &OutputHandle,
) -> crate::Result<()> {
    if !state.provider_pending && state.candidates.is_empty() && state.status.is_none() {
        state.overlay_visible = false;
        output.hide_overlay().map_err(output_error)?;
    }
    Ok(())
}

fn detect_foreground_process(
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

fn render_current(state: &mut RuntimeState, output: &OutputHandle) -> crate::Result<()> {
    if !state.overlay_visible
        && state.candidates.is_empty()
        && state.status.is_none()
        && state.pending_confirm.is_none()
    {
        return Ok(());
    }
    if state.candidates.is_empty() && state.status.is_none() && state.pending_confirm.is_none() {
        state.overlay_visible = false;
        output.hide_overlay().map_err(output_error)?;
        return Ok(());
    }
    let output_state = output.state().map_err(output_error)?;
    if output_state.foreground
        || output_state.alternate_screen
        || output_state.confidence == crate::terminal::AnchorConfidence::Unknown
        || !matches!(
            output_state.readiness,
            RenderReadiness::Ready {
                buffer_revision,
                screen_revision
            } if buffer_revision == state.buffer.revision
                && screen_revision == output_state.screen_revision
        )
    {
        return Ok(());
    }
    // Never commit a frame whose rows belong to a superseded query: their
    // ids already fail activation's freshness check, so painting the list
    // (and especially a selection marker) would present a choice the user
    // cannot act on. The last committed frame stays on screen until fresh
    // results replace it.
    if state.pending_confirm.is_none()
        && state.candidates.first().is_some_and(|candidate| {
            Some(candidate.query_id) != state.context.as_ref().map(|context| context.query_id)
        })
    {
        return Ok(());
    }
    let Some(geometry) = output.prepare_surface().map_err(output_error)? else {
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
    Ok(())
}

fn selected_candidate(state: &RuntimeState) -> Option<&Candidate> {
    let id = state.selected?;
    state.candidates.iter().find(|candidate| candidate.id == id)
}

fn move_selection(state: &mut RuntimeState, delta: isize) {
    if state.candidates.is_empty() {
        state.selected = None;
        return;
    }
    let length = state.candidates.len() as isize;
    let next = match state.selected.and_then(|id| {
        state
            .candidates
            .iter()
            .position(|candidate| candidate.id == id)
    }) {
        Some(current) => (current as isize + delta).rem_euclid(length) as usize,
        // No implicit selection: the first Down lands on the first row, the
        // first Up on the last; page jumps go to the first row / the start of
        // the last page.
        None => landing_row(state.candidates.len(), state.page_size, delta),
    };
    state.selected = Some(state.candidates[next].id);
    state.selection_intent = Some(SelectionIntent {
        delta,
        key: SelectionKey::of(&state.candidates[next]),
    });
}

/// Where a navigation lands when nothing is selected yet: Down on the first
/// row, Up on the last, page-down on the first row, page-up at the start of
/// the last page.
fn landing_row(candidate_count: usize, page_size: usize, delta: isize) -> usize {
    if delta == 1 {
        0
    } else if delta == -1 {
        candidate_count - 1
    } else if delta > 1 {
        (candidate_count - 1) / page_size * page_size
    } else {
        0
    }
}

fn output_error(error: crate::terminal::OutputError) -> crate::Error {
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

#[cfg(test)]
mod tests {
    use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path};

    use fs2::FileExt;

    use super::*;
    use crate::completion::{
        CandidateAction, CandidateKind, Completeness, CursorPlacement, TextEdit,
    };
    use crate::terminal::RiskLevel;

    fn runtime_state(directory: &Path) -> RuntimeState {
        RuntimeState::new(
            ShellKind::Zsh,
            TerminalSize::new(24, 80).expect("terminal size"),
            directory.to_owned(),
            true,
            HistoryCursor::default(),
            12,
            directory.join("credentials.toml"),
            "0123456789abcdef01234567".into(),
            None,
        )
    }

    #[test]
    fn startup_history_read_is_tail_bounded_and_line_aligned() {
        let directory = tempfile::tempdir().expect("history directory");
        let path = directory.path().join("history");
        std::fs::write(&path, b"discarded command\nrecent one\nrecent two\n")
            .expect("history fixture");
        let bytes = read_history_tail(&path, 22).expect("history tail");
        assert!(bytes.len() <= 22);
        assert_eq!(bytes, b"recent one\nrecent two\n");
    }

    #[test]
    fn startup_history_read_rejects_fifos_without_blocking() {
        use nix::{sys::stat::Mode, unistd::mkfifo};

        let directory = tempfile::tempdir().expect("history directory");
        let path = directory.path().join("history.fifo");
        mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).expect("history FIFO");
        let started = Instant::now();
        assert!(read_history_tail(&path, 1024).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn stale_candidate_activation_is_nonfatal() {
        let directory = tempfile::tempdir().expect("directory");
        let mut state = runtime_state(directory.path());
        state
            .buffer
            .set_exact("ec".into(), 2)
            .expect("initial buffer");
        let context = Arc::new(
            CompletionContext::new(
                QueryId::new(1),
                ShellKind::Zsh,
                directory.path().to_owned(),
                state.snapshot().expect("snapshot"),
            )
            .expect("context"),
        );
        let candidate = Candidate::new(
            context.query_id,
            "echo",
            "candidate",
            Some(TextEdit {
                range: 0..2,
                replacement: "echo".into(),
                cursor_after: CursorPlacement::End,
            }),
            CandidateAction::Insert,
            CandidateSource::History,
            CandidateKind::History,
            Completeness::Runnable,
            RiskLevel::Low,
            "stale-test",
        );
        state.selected = Some(candidate.id);
        state.candidates = vec![candidate];
        state.context = Some(context);
        state
            .buffer
            .set_exact("echo".into(), 4)
            .expect("newer buffer");

        assert!(matches!(
            resolve_selected_activation(&state).expect("stale activation is handled"),
            SelectedActivation::Rejected
        ));
    }

    #[test]
    fn history_lock_contention_queues_without_double_indexing() {
        let directory = tempfile::tempdir().expect("directory");
        let store = HistoryStore::open(directory.path()).expect("store");
        let lock_path = directory.path().join("history.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(lock_path)
            .expect("lock file");
        lock.lock_exclusive().expect("hold lock");

        let history = Arc::new(RwLock::new(HistoryIndex::default()));
        let policy = HistoryPolicy::new(16_384, &[]).expect("policy");
        let mut state = runtime_state(directory.path());
        record_history(
            "echo queued".into(),
            Some(0),
            &mut state,
            &store,
            &history,
            &policy,
        )
        .expect("queue history");
        assert_eq!(state.pending_history.len(), 1);
        assert_eq!(
            history
                .read()
                .expect("history index")
                .search("echo queued", Path::new("/"), 1, 10)[0]
                .record
                .count,
            1
        );

        FileExt::unlock(&lock).expect("unlock");
        state.history_retry_at = Instant::now();
        flush_pending_history(&mut state, &store).expect("flush queue");
        sync_history(&mut state, &store, &history, &policy).expect("sync queue");
        assert!(state.pending_history.is_empty());
        assert_eq!(
            history
                .read()
                .expect("history index")
                .search("echo queued", Path::new("/"), 1, 10)[0]
                .record
                .count,
            1
        );
    }

    #[test]
    fn pagination_capacity_tracks_terminal_height() {
        assert_eq!(
            visible_page_size(12, TerminalSize::new(24, 80).expect("size")),
            10
        );
        assert_eq!(
            visible_page_size(12, TerminalSize::new(5, 80).expect("size")),
            2
        );
    }

    #[test]
    fn recognizes_bash_ignorespace_history_modes() {
        assert!(history_control_ignores_space("ignorespace"));
        assert!(history_control_ignores_space("erasedups:ignoreboth"));
        assert!(!history_control_ignores_space("ignoredups:erasedups"));
    }

    #[test]
    fn live_config_keeps_structural_values_until_restart() {
        let current = Config::default();
        let mut loaded = current.clone();
        loaded.ui.max_rows = 7;
        loaded.completion.max_candidates = 77;
        loaded.core.login_shell = true;
        loaded.history.max_command_bytes = 999;
        loaded.logging.enabled = true;
        let (live, restart) = merge_live_config(&current, loaded);
        assert_eq!(live.ui.max_rows, 7);
        assert_eq!(live.completion.max_candidates, 77);
        assert_eq!(live.core, current.core);
        assert_eq!(live.history, current.history);
        assert_eq!(live.logging, current.logging);
        assert_eq!(restart, vec!["core", "history", "logging"]);
    }

    fn selection_candidate(state: &RuntimeState, primary: &str) -> Candidate {
        Candidate::new(
            QueryId::new(1),
            primary,
            "test",
            Some(TextEdit {
                range: 0..state.buffer.text.len(),
                replacement: primary.into(),
                cursor_after: CursorPlacement::End,
            }),
            CandidateAction::Insert,
            CandidateSource::History,
            CandidateKind::History,
            Completeness::Runnable,
            RiskLevel::Low,
            primary,
        )
    }

    #[test]
    fn move_selection_from_none_lands_on_the_edges() {
        let directory = tempfile::tempdir().expect("directory");
        let mut state = runtime_state(directory.path());
        state
            .buffer
            .set_exact("ec".into(), 2)
            .expect("initial buffer");
        state.candidates = (0..25)
            .map(|index| selection_candidate(&state, &format!("echo {index:02}")))
            .collect();
        state.selected = None;

        move_selection(&mut state, 1);
        assert_eq!(state.selected, Some(state.candidates[0].id));

        state.selected = None;
        move_selection(&mut state, -1);
        assert_eq!(state.selected, Some(state.candidates[24].id));

        // Page jumps land on the first row / the start of the last page.
        let page_size = state.page_size as isize;
        state.selected = None;
        move_selection(&mut state, page_size);
        assert_eq!(state.selected, Some(state.candidates[20].id));

        state.selected = None;
        move_selection(&mut state, -page_size);
        assert_eq!(state.selected, Some(state.candidates[0].id));

        // Selection movement still wraps once a selection exists.
        move_selection(&mut state, -1);
        assert_eq!(state.selected, Some(state.candidates[24].id));
    }

    fn enter_candidate(
        primary: &str,
        action: CandidateAction,
        completeness: Completeness,
        risk: RiskLevel,
    ) -> Candidate {
        Candidate::new(
            QueryId::new(1),
            primary,
            "test",
            (matches!(
                action,
                CandidateAction::Insert | CandidateAction::InsertAndContinue { .. }
            ))
            .then(|| TextEdit {
                range: 0..2,
                replacement: primary.into(),
                cursor_after: CursorPlacement::End,
            }),
            action,
            CandidateSource::History,
            CandidateKind::History,
            completeness,
            risk,
            primary,
        )
    }

    #[test]
    fn enter_executes_runnable_safe_candidates() {
        let candidate = enter_candidate(
            "echo ok",
            CandidateAction::Insert,
            Completeness::Runnable,
            RiskLevel::Low,
        );
        let activation = Activation::ReplaceBuffer {
            text: "echo ok".into(),
            cursor: 7,
        };
        assert!(matches!(
            resolve_enter(&candidate, &activation),
            EnterResolution::Execute(text) if text == "echo ok"
        ));
    }

    #[test]
    fn enter_degrades_non_runnable_candidates_to_fill() {
        let needs_input = enter_candidate(
            "tar -czf",
            CandidateAction::InsertAndContinue {
                next_slot: crate::completion::SlotKind::File,
            },
            Completeness::NeedsInput {
                slot: crate::completion::SlotKind::File,
            },
            RiskLevel::Low,
        );
        let activation = Activation::ReplaceBuffer {
            text: "tar -czf".into(),
            cursor: 8,
        };
        assert!(matches!(
            resolve_enter(&needs_input, &activation),
            EnterResolution::Fill
        ));

        let ai = enter_candidate(
            "ask ai",
            CandidateAction::RequestAi,
            Completeness::ActionOnly,
            RiskLevel::Low,
        );
        assert!(matches!(
            resolve_enter(&ai, &Activation::RequestAi),
            EnterResolution::Fill
        ));
    }

    #[test]
    fn enter_confirms_when_the_effective_risk_is_dangerous() {
        // Classified risk is stricter than the provider-assigned Low.
        let dangerous = enter_candidate(
            "rm -rf /tmp/x",
            CandidateAction::Insert,
            Completeness::Runnable,
            RiskLevel::Low,
        );
        let activation = Activation::ReplaceBuffer {
            text: "rm -rf /tmp/x".into(),
            cursor: 13,
        };
        match resolve_enter(&dangerous, &activation) {
            EnterResolution::Confirm {
                text,
                risk,
                reasons,
            } => {
                assert_eq!(text, "rm -rf /tmp/x");
                assert_eq!(risk, RiskLevel::High);
                assert!(reasons.contains(&"recursive operation".to_owned()));
                assert!(reasons.contains(&"force flag".to_owned()));
            }
            other => panic!("expected confirmation, got {}", enter_label(&other)),
        }

        // Provider-assigned risk is stricter than the classified ReadOnly.
        let flagged = enter_candidate(
            "ls",
            CandidateAction::Insert,
            Completeness::Runnable,
            RiskLevel::Unknown,
        );
        let activation = Activation::ReplaceBuffer {
            text: "ls".into(),
            cursor: 2,
        };
        assert!(matches!(
            resolve_enter(&flagged, &activation),
            EnterResolution::Confirm {
                risk: RiskLevel::Unknown,
                ..
            }
        ));

        // Medium risk still executes without confirmation.
        let medium = enter_candidate(
            "rm file",
            CandidateAction::Insert,
            Completeness::Runnable,
            RiskLevel::Low,
        );
        let activation = Activation::ReplaceBuffer {
            text: "rm file".into(),
            cursor: 7,
        };
        assert!(matches!(
            resolve_enter(&medium, &activation),
            EnterResolution::Execute(_)
        ));
    }

    fn enter_label(resolution: &EnterResolution) -> &'static str {
        match resolution {
            EnterResolution::Fill => "fill",
            EnterResolution::Execute(_) => "execute",
            EnterResolution::Confirm { .. } => "confirm",
        }
    }

    fn session_token() -> crate::terminal::SessionToken {
        crate::terminal::SessionToken::parse("0123456789abcdef0123456789abcdef")
            .expect("fixture token is valid")
    }

    fn history_candidate(query_id: QueryId, text: &str) -> Candidate {
        Candidate::new(
            query_id,
            text,
            "from history",
            Some(TextEdit {
                range: 0..0,
                replacement: text.into(),
                cursor_after: CursorPlacement::End,
            }),
            CandidateAction::Insert,
            CandidateSource::History,
            CandidateKind::History,
            Completeness::Runnable,
            RiskLevel::Low,
            "reselection-test",
        )
    }

    fn refresh_context(state: &mut RuntimeState, query_id: QueryId) {
        let context = Arc::new(
            CompletionContext::new(
                query_id,
                ShellKind::Zsh,
                state.cwd.clone(),
                state.snapshot().expect("snapshot"),
            )
            .expect("context"),
        );
        state.context = Some(context);
    }

    fn provider_result(state: &RuntimeState, candidates: Vec<Candidate>) -> ProviderResult {
        ProviderResult {
            context: Arc::clone(state.context.as_ref().expect("context")),
            output: ProviderOutput {
                candidates,
                diagnostics: Vec::new(),
            },
            final_batch: true,
        }
    }

    #[test]
    fn navigation_intent_is_reapplied_when_queued_buffer_events_move_the_query() {
        let directory = tempfile::tempdir().expect("directory");
        let mut state = runtime_state(directory.path());
        let (output, join) = crate::terminal::spawn_with_writer(
            Vec::new(),
            session_token(),
            TerminalSize::new(24, 80).expect("terminal size"),
            3,
        )
        .expect("output actor");

        // The user navigates against the visible list…
        state.buffer.set_exact("ec".into(), 2).expect("buffer");
        refresh_context(&mut state, QueryId::new(1));
        state.candidates = vec![history_candidate(QueryId::new(1), "echo HKSEL_HIDDEN")];
        move_selection(&mut state, 1);
        let first = state.selected.expect("navigation selects a row");

        // …but queued buffer events move the query on before the selection
        // is rendered; fresh candidates carry unrelated per-query ids.
        state
            .buffer
            .set_exact("echo HKSEL_H".into(), 12)
            .expect("buffer");
        refresh_context(&mut state, QueryId::new(2));
        state.selected = None;
        let result = provider_result(
            &state,
            vec![history_candidate(QueryId::new(2), "echo HKSEL_HIDDEN")],
        );
        handle_provider_result(result, &mut state, &output).expect("provider result");

        let reselected = state.selected.expect("intent re-applies the selection");
        assert_ne!(reselected, first, "the fresh candidate has a fresh id");
        assert_eq!(
            state
                .candidates
                .iter()
                .find(|candidate| candidate.id == reselected)
                .map(|candidate| candidate.display.primary.as_str()),
            Some("echo HKSEL_HIDDEN")
        );
        output.restore_and_exit().expect("shutdown");
        join.join().expect("actor joins").expect("actor exits");
    }

    #[test]
    fn navigation_intent_falls_back_to_the_delta_when_the_row_vanishes() {
        let directory = tempfile::tempdir().expect("directory");
        let mut state = runtime_state(directory.path());
        let (output, join) = crate::terminal::spawn_with_writer(
            Vec::new(),
            session_token(),
            TerminalSize::new(24, 80).expect("terminal size"),
            3,
        )
        .expect("output actor");

        state.buffer.set_exact("ec".into(), 2).expect("buffer");
        refresh_context(&mut state, QueryId::new(1));
        state.candidates = vec![history_candidate(QueryId::new(1), "echo gone")];
        move_selection(&mut state, 1);
        state.buffer.set_exact("echo HK".into(), 6).expect("buffer");
        refresh_context(&mut state, QueryId::new(2));
        state.selected = None;
        let result = provider_result(
            &state,
            vec![
                history_candidate(QueryId::new(2), "echo HKSEL_HIDDEN"),
                history_candidate(QueryId::new(2), "echo HKOTHER"),
            ],
        );
        handle_provider_result(result, &mut state, &output).expect("provider result");

        assert_eq!(
            state.selected,
            Some(state.candidates[0].id),
            "Down from nothing lands on the first row of the fresh list"
        );
        output.restore_and_exit().expect("shutdown");
        join.join().expect("actor joins").expect("actor exits");
    }

    #[test]
    fn navigation_intent_does_not_create_a_selection_by_itself() {
        let directory = tempfile::tempdir().expect("directory");
        let mut state = runtime_state(directory.path());
        let (output, join) = crate::terminal::spawn_with_writer(
            Vec::new(),
            session_token(),
            TerminalSize::new(24, 80).expect("terminal size"),
            3,
        )
        .expect("output actor");

        // No navigation happened: results must never pre-select a row.
        state.buffer.set_exact("ec".into(), 2).expect("buffer");
        refresh_context(&mut state, QueryId::new(1));
        let result = provider_result(
            &state,
            vec![history_candidate(QueryId::new(1), "echo HKSEL_HIDDEN")],
        );
        handle_provider_result(result, &mut state, &output).expect("provider result");
        assert_eq!(state.selected, None);
        output.restore_and_exit().expect("shutdown");
        join.join().expect("actor joins").expect("actor exits");
    }
}
