use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    time::{Duration, Instant},
};

use crate::completion::{Candidate, CompletionContext, CompletionMode, rank_and_dedupe};

// Match the overlay's frame cadence: publish the first usable result promptly,
// then merge fast sources before paying for another cumulative rank and clone.
const BATCH_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Clone, Copy, Debug)]
pub struct ProviderMetric {
    pub provider: &'static str,
    pub duration: Duration,
    pub candidate_count: usize,
    pub cancelled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderDiagnostic {
    pub provider: &'static str,
    pub code: &'static str,
    pub level: DiagnosticLevel,
    pub message: String,
}

/// Severity of a provider diagnostic. `Info` entries record normal
/// degradation — partial scans — and stay out of the
/// overlay status line; `Warning` marks a provider that failed to produce
/// its result, which the status line surfaces so an incomplete row set
/// does not look like a correct one.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DiagnosticLevel {
    Info,
    #[default]
    Warning,
}

impl DiagnosticLevel {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ProviderOutput {
    pub candidates: Vec<Candidate>,
    pub diagnostics: Vec<ProviderDiagnostic>,
}

pub trait CandidateProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn supports_mode(&self, mode: CompletionMode) -> bool {
        mode == CompletionMode::Normal
    }
    fn applies(&self, context: &CompletionContext) -> bool;
    fn complete(&self, context: &CompletionContext) -> ProviderOutput;
}

#[derive(Default)]
pub struct CompletionEngine {
    providers: Vec<Arc<dyn CandidateProvider>>,
    max_candidates: usize,
    local_timeout: Duration,
}

impl CompletionEngine {
    #[must_use]
    pub fn new(max_candidates: usize, _max_visible: usize) -> Self {
        Self {
            providers: Vec::new(),
            max_candidates: if max_candidates == 0 {
                usize::MAX
            } else {
                max_candidates
            },
            local_timeout: Duration::from_millis(100),
        }
    }

    #[must_use]
    pub const fn with_local_timeout(mut self, timeout: Duration) -> Self {
        self.local_timeout = timeout;
        self
    }

    pub fn register(&mut self, provider: impl CandidateProvider + 'static) {
        self.providers.push(Arc::new(provider));
    }

    #[cfg(test)]
    pub(crate) fn provider_ids(&self) -> Vec<&'static str> {
        self.providers
            .iter()
            .map(|provider| provider.id())
            .collect()
    }

    #[must_use]
    pub fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let mut final_output = ProviderOutput::default();
        self.complete_incremental(context, |output, _| final_output = output, || false);
        final_output
    }

    pub fn complete_incremental(
        &self,
        context: &CompletionContext,
        emit: impl FnMut(ProviderOutput, bool),
        cancelled: impl FnMut() -> bool,
    ) {
        self.complete_incremental_with_metrics(context, emit, cancelled, |_| {});
    }

    pub fn complete_incremental_with_metrics(
        &self,
        context: &CompletionContext,
        emit: impl FnMut(ProviderOutput, bool),
        cancelled: impl FnMut() -> bool,
        observe: impl FnMut(ProviderMetric),
    ) {
        self.complete_with_clock(context, emit, cancelled, observe, Instant::now);
    }

    fn complete_with_clock(
        &self,
        context: &CompletionContext,
        mut emit: impl FnMut(ProviderOutput, bool),
        mut cancelled: impl FnMut() -> bool,
        mut observe: impl FnMut(ProviderMetric),
        mut now: impl FnMut() -> Instant,
    ) {
        if cancelled() {
            return;
        }
        if context.buffer.sync == crate::completion::SyncQuality::Uncertain {
            emit(ProviderOutput::default(), true);
            return;
        }
        if context.parsed.tokens.iter().any(|token| {
            token.kind == crate::parser::TokenKind::Comment
                && context.buffer.cursor >= token.range.start
                && context.buffer.cursor <= token.range.end
        }) {
            emit(ProviderOutput::default(), true);
            return;
        }
        let mut combined = ProviderOutput::default();
        let mut providers = Vec::new();
        for provider in &self.providers {
            if cancelled() {
                return;
            }
            match catch_unwind(AssertUnwindSafe(|| {
                provider.supports_mode(context.mode) && provider.applies(context)
            })) {
                Ok(true) => providers.push(provider),
                Ok(false) => {}
                Err(_) => combined.diagnostics.push(provider_panic(provider.id())),
            }
        }
        if providers.is_empty() {
            emit(combined, true);
            return;
        }
        let provider_count = providers.len();
        // The budget bounds work between incremental batches, not the source
        // set. Publish what is ready and continue so slow sources cannot hide
        // later commands. Cancellation still stops superseded queries.
        let mut provider_elapsed = Duration::ZERO;
        let mut last_ranked: Vec<Candidate> = Vec::new();
        let mut candidates_changed = false;
        let mut last_emitted: Option<Instant> = None;
        for (index, provider) in providers.into_iter().enumerate() {
            if cancelled() {
                return;
            }
            let provider_started = now();
            let mut output = catch_unwind(AssertUnwindSafe(|| provider.complete(context)))
                .unwrap_or_else(|_| ProviderOutput {
                    candidates: Vec::new(),
                    diagnostics: vec![provider_panic(provider.id())],
                });
            let duration = now().saturating_duration_since(provider_started);
            provider_elapsed += duration;
            let was_cancelled = cancelled();
            observe(ProviderMetric {
                provider: provider.id(),
                duration,
                candidate_count: output.candidates.len(),
                cancelled: was_cancelled,
            });
            if was_cancelled {
                return;
            }
            candidates_changed |= !output.candidates.is_empty();
            combined.candidates.append(&mut output.candidates);
            combined.diagnostics.append(&mut output.diagnostics);
            let final_provider = index + 1 == provider_count;
            let budget_reached = provider_elapsed >= self.local_timeout;
            let batch_due = last_emitted
                .is_none_or(|last| now().saturating_duration_since(last) >= BATCH_INTERVAL);
            if !final_provider && !budget_reached && (!candidates_changed || !batch_due) {
                continue;
            }
            if candidates_changed {
                // The final pass owns the raw candidates; intermediate passes
                // keep them so cross-source matching and dedupe stay correct.
                let candidates = if final_provider {
                    std::mem::take(&mut combined.candidates)
                } else {
                    combined.candidates.clone()
                };
                last_ranked = rank_and_dedupe(context, candidates, self.max_candidates);
                candidates_changed = false;
            }
            if cancelled() {
                return;
            }
            if final_provider {
                emit(
                    ProviderOutput {
                        candidates: last_ranked,
                        diagnostics: combined.diagnostics,
                    },
                    true,
                );
                return;
            }
            // An empty intermediate batch is not evidence of no matches:
            // later providers can still supply rows. Never close the overlay
            // just because an earlier source had nothing to contribute.
            if last_ranked.is_empty() {
                continue;
            }
            emit(
                ProviderOutput {
                    candidates: last_ranked.clone(),
                    diagnostics: combined.diagnostics.clone(),
                },
                false,
            );
            last_emitted = Some(now());
            provider_elapsed = Duration::ZERO;
        }
    }
}

fn provider_panic(provider: &'static str) -> ProviderDiagnostic {
    ProviderDiagnostic {
        provider,
        code: "HK-CMP-002",
        level: DiagnosticLevel::Warning,
        message: format!("provider {provider} failed internally; other sources remain available"),
    }
}

#[cfg(test)]
mod tests;
