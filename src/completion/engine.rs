use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    time::Duration,
};

use crate::completion::{Candidate, CompletionContext, rank_and_dedupe};

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
    pub message: String,
}

#[derive(Clone, Debug, Default)]
pub struct ProviderOutput {
    pub candidates: Vec<Candidate>,
    pub diagnostics: Vec<ProviderDiagnostic>,
}

pub trait CandidateProvider: Send + Sync {
    fn id(&self) -> &'static str;
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
        mut emit: impl FnMut(ProviderOutput, bool),
        mut cancelled: impl FnMut() -> bool,
        mut observe: impl FnMut(ProviderMetric),
    ) {
        if context.buffer.sync == crate::completion::SyncQuality::Uncertain {
            emit(ProviderOutput::default(), true);
            return;
        }
        let mut combined = ProviderOutput::default();
        let mut providers = Vec::new();
        for provider in &self.providers {
            match catch_unwind(AssertUnwindSafe(|| provider.applies(context))) {
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
        let started = std::time::Instant::now();
        for (index, provider) in providers.into_iter().enumerate() {
            if cancelled() {
                return;
            }
            if index > 0 && started.elapsed() >= self.local_timeout {
                combined.diagnostics.push(ProviderDiagnostic {
                    provider: "engine",
                    code: "HK-CMP-001",
                    message: format!(
                        "local provider budget reached after {} ms",
                        self.local_timeout.as_millis()
                    ),
                });
                emit(
                    ProviderOutput {
                        candidates: rank_and_dedupe(
                            context,
                            combined.candidates,
                            self.max_candidates,
                        ),
                        diagnostics: combined.diagnostics,
                    },
                    true,
                );
                return;
            }
            let provider_started = std::time::Instant::now();
            let mut output = catch_unwind(AssertUnwindSafe(|| provider.complete(context)))
                .unwrap_or_else(|_| ProviderOutput {
                    candidates: Vec::new(),
                    diagnostics: vec![provider_panic(provider.id())],
                });
            let was_cancelled = cancelled();
            observe(ProviderMetric {
                provider: provider.id(),
                duration: provider_started.elapsed(),
                candidate_count: output.candidates.len(),
                cancelled: was_cancelled,
            });
            if was_cancelled {
                return;
            }
            combined.candidates.append(&mut output.candidates);
            combined.diagnostics.append(&mut output.diagnostics);
            emit(
                ProviderOutput {
                    candidates: rank_and_dedupe(
                        context,
                        combined.candidates.clone(),
                        self.max_candidates,
                    ),
                    diagnostics: combined.diagnostics.clone(),
                },
                index + 1 == provider_count,
            );
        }
    }
}

fn provider_panic(provider: &'static str) -> ProviderDiagnostic {
    ProviderDiagnostic {
        provider,
        code: "HK-CMP-002",
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

    struct PanickingProvider;

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
        assert_eq!(*batches.borrow(), vec![(1, 0, false), (1, 1, true)]);
    }

    #[test]
    fn provider_panics_are_isolated_without_exposing_the_payload() {
        let context = context();
        let mut engine = CompletionEngine::new(100, 3);
        engine.register(PanickingProvider);
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
            .find(|diagnostic| diagnostic.code == "HK-CMP-002")
            .expect("panic diagnostic");
        assert_eq!(diagnostic.provider, "panicking");
        assert!(!diagnostic.message.contains("payload"));
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
