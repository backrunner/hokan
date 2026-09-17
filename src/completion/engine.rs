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
/// degradation — budget cutoffs, partial scans — and stay out of the
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
            max_candidates: max_candidates.max(1),
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
        // The budget counts provider execution only — ranking, dedupe, and
        // emit work happen outside provider implementations and must not
        // starve later providers on slower machines.
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
            let budget_cutoff = !final_provider && budget_reached && !last_ranked.is_empty();
            if budget_cutoff {
                combined.diagnostics.push(ProviderDiagnostic {
                    provider: "engine",
                    code: "HK-CMP-001",
                    level: DiagnosticLevel::Info,
                    message: format!(
                        "local provider budget reached after {} ms",
                        self.local_timeout.as_millis()
                    ),
                });
            }
            if final_provider || budget_cutoff {
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
mod tests {
    use std::{cell::Cell, path::PathBuf};

    use super::*;
    use crate::{
        completion::{
            BufferSnapshot, CandidateAction, CandidateKind, CandidateSource, Completeness,
            SyncQuality,
        },
        shell::ShellKind,
        terminal::{BufferRevision, QueryId, RiskLevel},
    };

    struct ManyProvider;

    struct OneProvider {
        id: &'static str,
        primary: &'static str,
    }

    struct EmptyProvider;

    struct PanickingProvider;

    struct PanickingModeProvider;

    struct HistoryOnlyProvider;

    impl CandidateProvider for ManyProvider {
        fn id(&self) -> &'static str {
            "many"
        }

        fn applies(&self, _: &CompletionContext) -> bool {
            true
        }

        fn complete(&self, context: &CompletionContext) -> ProviderOutput {
            ProviderOutput {
                candidates: (0..30)
                    .map(|index| {
                        Candidate::new(
                            context.query_id,
                            format!("x{index}"),
                            "candidate",
                            None,
                            CandidateAction::None,
                            CandidateSource::Diagnostic,
                            CandidateKind::Diagnostic,
                            Completeness::ActionOnly,
                            RiskLevel::Low,
                            format!("many:{index}"),
                        )
                    })
                    .collect(),
                diagnostics: Vec::new(),
            }
        }
    }

    impl CandidateProvider for EmptyProvider {
        fn id(&self) -> &'static str {
            "empty"
        }

        fn applies(&self, _: &CompletionContext) -> bool {
            true
        }

        fn complete(&self, _: &CompletionContext) -> ProviderOutput {
            ProviderOutput::default()
        }
    }

    impl CandidateProvider for OneProvider {
        fn id(&self) -> &'static str {
            self.id
        }

        fn applies(&self, _: &CompletionContext) -> bool {
            true
        }

        fn complete(&self, context: &CompletionContext) -> ProviderOutput {
            ProviderOutput {
                candidates: vec![Candidate::new(
                    context.query_id,
                    self.primary,
                    "candidate",
                    None,
                    CandidateAction::None,
                    CandidateSource::Diagnostic,
                    CandidateKind::Diagnostic,
                    Completeness::ActionOnly,
                    RiskLevel::Low,
                    self.id,
                )],
                diagnostics: Vec::new(),
            }
        }
    }

    impl CandidateProvider for PanickingProvider {
        fn id(&self) -> &'static str {
            "panicking"
        }

        fn applies(&self, _: &CompletionContext) -> bool {
            true
        }

        fn complete(&self, _: &CompletionContext) -> ProviderOutput {
            panic!("provider payload must not reach diagnostics")
        }
    }

    impl CandidateProvider for PanickingModeProvider {
        fn id(&self) -> &'static str {
            "panicking_mode"
        }

        fn supports_mode(&self, _: CompletionMode) -> bool {
            panic!("mode payload must stay private")
        }

        fn applies(&self, _: &CompletionContext) -> bool {
            true
        }

        fn complete(&self, _: &CompletionContext) -> ProviderOutput {
            ProviderOutput::default()
        }
    }

    impl CandidateProvider for HistoryOnlyProvider {
        fn id(&self) -> &'static str {
            "history_only"
        }

        fn supports_mode(&self, _: CompletionMode) -> bool {
            true
        }

        fn applies(&self, _: &CompletionContext) -> bool {
            true
        }

        fn complete(&self, context: &CompletionContext) -> ProviderOutput {
            ProviderOutput {
                candidates: vec![Candidate::new(
                    context.query_id,
                    "history row",
                    "history",
                    None,
                    CandidateAction::None,
                    CandidateSource::History,
                    CandidateKind::History,
                    Completeness::Runnable,
                    RiskLevel::Low,
                    "history_only",
                )],
                diagnostics: Vec::new(),
            }
        }
    }

    #[test]
    fn keeps_ranked_candidates_beyond_the_visible_page() {
        let context = context();
        let mut engine = CompletionEngine::new(100, 3);
        engine.register(ManyProvider);
        assert_eq!(engine.complete(&context).candidates.len(), 30);
    }

    #[test]
    fn emits_cumulative_batches_and_stops_at_provider_boundaries() {
        let context = context();
        let mut engine = CompletionEngine::new(100, 3);
        engine.register(OneProvider {
            id: "first",
            primary: "x-first",
        });
        engine.register(OneProvider {
            id: "second",
            primary: "x-second",
        });
        let batches = std::cell::RefCell::new(Vec::new());
        engine.complete_incremental(
            &context,
            |output, final_batch| {
                batches
                    .borrow_mut()
                    .push((output.candidates.len(), final_batch));
            },
            || false,
        );
        assert_eq!(*batches.borrow(), vec![(1, false), (2, true)]);

        let cancelled = Cell::new(false);
        let count = Cell::new(0);
        engine.complete_incremental(
            &context,
            |_, _| {
                count.set(count.get() + 1);
                cancelled.set(true);
            },
            || cancelled.get(),
        );
        assert_eq!(count.get(), 1);
    }

    #[test]
    fn fast_sources_coalesce_without_losing_final_candidates() {
        let context = context();
        let mut engine = CompletionEngine::new(100, 3);
        engine.register(EmptyProvider);
        for primary in ["x-first", "x-second", "x-third"] {
            engine.register(OneProvider {
                id: "fixture",
                primary,
            });
        }
        engine.register(EmptyProvider);
        let mut batches = Vec::new();
        let now = Instant::now();
        engine.complete_with_clock(
            &context,
            |output, final_batch| batches.push((output.candidates.len(), final_batch)),
            || false,
            |_| {},
            || now,
        );
        assert_eq!(batches, vec![(1, false), (3, true)]);
    }

    #[test]
    fn slower_sources_keep_publishing_at_the_frame_cadence() {
        let context = context();
        let mut engine = CompletionEngine::new(100, 3);
        for primary in ["x-first", "x-second", "x-third"] {
            engine.register(OneProvider {
                id: "fixture",
                primary,
            });
        }
        let clock = Cell::new(Instant::now());
        let mut batches = Vec::new();
        engine.complete_with_clock(
            &context,
            |output, final_batch| batches.push((output.candidates.len(), final_batch)),
            || false,
            |_| clock.set(clock.get() + BATCH_INTERVAL),
            || clock.get(),
        );
        assert_eq!(batches, vec![(1, false), (2, false), (3, true)]);
    }

    #[test]
    fn empty_sources_only_publish_a_final_result() {
        let mut engine = CompletionEngine::new(100, 3);
        engine.register(EmptyProvider);
        engine.register(EmptyProvider);
        let mut batches = Vec::new();
        engine.complete_incremental(
            &context(),
            |output, final_batch| batches.push((output.candidates.len(), final_batch)),
            || false,
        );
        assert_eq!(batches, vec![(0, true)]);
    }

    #[test]
    fn budget_cutoff_includes_candidates_accumulated_since_the_last_batch() {
        let context = context();
        let mut engine = CompletionEngine::new(100, 3).with_local_timeout(Duration::from_millis(3));
        for primary in ["x-first", "x-second", "x-third", "x-never"] {
            engine.register(OneProvider {
                id: "fixture",
                primary,
            });
        }
        let clock = Cell::new(Instant::now());
        let mut batches = Vec::new();
        engine.complete_with_clock(
            &context,
            |output, final_batch| {
                batches.push((
                    output.candidates.len(),
                    output.diagnostics.len(),
                    final_batch,
                ));
            },
            || false,
            |_| {},
            || {
                let now = clock.get();
                clock.set(now + Duration::from_millis(1));
                now
            },
        );
        assert_eq!(batches, vec![(1, 0, false), (3, 1, true)]);
    }

    #[test]
    fn history_only_mode_excludes_normal_providers_before_ranking() {
        let context = CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from("/tmp"),
            BufferSnapshot::new("", 0, BufferRevision::new(1), SyncQuality::Exact).expect("buffer"),
        )
        .expect("context")
        .with_mode(CompletionMode::HistoryOnly);
        let mut engine = CompletionEngine::new(1, 1);
        engine.register(OneProvider {
            id: "normal",
            primary: "normal row",
        });
        engine.register(HistoryOnlyProvider);

        let output = engine.complete(&context);
        assert_eq!(output.candidates.len(), 1);
        assert_eq!(output.candidates[0].source, CandidateSource::History);
        assert_eq!(output.candidates[0].display.primary, "history row");
    }

    #[test]
    fn shell_comments_do_not_produce_candidates() {
        let context = CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from("/tmp"),
            BufferSnapshot::new(
                "echo ok # unfinished note",
                "echo ok # unfinished note".len(),
                BufferRevision::new(1),
                SyncQuality::Exact,
            )
            .expect("buffer"),
        )
        .expect("context");
        let mut engine = CompletionEngine::new(100, 3);
        engine.register(OneProvider {
            id: "always",
            primary: "must-not-appear",
        });
        assert!(engine.complete(&context).candidates.is_empty());
    }

    #[test]
    fn local_budget_stops_before_the_next_provider() {
        let context = context();
        let mut engine = CompletionEngine::new(100, 3).with_local_timeout(Duration::ZERO);
        engine.register(OneProvider {
            id: "first",
            primary: "x-first",
        });
        engine.register(OneProvider {
            id: "second",
            primary: "x-second",
        });
        let batches = std::cell::RefCell::new(Vec::new());
        engine.complete_incremental(
            &context,
            |output, final_batch| {
                batches.borrow_mut().push((
                    output.candidates.len(),
                    output.diagnostics.len(),
                    final_batch,
                ));
            },
            || false,
        );
        assert_eq!(*batches.borrow(), vec![(1, 1, true)]);
    }

    #[test]
    fn local_budget_does_not_finalize_an_empty_result_before_a_later_provider() {
        let context = context();
        let mut engine = CompletionEngine::new(100, 3).with_local_timeout(Duration::ZERO);
        engine.register(EmptyProvider);
        engine.register(OneProvider {
            id: "fallback",
            primary: "x-fallback",
        });

        let output = engine.complete(&context);
        assert_eq!(output.candidates.len(), 1);
        assert_eq!(output.candidates[0].display.primary, "x-fallback");
    }

    #[test]
    fn provider_panics_are_isolated_without_exposing_the_payload() {
        let context = context();
        let mut engine = CompletionEngine::new(100, 3);
        engine.register(PanickingProvider);
        engine.register(PanickingModeProvider);
        engine.register(OneProvider {
            id: "healthy",
            primary: "x-healthy",
        });

        let output = engine.complete(&context);

        assert_eq!(output.candidates.len(), 1);
        assert_eq!(output.candidates[0].display.primary, "x-healthy");
        let diagnostic = output
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.provider == "panicking")
            .expect("panic diagnostic");
        assert_eq!(diagnostic.code, "HK-CMP-002");
        assert!(!diagnostic.message.contains("payload"));
        let mode_diagnostic = output
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.provider == "panicking_mode")
            .expect("mode panic diagnostic");
        assert_eq!(mode_diagnostic.code, "HK-CMP-002");
        assert!(!mode_diagnostic.message.contains("payload"));
    }

    #[test]
    fn reports_typed_provider_metrics_without_query_text() {
        let context = context();
        let mut engine = CompletionEngine::new(100, 3);
        engine.register(OneProvider {
            id: "observed",
            primary: "candidate",
        });
        let metrics = std::cell::RefCell::new(Vec::new());
        engine.complete_incremental_with_metrics(
            &context,
            |_, _| {},
            || false,
            |metric| metrics.borrow_mut().push(metric),
        );
        let metrics = metrics.borrow();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].provider, "observed");
        assert_eq!(metrics[0].candidate_count, 1);
        assert!(!metrics[0].cancelled);
    }

    fn context() -> CompletionContext {
        let buffer = BufferSnapshot::new("x", 1, BufferRevision::new(1), SyncQuality::Exact)
            .expect("buffer");
        CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from("/tmp"),
            buffer,
        )
        .expect("context")
    }
}
