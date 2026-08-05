use std::{sync::Arc, thread};

use super::{
    output_error,
    render::render_current,
    state::{ActiveAiRequest, RuntimeState, landing_row, selected_candidate},
    worker::ProviderResult,
};
use crate::{
    ai::{AiClient, build_context},
    completion::{
        Activation, Candidate, CandidateAction, CandidateSource, Completeness, CompletionContext,
        SyncQuality, activate_candidate, rank_and_dedupe, stricter_risk,
    },
    config::Config,
    providers::ai_result_candidates,
    safety::classify_command,
    terminal::{OutputHandle, QueryId},
};
use crossbeam_channel::Sender;
use tokio_util::sync::CancellationToken;

pub(super) struct AiResult {
    pub(super) query_id: QueryId,
    pub(super) generation: u64,
    pub(super) result: Result<Vec<crate::ai::AiCommand>, crate::ai::AiClientError>,
}

pub(super) enum SelectedActivation {
    None,
    Ready {
        activation: Activation,
        context: Arc<CompletionContext>,
    },
    Rejected,
}

/// Outcome of pressing the activate key (Enter) with a selection.
pub(super) enum EnterResolution {
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

pub(super) fn resolve_enter(candidate: &Candidate, activation: &Activation) -> EnterResolution {
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
    // Only the highest-risk commands (recursive/force destructive operations
    // and the like) gate execution behind a confirmation. Merely running an
    // executable — including opaque lines the classifier cannot see through
    // (`$(...)`, `eval`, substitutions) — is not dangerous by itself.
    if matches!(risk, crate::terminal::RiskLevel::High) {
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

pub(super) fn resolve_selected_activation(
    state: &RuntimeState,
) -> crate::Result<SelectedActivation> {
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

pub(super) fn start_ai_request(
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
    state.ai_generation = state.ai_generation.wrapping_add(1);
    let generation = state.ai_generation;
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
            let _ = sender.send(AiResult {
                query_id,
                generation,
                result,
            });
        })?;
    state.ai_query = Some(ActiveAiRequest {
        query_id,
        generation,
        cancel,
    });
    state.ai_owns_candidates = true;
    if let Some(log) = &state.debug_log {
        log.ai_event("started");
    }
    state.candidates.clear();
    state.selected = None;
    state.status = Some("HK-AI-WAIT requesting commands; Esc cancels".into());
    render_current(state, output)
}

pub(super) fn handle_provider_result(
    mut result: ProviderResult,
    state: &mut RuntimeState,
    output: &OutputHandle,
) -> crate::Result<()> {
    let Some(current) = state.context.as_ref() else {
        return Ok(());
    };
    // While an AI request is pending — and until the next query after its
    // result landed — the AI wait screen / AI candidates own the overlay.
    // Provider batches for the same query id would pass the staleness check
    // below (the buffer never moved) and wipe the AI rows, so drop them.
    if state.ai_query.is_some() || state.ai_owns_candidates {
        return Ok(());
    }
    if result.context.query_id != current.query_id
        || result.context.buffer.revision != state.buffer.revision
        || result.context.buffer.hash != state.snapshot()?.hash
        // `mark_uncertain` changes neither the revision nor the text, so a
        // late batch still matches on those — but with uncertain sync the
        // overlay rows can never be activated and must not be repainted.
        || state.buffer.sync == SyncQuality::Uncertain
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

pub(super) fn handle_ai_result(
    result: AiResult,
    state: &mut RuntimeState,
    output: &OutputHandle,
) -> crate::Result<()> {
    // Match on the per-request generation, not just the query id: two
    // consecutive AI requests can share a query id (the buffer never moved),
    // and a late result from the cancelled one must not take the active
    // request's slot — that would both paint stale rows and strand the
    // active request uncancellable.
    let active = state
        .ai_query
        .as_ref()
        .map(|request| (request.query_id, request.generation));
    if active != Some((result.query_id, result.generation)) {
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
                    | crate::ai::AiClientError::CodeAssistProject
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
