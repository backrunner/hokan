mod classifier;

pub(crate) use classifier::effective_command_word;
pub use classifier::{RiskAssessment, RiskReason, classify_command};
