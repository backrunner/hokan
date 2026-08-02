use std::{path::PathBuf, sync::Arc};

use crate::{
    parser::{ParsedLine, parse_line},
    shell::ShellKind,
    terminal::{BufferRevision, QueryId},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncQuality {
    Exact,
    Mirrored,
    Uncertain,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BufferSnapshot {
    pub text: Arc<str>,
    pub cursor: usize,
    pub revision: BufferRevision,
    pub sync: SyncQuality,
    pub hash: u32,
}

impl BufferSnapshot {
    pub fn new(
        text: impl Into<Arc<str>>,
        cursor: usize,
        revision: BufferRevision,
        sync: SyncQuality,
    ) -> crate::Result<Self> {
        let text = text.into();
        if cursor > text.len() || !text.is_char_boundary(cursor) {
            return Err(crate::Error::Parse(
                "buffer cursor does not fall on a UTF-8 boundary".into(),
            ));
        }
        let hash = crc32fast::hash(text.as_bytes());
        Ok(Self {
            text,
            cursor,
            revision,
            sync,
            hash,
        })
    }
}

#[derive(Clone, Debug)]
pub struct CompletionContext {
    pub query_id: QueryId,
    pub shell: ShellKind,
    pub platform: &'static str,
    pub cwd: Arc<PathBuf>,
    pub buffer: BufferSnapshot,
    pub parsed: ParsedLine,
}

impl CompletionContext {
    pub fn new(
        query_id: QueryId,
        shell: ShellKind,
        cwd: PathBuf,
        buffer: BufferSnapshot,
    ) -> crate::Result<Self> {
        let parsed = parse_line(&buffer.text, buffer.cursor)?;
        Ok(Self {
            query_id,
            shell,
            platform: std::env::consts::OS,
            cwd: Arc::new(cwd),
            buffer,
            parsed,
        })
    }

    #[must_use]
    pub fn command(&self) -> Option<&str> {
        self.parsed.command.as_deref()
    }
}
