//! Shell command segments and working-directory flow for history validation.
use crate::completion::CompletionContext;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HistorySegmentLink {
    Always,
    OnSuccess,
    OnFailure,
    Pipe,
    Background,
    End,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct HistorySegment {
    pub(super) words: Vec<String>,
    pub(super) next: HistorySegmentLink,
    pub(super) backgrounded: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HistoryCommandStatus {
    Success,
    Failure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct HistoryFlowState {
    pub(super) cwd: PathBuf,
    pub(super) status: HistoryCommandStatus,
    pub(super) active: bool,
    pub(super) background_cwd: Option<PathBuf>,
}

pub(super) const MAX_HISTORY_FLOW_STATES: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum HistoryDirectoryChange {
    NotApplicable,
    Failed,
    Known(PathBuf),
    Unknown,
}

pub(super) fn push_history_flow_state(states: &mut Vec<HistoryFlowState>, state: HistoryFlowState) {
    if !states.contains(&state) {
        states.push(state);
    }
}

pub(super) fn command_segment_words(command: &str) -> Option<Vec<HistorySegment>> {
    let parsed = crate::parser::parse_line(command, command.len()).ok()?;
    if parsed
        .tokens
        .iter()
        .any(|token| token.quote == crate::parser::QuoteContext::Opaque)
    {
        return None;
    }
    let mut segments = Vec::new();
    let mut start = 0;
    for token in &parsed.tokens {
        if matches!(token.kind, crate::parser::TokenKind::Comment) {
            push_history_segment(
                &mut segments,
                &parsed.tokens,
                start..token.range.start,
                HistorySegmentLink::End,
            );
            start = command.len();
            break;
        }
        let next = match token.kind {
            crate::parser::TokenKind::Pipe => Some(HistorySegmentLink::Pipe),
            crate::parser::TokenKind::AndIf => Some(HistorySegmentLink::OnSuccess),
            crate::parser::TokenKind::OrIf => Some(HistorySegmentLink::OnFailure),
            crate::parser::TokenKind::Separator
                if command
                    .get(token.range.clone())
                    .is_some_and(|operator| operator == "&") =>
            {
                Some(HistorySegmentLink::Background)
            }
            crate::parser::TokenKind::Separator => Some(HistorySegmentLink::Always),
            _ => None,
        };
        if let Some(next) = next {
            push_history_segment(
                &mut segments,
                &parsed.tokens,
                start..token.range.start,
                next,
            );
            start = token.range.end;
        }
    }
    if start < command.len() {
        push_history_segment(
            &mut segments,
            &parsed.tokens,
            start..command.len(),
            HistorySegmentLink::End,
        );
    }
    mark_history_background_groups(&mut segments);
    Some(segments)
}

pub(super) fn push_history_segment(
    segments: &mut Vec<HistorySegment>,
    tokens: &[crate::parser::Token],
    range: std::ops::Range<usize>,
    next: HistorySegmentLink,
) {
    let words = crate::parser::semantic_word_tokens(tokens, &range)
        .into_iter()
        .map(|token| token.cooked_prefix.clone())
        .collect::<Vec<_>>();
    if !words.is_empty() {
        segments.push(HistorySegment {
            words,
            next,
            backgrounded: false,
        });
    }
}

pub(super) fn mark_history_background_groups(segments: &mut [HistorySegment]) {
    let mut group_start = 0;
    for index in 0..segments.len() {
        match segments[index].next {
            HistorySegmentLink::Background => {
                for segment in &mut segments[group_start..=index] {
                    segment.backgrounded = true;
                }
                group_start = index + 1;
            }
            HistorySegmentLink::Always | HistorySegmentLink::End => {
                group_start = index + 1;
            }
            HistorySegmentLink::OnSuccess
            | HistorySegmentLink::OnFailure
            | HistorySegmentLink::Pipe => {}
        }
    }
}

pub(super) fn history_directory_change(
    context: &CompletionContext,
    words: &[String],
    aliases: &crate::shell::ShellAliases,
) -> HistoryDirectoryChange {
    let cooked = words.iter().map(String::as_str).collect::<Vec<_>>();
    let analysis =
        crate::parser::effective_command_analysis_for_shell(&cooked, false, context.shell);
    let command_index = match analysis.state {
        crate::parser::EffectiveCommandState::Found(index) => index,
        crate::parser::EffectiveCommandState::WrapperCommand(_)
        | crate::parser::EffectiveCommandState::IndeterminateWrapper(_)
        | crate::parser::EffectiveCommandState::AwaitingCommand
        | crate::parser::EffectiveCommandState::AwaitingWrapperValue => {
            return HistoryDirectoryChange::NotApplicable;
        }
    };
    let Some(command) = cooked.get(command_index).copied() else {
        return HistoryDirectoryChange::NotApplicable;
    };
    if crate::providers::executable_basename(command) != "cd"
        || command.contains('/')
        || analysis.privileged
        || analysis.opaque
        || analysis.kind == crate::parser::EffectiveCommandKind::External
    {
        return HistoryDirectoryChange::NotApplicable;
    }
    if analysis.kind == crate::parser::EffectiveCommandKind::Shell && aliases.contains(command) {
        return HistoryDirectoryChange::Unknown;
    }
    if cooked[..command_index]
        .iter()
        .any(|word| matches!(*word, "!" | "not" | "and" | "or"))
    {
        return HistoryDirectoryChange::Unknown;
    }

    let mut path = None;
    let mut options = true;
    for argument in cooked.get(command_index + 1..).unwrap_or_default() {
        if options && *argument == "--" {
            options = false;
            continue;
        }
        if options && argument.starts_with('-') {
            if *argument == "-"
                || argument.len() == 1
                || !argument[1..].chars().all(|flag| matches!(flag, 'L' | 'P'))
            {
                return HistoryDirectoryChange::Unknown;
            }
            continue;
        }
        options = false;
        if path.replace(*argument).is_some() {
            return HistoryDirectoryChange::Unknown;
        }
    }

    let target = match path {
        Some("") => return HistoryDirectoryChange::Failed,
        Some(value) => {
            if value
                .chars()
                .any(|character| matches!(character, '$' | '`' | '*' | '?' | '[' | '{'))
                || (value.starts_with('~') && value != "~" && !value.starts_with("~/"))
            {
                return HistoryDirectoryChange::Unknown;
            }
            resolve_history_path(&context.cwd, value)
        }
        None => {
            let Some(home) = std::env::home_dir() else {
                return HistoryDirectoryChange::Failed;
            };
            home
        }
    };
    match std::fs::canonicalize(target) {
        Ok(directory) if directory.is_dir() => HistoryDirectoryChange::Known(directory),
        Ok(_) | Err(_) => HistoryDirectoryChange::Failed,
    }
}

pub(super) fn resolve_history_path(base: &Path, value: &str) -> std::path::PathBuf {
    if value == "~"
        && let Some(home) = std::env::home_dir()
    {
        return home;
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = std::env::home_dir()
    {
        return home.join(rest);
    }
    let value = std::path::PathBuf::from(value);
    if value.is_absolute() {
        value
    } else {
        base.join(value)
    }
}
