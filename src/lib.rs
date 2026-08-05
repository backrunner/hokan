#![forbid(unsafe_code)]

pub mod ai;
pub mod app;
pub mod cli;
pub mod completion;
pub mod config;
pub mod diagnostics;
pub mod history;
pub mod parser;
pub mod platform;
pub mod project;
pub mod providers;
pub mod pty;
pub mod safety;
pub mod shell;
pub mod specs;
pub mod terminal;
pub mod update;

#[must_use]
pub fn history_now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().min(i64::MAX as u128) as i64
        })
}

mod error;

pub use error::{Error, Result};
