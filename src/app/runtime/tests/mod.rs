use std::{
    fs::OpenOptions,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use fs2::FileExt;

use super::*;
use crate::completion::{
    Activation, BufferSnapshot, Candidate, CandidateAction, CandidateKind, CandidateSource,
    Completeness, CompletionContext, CursorPlacement, ProviderOutput, SyncQuality, TextEdit,
};
use crate::config::{Config, ConfigPaths};
use crate::history::{HistoryCursor, HistoryIndex, HistoryPolicy, HistoryStore};
use crate::platform::CommandPathCache;
use crate::shell::{ControlMessage, ShellEvent, ShellKind};
use crate::terminal::BoundaryId;
use crate::terminal::{
    BufferRevision, QueryId, RiskLevel, TerminalQueryKind, TerminalReply, TerminalSize,
};

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
        Arc::new(CommandPathCache::default()),
        Arc::new(crate::specs::SpecRegistry::default()),
        Arc::new(crate::providers::CommandHelpCache::default()),
    )
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

fn test_output() -> crate::terminal::SpawnedOutput<Vec<u8>> {
    crate::terminal::spawn_with_writer(
        Vec::new(),
        session_token(),
        TerminalSize::new(24, 80).expect("terminal size"),
        3,
    )
    .expect("output actor")
}

fn ai_command(text: &str) -> crate::ai::AiCommand {
    crate::ai::AiCommand {
        command: text.to_owned(),
        explanation: "ai suggestion".to_owned(),
        risk: None,
    }
}

mod activation;
mod ai;
mod configuration;
mod navigation;
mod providers;
mod results;
mod terminal;
