use std::{fs, os::unix::fs::PermissionsExt, time::Instant};

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderDiagnostic, ProviderOutput,
        SlotKind, TextEdit,
    },
    parser::escape_for_shell,
    terminal::RiskLevel,
};

const MAX_DIRECTORY_ENTRIES: usize = 5_000;
const DIRECTORY_BUDGET_MS: u128 = 80;

pub struct FilesystemProvider {
    show_hidden: bool,
}

impl FilesystemProvider {
    #[must_use]
    pub const fn new(show_hidden: bool) -> Self {
        Self { show_hidden }
    }
}

impl CandidateProvider for FilesystemProvider {
    fn id(&self) -> &'static str {
        "filesystem"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        infer_slot(context).is_some()
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let Some(slot) = infer_slot(context) else {
            return ProviderOutput::default();
        };
        let prefix = context.parsed.current_prefix.as_str();
        let (directory_prefix, basename) = split_prefix(prefix);
        let scan_directory = if directory_prefix.is_empty() {
            context.cwd.as_ref().clone()
        } else {
            context.cwd.join(directory_prefix)
        };
        let started = Instant::now();
        let entries = match fs::read_dir(&scan_directory) {
            Ok(entries) => entries,
            Err(error) => {
                return ProviderOutput {
                    candidates: Vec::new(),
                    diagnostics: vec![ProviderDiagnostic {
                        provider: self.id(),
                        code: "HK-FS-001",
                        message: format!("cannot read {}: {error}", scan_directory.display()),
                    }],
                };
            }
        };
        let show_hidden = self.show_hidden || basename.starts_with('.');
        let mut candidates = Vec::new();
        let mut partial = false;
        for (position, entry) in entries.enumerate() {
            if position >= MAX_DIRECTORY_ENTRIES {
                partial = true;
                break;
            }
            if started.elapsed().as_millis() > DIRECTORY_BUDGET_MS {
                partial = true;
                break;
            }
            let Ok(entry) = entry else {
                continue;
            };
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if (!show_hidden && name.starts_with('.')) || name.contains(['\0', '\n', '\r']) {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let directory = metadata.is_dir();
            if !slot_accepts(slot, &name, directory, &metadata) {
                continue;
            }
            let mut logical = format!("{directory_prefix}{name}");
            if directory {
                logical.push('/');
            } else if directory_prefix.is_empty() && logical.starts_with('-') {
                logical.insert_str(0, "./");
            }
            // The edit replaces the complete raw token, including any open quote.
            let replacement = escape_for_shell(
                &logical,
                crate::parser::QuoteContext::Unquoted,
                context.shell,
            );
            if replacement.is_empty() {
                continue;
            }
            let executable = metadata.permissions().mode() & 0o111 != 0;
            let mut candidate = Candidate::new(
                context.query_id,
                &logical,
                if directory {
                    "进入目录继续补全"
                } else if matches!(slot, SlotKind::Executable) {
                    "当前目录中的可执行文件或 shell 脚本"
                } else {
                    "当前目录中的文件"
                },
                Some(TextEdit {
                    range: context.parsed.replacement.clone(),
                    replacement,
                    cursor_after: CursorPlacement::End,
                }),
                if directory {
                    CandidateAction::InsertAndContinue {
                        next_slot: SlotKind::Path,
                    }
                } else {
                    CandidateAction::Insert
                },
                CandidateSource::Filesystem,
                if directory {
                    CandidateKind::Directory
                } else {
                    CandidateKind::File
                },
                if directory {
                    Completeness::NeedsInput {
                        slot: SlotKind::Path,
                    }
                } else {
                    Completeness::Runnable
                },
                RiskLevel::Low,
                format!("fs:{}", entry.path().display()),
            );
            candidate.score.cwd_affinity = 100;
            candidate.score.spec_priority =
                match (slot, directory, executable, name.ends_with(".sh")) {
                    (SlotKind::Executable, false, true, _) => 120,
                    (SlotKind::Executable, false, false, true) => 100,
                    (SlotKind::Executable, true, _, _) => 60,
                    (_, true, _, _) => 50,
                    _ => 80,
                };
            candidates.push(candidate);
        }
        ProviderOutput {
            candidates,
            diagnostics: partial
                .then_some(ProviderDiagnostic {
                    provider: self.id(),
                    code: "HK-FS-002",
                    message: format!(
                        "partial directory results for {} (80 ms / 5000 entry budget)",
                        scan_directory.display()
                    ),
                })
                .into_iter()
                .collect(),
        }
    }
}

fn infer_slot(context: &CompletionContext) -> Option<SlotKind> {
    let command = context.command()?;
    let command_token = context.parsed.tokens.iter().find(|token| {
        token.kind == crate::parser::TokenKind::Word
            && token.range.start >= context.parsed.active_segment.start
    })?;
    if context.buffer.cursor <= command_token.range.end
        && !context.buffer.text[..context.buffer.cursor].ends_with(char::is_whitespace)
    {
        return None;
    }
    let words: Vec<_> = context
        .parsed
        .tokens
        .iter()
        .filter(|token| {
            token.kind == crate::parser::TokenKind::Word
                && token.range.start >= context.parsed.active_segment.start
                && token.range.start <= context.buffer.cursor
        })
        .map(|token| token.cooked_prefix.as_str())
        .collect();
    let trailing_space = context.buffer.text[..context.buffer.cursor]
        .chars()
        .next_back()
        .is_some_and(char::is_whitespace);
    let argument_position = if trailing_space {
        words.len().saturating_sub(1)
    } else {
        words.len().saturating_sub(2)
    };
    match command {
        "cd" => Some(SlotKind::Directory),
        "bash" | "zsh" | "sh" => Some(SlotKind::Executable),
        "df" => Some(SlotKind::Path),
        "tar" => tar_slot(&words, argument_position),
        "lsof" if words.contains(&"+D") => Some(SlotKind::Directory),
        "lsof" => None,
        "kill" | "ifconfig" | "ip" | "ps" => None,
        _ => Some(SlotKind::Path),
    }
}

