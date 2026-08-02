mod candidate;
mod context;
mod engine;
mod ranking;

pub use candidate::{
    Activation, Candidate, CandidateAction, CandidateId, CandidateKind, CandidateSource,
    Completeness, CursorPlacement, DisplayText, ScoreSignals, SlotKind, TextEdit,
    activate_candidate,
};
pub use context::{BufferSnapshot, CompletionContext, SyncQuality};
pub use engine::{
    CandidateProvider, CompletionEngine, ProviderDiagnostic, ProviderMetric, ProviderOutput,
};
pub(crate) use ranking::match_quality_folded;
pub use ranking::{match_quality, rank_and_dedupe};
