use std::{hint::black_box, path::PathBuf, time::Instant};

use hokan::{
    completion::{
        BufferSnapshot, Candidate, CandidateAction, CandidateKind, CandidateProvider,
        CandidateSource, Completeness, CompletionContext, CompletionEngine, ProviderOutput,
        SyncQuality,
    },
    shell::ShellKind,
    terminal::{BufferRevision, QueryId, RiskLevel},
};

struct FixtureProvider(usize, usize);

impl CandidateProvider for FixtureProvider {
    fn id(&self) -> &'static str {
        "benchmark"
    }

    fn applies(&self, _: &CompletionContext) -> bool {
        true
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        ProviderOutput {
            candidates: (0..self.1)
                .map(|row| {
                    Candidate::new(
                        context.query_id,
                        format!("command-{}-{row}", self.0),
                        "completion pipeline benchmark",
                        None,
                        CandidateAction::None,
                        CandidateSource::Diagnostic,
                        CandidateKind::Diagnostic,
                        Completeness::ActionOnly,
                        RiskLevel::Low,
                        "benchmark",
                    )
                })
                .collect(),
            diagnostics: Vec::new(),
        }
    }
}

/// Measures collection/ranking/batching independently of filesystem and shell
/// latency. Run in release mode with --ignored --nocapture --test-threads=1.
#[test]
#[ignore = "manual release-mode completion pipeline benchmark"]
fn completion_pipeline_throughput() {
    measure_pipeline(8, 64, 100);
}

#[test]
#[ignore = "manual release-mode unlimited completion pipeline benchmark"]
fn unlimited_completion_pipeline_throughput() {
    measure_pipeline(8, 256, 0);
}

fn measure_pipeline(sources: usize, rows_per_source: usize, limit: usize) {
    let mut engine = CompletionEngine::new(limit, 8);
    for index in 0..sources {
        engine.register(FixtureProvider(index, rows_per_source));
    }
    let context = CompletionContext::new(
        QueryId::new(1),
        ShellKind::Zsh,
        PathBuf::from("/tmp"),
        BufferSnapshot::new("com", 3, BufferRevision::new(1), SyncQuality::Exact).expect("buffer"),
    )
    .expect("context");
    for _ in 0..100 {
        black_box(engine.complete(&context));
    }
    let mut durations = Vec::new();
    let mut batches = 0;
    for _ in 0..1_000 {
        let started = Instant::now();
        engine.complete_incremental(
            black_box(&context),
            |output, final_batch| {
                batches += 1;
                if final_batch {
                    let expected = if limit == 0 {
                        sources * rows_per_source
                    } else {
                        limit
                    };
                    assert_eq!(output.candidates.len(), expected);
                }
                black_box(output);
            },
            || false,
        );
        durations.push(started.elapsed());
    }
    durations.sort_unstable();
    eprintln!(
        "{sources} sources x {rows_per_source} candidates, limit={limit}, 1000 queries: p50={:?}, p95={:?}, batches/query={:.2}",
        durations[500],
        durations[950],
        batches as f64 / 1_000.0,
    );
}
