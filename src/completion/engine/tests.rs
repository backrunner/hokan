use std::{cell::Cell, path::PathBuf};

use super::*;
use crate::{
    completion::{
        BufferSnapshot, CandidateAction, CandidateKind, CandidateSource, Completeness, SyncQuality,
    },
    shell::ShellKind,
    terminal::{BufferRevision, QueryId, RiskLevel},
};

struct ManyProvider(usize);

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
            candidates: (0..self.0)
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
    engine.register(ManyProvider(30));
    assert_eq!(engine.complete(&context).candidates.len(), 30);
}

#[test]
fn unlimited_candidates_keep_all_rows_while_explicit_limits_still_apply() {
    for (limit, expected) in [(0, 1_205), (75, 75)] {
        let mut engine = CompletionEngine::new(limit, 3);
        engine.register(ManyProvider(1_205));
        assert_eq!(engine.complete(&context()).candidates.len(), expected);
    }
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
fn budget_flush_keeps_collecting_later_sources() {
    let context = context();
    let mut engine = CompletionEngine::new(100, 3).with_local_timeout(Duration::from_millis(3));
    for primary in ["x-first", "x-second", "x-third", "x-fourth", "x-fifth"] {
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
    assert_eq!(batches, vec![(1, 0, false), (4, 0, false), (5, 0, true)]);
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
fn local_budget_publishes_without_skipping_the_next_provider() {
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
    assert_eq!(*batches.borrow(), vec![(1, 0, false), (2, 0, true)]);
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
    let buffer =
        BufferSnapshot::new("x", 1, BufferRevision::new(1), SyncQuality::Exact).expect("buffer");
    CompletionContext::new(
        QueryId::new(1),
        ShellKind::Zsh,
        PathBuf::from("/tmp"),
        buffer,
    )
    .expect("context")
}
