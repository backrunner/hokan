use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("terminal protocol error: {0}")]
    TerminalProtocol(String),

    #[error("invalid terminal geometry: {0}")]
    InvalidGeometry(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("shell integration error: {0}")]
    Shell(String),

    #[error("PTY error: {0}")]
    Pty(String),

    #[error("parse error: {0}")]
    Parse(String),

    #[error("completion error: {0}")]
    Completion(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("history error: {0}")]
    History(String),

    #[error("project error: {0}")]
    Project(String),

    #[error("runtime error: {0}")]
    Runtime(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