fn tar_slot(words: &[&str], argument_position: usize) -> Option<SlotKind> {
    let operation = words.get(1).copied().unwrap_or_default();
    if argument_position == 0 || !operation.starts_with('-') {
        return None;
    }
    if argument_position == 1 {
        return if operation.contains('c') {
            Some(SlotKind::NewFile)
        } else if operation.contains('x') || operation.contains('t') {
            Some(SlotKind::File)
        } else {
            None
        };
    }
    operation.contains('c').then_some(SlotKind::Path)
}

fn split_prefix(prefix: &str) -> (&str, &str) {
    prefix
        .rfind('/')
        .map_or(("", prefix), |index| prefix.split_at(index + 1))
}

fn slot_accepts(slot: SlotKind, name: &str, directory: bool, metadata: &fs::Metadata) -> bool {
    match slot {
        SlotKind::Directory => directory,
        SlotKind::Executable => {
            directory || metadata.permissions().mode() & 0o111 != 0 || name.ends_with(".sh")
        }
        SlotKind::File => !directory,
        SlotKind::Path => true,
        SlotKind::NewFile => directory,
        SlotKind::Process | SlotKind::Interface | SlotKind::Port | SlotKind::Value => false,
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Write, path::PathBuf, time::Duration};

    use super::*;
    use crate::{
        completion::{BufferSnapshot, CompletionEngine, SyncQuality},
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };

    #[test]
    fn bash_prefers_scripts_and_escapes_spaces() {
        let directory = tempfile::tempdir().expect("directory");
        fs::File::create(directory.path().join("hello world.sh"))
            .expect("script")
            .write_all(b"echo ok\n")
            .expect("write script");
        fs::create_dir(directory.path().join("nested")).expect("nested directory");
        let context = CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from(directory.path()),
            BufferSnapshot::new("bash ", 5, BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer"),
        )
        .expect("context");
        let mut engine = CompletionEngine::new(100, 20);
        engine.register(FilesystemProvider::new(false));
        let output = engine.complete(&context);
        assert!(output.candidates.iter().any(|candidate| {
            candidate.display.primary == "hello world.sh"
                && candidate
                    .edit
                    .as_ref()
                    .is_some_and(|edit| edit.replacement == "'hello world.sh'")
        }));
        assert!(
            output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "nested/")
        );

        let quoted = CompletionContext::new(
            QueryId::new(2),
            ShellKind::Zsh,
            PathBuf::from(directory.path()),
            BufferSnapshot::new(
                "bash 'hello",
                11,
                BufferRevision::new(2),
                SyncQuality::Exact,
            )
            .expect("quoted buffer"),
        )
        .expect("quoted context");
        let output = engine.complete(&quoted);
        let candidate = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "hello world.sh")
            .expect("quoted script candidate");
        assert_eq!(candidate.edit.as_ref().expect("edit").range, 5..11);
        assert_eq!(
            candidate.edit.as_ref().expect("edit").replacement,
            "'hello world.sh'"
        );
    }

    #[test]
    fn tar_slots_distinguish_new_and_existing_archives() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("archive.tgz"), b"archive").expect("archive");
        fs::create_dir(directory.path().join("src")).expect("source directory");
        let provider = FilesystemProvider::new(false);

        let create = context(directory.path(), "tar -czf ", 1);
        let create_output = provider.complete(&create);
        assert!(
            create_output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "src/")
        );
        assert!(
            !create_output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "archive.tgz")
        );

        let extract = context(directory.path(), "tar -xzf ", 2);
        let extract_output = provider.complete(&extract);
        assert!(
            extract_output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "archive.tgz")
        );
    }

    #[test]
    fn five_thousand_entry_directory_returns_within_the_first_batch_budget() {
        let directory = tempfile::tempdir().expect("directory");
        for index in 0..5_000 {
            fs::write(directory.path().join(format!("entry-{index:04}")), b"")
                .expect("directory fixture");
        }
        let context = context(directory.path(), "cat ", 3);
        let provider = FilesystemProvider::new(false);
        let mut samples = Vec::new();
        for _ in 0..10 {
            let started = Instant::now();
            let output = provider.complete(&context);
            samples.push(started.elapsed());
            assert!(!output.candidates.is_empty());
        }
        samples.sort_unstable();
        let p95 = samples[9];
        let budget = if cfg!(debug_assertions) {
            Duration::from_millis(120)
        } else {
            Duration::from_millis(85)
        };
        assert!(p95 <= budget, "5000-entry directory p95 was {p95:?}");
    }

    fn context(directory: &std::path::Path, text: &str, query: u64) -> CompletionContext {
        CompletionContext::new(
            QueryId::new(query),
            ShellKind::Zsh,
            directory.to_owned(),
            BufferSnapshot::new(
                text,
                text.len(),
                BufferRevision::new(query),
                SyncQuality::Exact,
            )
            .expect("buffer"),
        )
        .expect("context")
    }
}
