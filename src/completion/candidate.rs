use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    ops::Range,
};

use crate::{
    completion::{BufferSnapshot, CompletionContext},
    parser::apply_edit,
    terminal::{QueryId, RiskLevel},
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CandidateId(pub u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisplayText {
    pub primary: String,
    pub description: String,
    pub annotation: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CandidateSource {
    CommandSpec,
    History,
    Filesystem,
    Project,
    Process,
    NetworkInterface,
    PathCommand,
    Ai,
    Action,
    Diagnostic,
}

impl CandidateSource {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CommandSpec => "SPEC",
            Self::History => "HIS",
            Self::Filesystem => "FILE",
            Self::Project => "PROJ",
            Self::Process => "PID",
            Self::NetworkInterface => "NET",
            Self::PathCommand => "CMD",
            Self::Ai => "AI",
            Self::Action => "ACT",
            Self::Diagnostic => "INFO",
        }
    }

    pub(crate) const fn trust(self) -> i16 {
        match self {
            Self::CommandSpec => 300,
            Self::Project => 270,
            Self::Filesystem | Self::PathCommand => 240,
            Self::History => 220,
            Self::Process | Self::NetworkInterface => 210,
            Self::Action => 180,
            Self::Ai => 100,
            Self::Diagnostic => 0,
        }
    }

    pub(crate) const fn order(self) -> u8 {
        match self {
            Self::CommandSpec => 0,
            Self::History => 1,
            Self::Project => 2,
            Self::Filesystem => 3,
            Self::PathCommand => 4,
            Self::Process => 5,
            Self::NetworkInterface => 6,
            Self::Action => 7,
            Self::Ai => 8,
            Self::Diagnostic => 9,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateKind {
    Command,
    Recipe,
    History,
    File,
    Directory,
    ProjectScript,
    Process,
    Interface,
    AiAction,
    AiCommand,
    Diagnostic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlotKind {
    File,
    Directory,
    Path,
    Executable,
    NewFile,
    Process,
    Interface,
    Port,
    Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorPlacement {
    End,
    Offset(usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextEdit {
    pub range: Range<usize>,
    pub replacement: String,
    pub cursor_after: CursorPlacement,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CandidateAction {
    Insert,
    InsertAndContinue { next_slot: SlotKind },
    RequestAi,
    ConfigureAi,
    Retry,
    None,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Completeness {
    Runnable,
    NeedsInput { slot: SlotKind },
    ActionOnly,
}

/// Additive ranking signals for one candidate. `total()` is
///
/// ```text
/// match_quality + source_trust + spec_priority + cwd_affinity + frecency
///     + transition + context - risk_penalty - incomplete_penalty - failed_penalty
/// ```
///
/// Signal ranges:
/// - `match_quality`: 0..=1000 (exact 1000, prefix 900-.., substring 700-..,
///   subsequence 450, no-match 0; empty query 500) — set centrally by ranking.
/// - `source_trust`: 0..=300 — set centrally from the candidate source.
/// - `spec_priority`: 0..=200 — provider-set (command spec / filesystem).
/// - `cwd_affinity`: 0 or 100 — provider-set (history recorded in this cwd).
/// - `frecency`: 0..=200 — provider-set (history recency + frequency).
/// - `transition`: 0..=200 — provider-set bigram boost: how often the
///   candidate's skeleton followed the previous executed command.
/// - `context`: 0..=100 (40 per matched workspace rule) — set centrally by
///   ranking from the detected workspace markers (git, package.json,
///   Cargo.toml, Makefile, justfile).
/// - `risk_penalty`: 0..=300, subtracted — set centrally from the risk level.
/// - `incomplete_penalty`: 0..=80, subtracted — set centrally from completeness.
/// - `failed_penalty`: 0 or 150, subtracted — provider-set when the history
///   record's last known run exited non-zero (excluding SIGINT 130).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScoreSignals {
    pub match_quality: i16,
    pub source_trust: i16,
    pub spec_priority: i16,
    pub cwd_affinity: i16,
    pub frecency: i16,
    pub transition: i16,
    pub context: i16,
    pub risk_penalty: i16,
    pub incomplete_penalty: i16,
    pub failed_penalty: i16,
}

impl ScoreSignals {
    #[must_use]
    pub const fn total(self) -> i32 {
        self.match_quality as i32
            + self.source_trust as i32
            + self.spec_priority as i32
            + self.cwd_affinity as i32
            + self.frecency as i32
            + self.transition as i32
            + self.context as i32
            - self.risk_penalty as i32
            - self.incomplete_penalty as i32
            - self.failed_penalty as i32
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    pub id: CandidateId,
    pub query_id: QueryId,
    pub display: DisplayText,
    pub edit: Option<TextEdit>,
    pub action: CandidateAction,
    pub source: CandidateSource,
    pub kind: CandidateKind,
    pub completeness: Completeness,
    pub risk: RiskLevel,
    pub score: ScoreSignals,
    pub provenance: String,
}

impl Candidate {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        query_id: QueryId,
        primary: impl Into<String>,
        description: impl Into<String>,
        edit: Option<TextEdit>,
        action: CandidateAction,
        source: CandidateSource,
        kind: CandidateKind,
        completeness: Completeness,
        risk: RiskLevel,
        provenance: impl Into<String>,
    ) -> Self {
        let primary = primary.into();
        let provenance = provenance.into();
        let mut hasher = DefaultHasher::new();
        query_id.get().hash(&mut hasher);
        source.hash(&mut hasher);
        primary.hash(&mut hasher);
        provenance.hash(&mut hasher);
        edit.as_ref()
            .map(|value| &value.replacement)
            .hash(&mut hasher);
        Self {
            id: CandidateId(hasher.finish()),
            query_id,
            display: DisplayText {
                primary,
                description: description.into(),
                annotation: None,
            },
            edit,
            action,
            source,
            kind,
            completeness,
            risk,
            score: ScoreSignals::default(),
            provenance,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Activation {
    ReplaceBuffer { text: String, cursor: usize },
    RequestAi,
    ConfigureAi,
    Retry,
    None,
}

pub fn activate_candidate(
    candidate: &Candidate,
    context: &CompletionContext,
    current: &BufferSnapshot,
) -> crate::Result<Activation> {
    if candidate.query_id != context.query_id
        || current.revision != context.buffer.revision
        || current.hash != context.buffer.hash
        || current.sync == crate::completion::SyncQuality::Uncertain
    {
        return Err(crate::Error::Completion(
            "candidate no longer belongs to the current buffer".into(),
        ));
    }
    match candidate.action {
        CandidateAction::Insert | CandidateAction::InsertAndContinue { .. } => {
            let edit = candidate.edit.as_ref().ok_or_else(|| {
                crate::Error::Completion("insert candidate is missing its text edit".into())
            })?;
            let text = apply_edit(&current.text, edit.range.clone(), &edit.replacement)
                .map_err(|error| crate::Error::Completion(error.to_string()))?;
            let cursor = match edit.cursor_after {
                CursorPlacement::End => edit.range.start + edit.replacement.len(),
                CursorPlacement::Offset(offset) => edit.range.start + offset,
            };
            if cursor > text.len()
                || !text.is_char_boundary(cursor)
                || text.chars().any(char::is_control)
            {
                return Err(crate::Error::Completion(
                    "candidate produced unsafe shell editing text".into(),
                ));
            }
            Ok(Activation::ReplaceBuffer { text, cursor })
        }
        CandidateAction::RequestAi => Ok(Activation::RequestAi),
        CandidateAction::ConfigureAi => Ok(Activation::ConfigureAi),
        CandidateAction::Retry => Ok(Activation::Retry),
        CandidateAction::None => Ok(Activation::None),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{completion::SyncQuality, shell::ShellKind, terminal::BufferRevision};

    #[test]
    fn history_insert_activation_only_replaces_the_buffer() {
        let revision = BufferRevision::new(1);
        let buffer = BufferSnapshot::new("ec", 2, revision, SyncQuality::Exact)
            .expect("buffer should be valid");
        let context = CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from("/tmp"),
            buffer.clone(),
        )
        .expect("context should build");
        let insert = Candidate::new(
            context.query_id,
            "echo ok",
            "history",
            Some(TextEdit {
                range: 0..2,
                replacement: "echo ok".into(),
                cursor_after: CursorPlacement::End,
            }),
            CandidateAction::Insert,
            CandidateSource::History,
            CandidateKind::History,
            Completeness::Runnable,
            RiskLevel::Low,
            "history",
        );
        // Every candidate — including a runnable history entry — activates as a
        // buffer replacement (edit-back). The runtime decides what happens
        // next: Tab stops at the fill, Enter on a selection executes runnable
        // candidates (after confirmation when dangerous).
        assert_eq!(
            activate_candidate(&insert, &context, &buffer).expect("insert should activate"),
            Activation::ReplaceBuffer {
                text: "echo ok".into(),
                cursor: 7
            }
        );
    }
}
