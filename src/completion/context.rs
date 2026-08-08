use std::{path::PathBuf, sync::Arc};

use crate::{
    parser::{ParsedLine, parse_line},
    project::WorkspaceMarkers,
    shell::ShellKind,
    terminal::{BufferRevision, QueryId},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncQuality {
    Exact,
    Mirrored,
    Uncertain,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CompletionMode {
    #[default]
    Normal,
    HistoryOnly,
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
    /// Restricts which provider families may participate in this query.
    /// Explicit history focus must be applied before ranking/truncation so a
    /// broad empty-prefix provider cannot crowd every history row out.
    pub mode: CompletionMode,
    /// Last command executed in this session, when known. Session memory
    /// only — used for the transition bigram signal, never persisted.
    pub previous_command: Option<String>,
    /// Workspace markers detected for `cwd`; drives the context bonus.
    pub workspace: WorkspaceMarkers,
}

impl CompletionContext {
    pub fn new(
        query_id: QueryId,
        shell: ShellKind,
        cwd: PathBuf,
        buffer: BufferSnapshot,
    ) -> crate::Result<Self> {
        let mut parsed = parse_line(&buffer.text, buffer.cursor)?;
        let command = {
            let words = crate::parser::semantic_word_tokens(&parsed.tokens, &parsed.active_segment);
            let cooked: Vec<&str> = words
                .iter()
                .map(|token| token.cooked_prefix.as_str())
                .collect();
            crate::parser::effective_command_index_for_shell(&cooked, shell).map(|index| {
                (
                    words[index].cooked_prefix.clone(),
                    words[index].range.clone(),
                )
            })
        };
        (parsed.command, parsed.command_range) = command
            .map(|(command, range)| (Some(command), Some(range)))
            .unwrap_or((None, None));
        Ok(Self {
            query_id,
            shell,
            platform: std::env::consts::OS,
            cwd: Arc::new(cwd),
            buffer,
            parsed,
            mode: CompletionMode::Normal,
            previous_command: None,
            workspace: WorkspaceMarkers::default(),
        })
    }

    #[must_use]
    pub const fn with_mode(mut self, mode: CompletionMode) -> Self {
        self.mode = mode;
        self
    }

    #[must_use]
    pub fn with_previous_command(mut self, previous_command: Option<String>) -> Self {
        self.previous_command = previous_command;
        self
    }

    #[must_use]
    pub fn with_workspace(mut self, workspace: WorkspaceMarkers) -> Self {
        self.workspace = workspace;
        self
    }

    #[must_use]
    pub fn command(&self) -> Option<&str> {
        self.parsed.command.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{BufferRevision, QueryId};

    fn context(shell: ShellKind, text: &str) -> CompletionContext {
        CompletionContext::new(
            QueryId::new(1),
            shell,
            PathBuf::from("/tmp"),
            BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer"),
        )
        .expect("context")
    }

    #[test]
    fn resolves_shell_specific_command_modifiers() {
        for (shell, text, expected) in [
            (ShellKind::Fish, "not cod", "cod"),
            (ShellKind::Fish, "and cod", "cod"),
            (ShellKind::Fish, "or cod", "cod"),
            (ShellKind::Zsh, "nocorrect cod", "cod"),
            (ShellKind::Zsh, "noglob cod", "cod"),
            (ShellKind::Bash, "! cod", "cod"),
        ] {
            let context = context(shell, text);
            assert_eq!(context.command(), Some(expected), "command for {text:?}");
            assert_eq!(
                context
                    .parsed
                    .command_range
                    .as_ref()
                    .map(|range| &text[range.clone()]),
                Some(expected),
                "command range for {text:?}"
            );
        }
    }

    #[test]
    fn does_not_apply_modifiers_from_another_shell_or_inside_external_wrappers() {
        for (shell, text, expected) in [
            (ShellKind::Bash, "not cod", "not"),
            (ShellKind::Zsh, "and cod", "and"),
            (ShellKind::Fish, "nocorrect cod", "nocorrect"),
            (ShellKind::Fish, "! cod", "!"),
            (ShellKind::Fish, "sudo not cod", "not"),
            (ShellKind::Zsh, "sudo nocorrect cod", "nocorrect"),
        ] {
            assert_eq!(context(shell, text).command(), Some(expected), "{text:?}");
        }
    }
}
