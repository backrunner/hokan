use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};

use super::{super::buffer::EditableBuffer, worker::ProviderWorker};
use crate::{
    completion::{BufferSnapshot, Candidate, CompletionContext, SyncQuality},
    diagnostics::DebugLog,
    history::{HistoryCursor, HistoryEventV1},
    platform::CommandPathCache,
    providers::{CommandHelpCache, argument_progress},
    shell::ShellKind,
    specs::SpecRegistry,
    terminal::{
        BufferRevision, FrameRequest, FrameRevision, LatestFrameScheduler, QueryId, TerminalSize,
    },
};
use tokio_util::sync::CancellationToken;

pub(super) struct RuntimeState {
    pub(super) shell: ShellKind,
    pub(super) terminal_size: TerminalSize,
    pub(super) cwd: PathBuf,
    pub(super) buffer: EditableBuffer,
    pub(super) query_id: QueryId,
    pub(super) context: Option<Arc<CompletionContext>>,
    pub(super) candidates: Vec<Candidate>,
    pub(super) selected: Option<crate::completion::CandidateId>,
    pub(super) selection_intent: Option<SelectionIntent>,
    pub(super) history_only: bool,
    pub(super) provider_pending: bool,
    pub(super) overlay_visible: bool,
    /// Set when `render_current` had rows to show but the terminal was not
    /// ready (render gate, anchor, or geometry) — the main loop retries the
    /// repaint on every tick until it lands or the query moves on.
    pub(super) repaint_pending: bool,
    pub(super) frame_revision: FrameRevision,
    pub(super) editing: bool,
    pub(super) pending_mirror_revision: Option<BufferRevision>,
    pub(super) status: Option<String>,
    pub(super) escape_deadline: Option<Instant>,
    pub(super) need_cpr: bool,
    pub(super) pending_command: Option<String>,
    /// Last command recorded as executed in this session; feeds the
    /// transition bigram signal on the next completion query.
    pub(super) previous_command: Option<String>,
    pub(super) workspace_probe: crate::project::WorkspaceProbe,
    pub(super) commands: Arc<CommandPathCache>,
    pub(super) specs: Arc<SpecRegistry>,
    pub(super) help: Arc<CommandHelpCache>,
    pub(super) pending_confirm: Option<PendingConfirm>,
    pub(super) ai_query: Option<ActiveAiRequest>,
    /// Bumped for every AI request so a late result from a superseded request
    /// (which can share the query id) never matches the active one.
    pub(super) ai_generation: u64,
    /// True from `start_ai_request` until the next `schedule_query`: while the
    /// AI wait screen or its results own the overlay, provider batches for the
    /// same query must not replace them.
    pub(super) ai_owns_candidates: bool,
    pub(super) foreground_process: bool,
    pub(super) suspended: bool,
    pub(super) pending_reanchor: bool,
    pub(super) history_cursor: HistoryCursor,
    pub(super) pending_history: VecDeque<HistoryEventV1>,
    pub(super) local_history_ids: HashSet<String>,
    pub(super) history_retry_at: Instant,
    pub(super) history_session_id: String,
    pub(super) history_sequence: u64,
    pub(super) history_compaction: Arc<AtomicBool>,
    pub(super) ignore_leading_space_history: bool,
    pub(super) page_size: usize,
    pub(super) max_overlay_height: u16,
    pub(super) credentials_file: PathBuf,
    pub(super) scheduler: LatestFrameScheduler<FrameRequest>,
    pub(super) theme_revision: u64,
    pub(super) debug_log: Option<DebugLog>,
}

impl RuntimeState {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        shell: ShellKind,
        terminal_size: TerminalSize,
        cwd: PathBuf,
        exact: bool,
        history_cursor: HistoryCursor,
        max_overlay_height: u16,
        credentials_file: PathBuf,
        history_session_id: String,
        debug_log: Option<DebugLog>,
        commands: Arc<CommandPathCache>,
        specs: Arc<SpecRegistry>,
        help: Arc<CommandHelpCache>,
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
            repaint_pending: false,
            frame_revision: FrameRevision::ZERO,
            editing: false,
            pending_mirror_revision: None,
            status: None,
            escape_deadline: None,
            need_cpr: false,
            pending_command: None,
            previous_command: None,
            workspace_probe: crate::project::WorkspaceProbe::default(),
            commands,
            specs,
            help,
            pending_confirm: None,
            ai_query: None,
            ai_generation: 0,
            ai_owns_candidates: false,
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

    pub(super) fn update_terminal_size(&mut self, size: TerminalSize) {
        self.terminal_size = size;
        self.page_size = visible_page_size(self.max_overlay_height, size);
    }

    pub(super) fn update_overlay_height(&mut self, max_overlay_height: u16) {
        self.max_overlay_height = max_overlay_height.max(1);
        self.page_size = visible_page_size(self.max_overlay_height, self.terminal_size);
        self.theme_revision = self.theme_revision.saturating_add(1);
    }

    pub(super) fn snapshot(&self) -> crate::Result<BufferSnapshot> {
        BufferSnapshot::new(
            Arc::<str>::from(self.buffer.text.as_str()),
            self.buffer.cursor,
            self.buffer.revision,
            self.buffer.sync,
        )
    }

    pub(super) fn schedule_query(&mut self, worker: &ProviderWorker) -> crate::Result<()> {
        self.cancel_ai();
        self.ai_owns_candidates = false;
        self.repaint_pending = false;
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
        // A bare executable word with the cursor still on it (`kimi`, no
        // trailing space) is already runnable — hold suggestions until the
        // user commits to arguments with a space instead of flashing rows
        // that all just complete the same command name.
        if !self.history_only
            && executable_awaiting_arguments(&context, &self.commands, &self.specs, &self.help)
        {
            self.context = None;
            self.candidates.clear();
            self.selected = None;
            self.overlay_visible = false;
            self.provider_pending = false;
            return Ok(());
        }
        self.context = Some(Arc::clone(&context));
        self.provider_pending = true;
        worker.schedule(context)
    }

    pub(super) fn cancel_ai(&mut self) {
        if let Some(request) = self.ai_query.take() {
            request.cancel.cancel();
            if let Some(log) = &self.debug_log {
                log.ai_event("cancelled");
            }
        }
    }

    pub(super) fn ai_query_id(&self) -> Option<QueryId> {
        self.ai_query.as_ref().map(|request| request.query_id)
    }

    pub(super) fn next_history_event_id(&mut self) -> crate::Result<String> {
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

pub(super) struct ActiveAiRequest {
    pub(super) query_id: QueryId,
    pub(super) generation: u64,
    pub(super) cancel: CancellationToken,
}

/// A dangerous candidate execution awaiting explicit confirmation: the full
/// final command text plus the effective risk that triggered the prompt.
pub(super) struct PendingConfirm {
    pub(super) text: String,
    pub(super) risk: crate::terminal::RiskLevel,
    pub(super) reasons: Vec<String>,
}

/// Number of candidate rows visible per page: the overlay box height minus
/// its two border rows (pagination lives in the top border, status/hints in
/// the bottom border, so no item row is reserved).
pub(super) fn visible_page_size(max_overlay_height: u16, terminal_size: TerminalSize) -> usize {
    let height = max_overlay_height
        .min(terminal_size.rows.saturating_sub(1))
        .max(1);
    usize::from(height.saturating_sub(2).max(1))
}

/// True while the cursor is still on the command token (no trailing
/// whitespace) and the typed word is itself an executable on PATH that runs
/// standalone — a bare `kimi`. The line is already runnable, so suggestions
/// wait for the space that commits the user to typing arguments. Commands
/// that do nothing useful without arguments (`git` and other subcommand-style
/// CLIs, specs with `requires_arguments`, man pages with subcommands) are NOT
/// held back: their suggestions are exactly what the user needs next.
pub(super) fn executable_awaiting_arguments(
    context: &CompletionContext,
    commands: &CommandPathCache,
    specs: &SpecRegistry,
    help: &CommandHelpCache,
) -> bool {
    argument_progress(context).is_none()
        && context.command().is_some_and(|command| {
            commands.contains(command) && !requires_arguments(command, specs, help)
        })
}

/// Commands that cannot run standalone. The PATH cache cannot express this,
/// so combine three signals: a built-in list of subcommand-style CLIs, the
/// spec registry's `requires_arguments` flag, and man-derived subcommand
/// coverage when the help cache happens to be warm.
fn requires_arguments(command: &str, specs: &SpecRegistry, help: &CommandHelpCache) -> bool {
    const SUBCOMMAND_COMMANDS: &[&str] = &[
        "ansible",
        "apt",
        "aws",
        "az",
        "brew",
        "cargo",
        "consul",
        "dnf",
        "docker",
        "docker-compose",
        "eksctl",
        "firebase",
        "flyctl",
        "gem",
        "gh",
        "gcloud",
        "go",
        "helm",
        "heroku",
        "istioctl",
        "kubectl",
        "mise",
        "nerdctl",
        "nix",
        "npm",
        "oc",
        "pacman",
        "pip",
        "pip3",
        "pipx",
        "pnpm",
        "podman",
        "railway",
        "rustup",
        "snap",
        "supabase",
        "systemctl",
        "terraform",
        "tmux",
        "vagrant",
        "vault",
        "vercel",
        "wrangler",
        "yarn",
        "bun",
        "git",
    ];
    if SUBCOMMAND_COMMANDS.contains(&command) {
        return true;
    }
    if let Some(spec) = specs.get(command) {
        return spec.requires_arguments;
    }
    help.peek(command)
        .is_some_and(|help| help.has_subcommands())
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
pub(super) struct SelectionIntent {
    pub(super) delta: isize,
    pub(super) key: SelectionKey,
}

/// Content identity of a candidate row: two queries for different buffer
/// revisions agree on it while per-query candidate ids never match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SelectionKey {
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

    pub(super) fn matches(&self, candidate: &Candidate) -> bool {
        self.source == candidate.source
            && self.primary == candidate.display.primary
            && self.replacement.as_ref() == candidate.edit.as_ref().map(|edit| &edit.replacement)
    }
}

pub(super) fn selected_candidate(state: &RuntimeState) -> Option<&Candidate> {
    let id = state.selected?;
    state.candidates.iter().find(|candidate| candidate.id == id)
}

pub(super) fn move_selection(state: &mut RuntimeState, delta: isize) {
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
pub(super) fn landing_row(candidate_count: usize, page_size: usize, delta: isize) -> usize {
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
