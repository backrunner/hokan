use std::sync::Arc;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderOutput, SlotKind, TextEdit,
    },
    parser::{QuoteContext, escape_for_shell},
    shell::{AliasCache, AliasKind, FunctionSlot},
    terminal::RiskLevel,
};

/// Command-position rows for aliases, functions, and abbreviations defined
/// in the user's rc files. The description is the expansion so `ll` shows
/// what it actually runs; alias rows outrank plain PATH rows so the user's
/// own definition wins the dedupe when names collide.
pub struct AliasProvider {
    source: AliasSource,
}

enum AliasSource {
    Cache(Arc<AliasCache>),
    #[cfg(test)]
    Fixed(crate::shell::ShellAliases),
}

impl AliasProvider {
    #[must_use]
    pub fn new(cache: Arc<AliasCache>) -> Self {
        Self {
            source: AliasSource::Cache(cache),
        }
    }

    #[cfg(test)]
    fn fixed(aliases: crate::shell::ShellAliases) -> Self {
        Self {
            source: AliasSource::Fixed(aliases),
        }
    }

    fn aliases(&self, shell: crate::shell::ShellKind) -> crate::shell::ShellAliases {
        match &self.source {
            AliasSource::Cache(cache) => cache.load(shell).as_ref().clone(),
            #[cfg(test)]
            AliasSource::Fixed(aliases) => aliases.clone(),
        }
    }
}

impl CandidateProvider for AliasProvider {
    fn id(&self) -> &'static str {
        "alias"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        at_command_position(context) || self.function_arg_slot(context).is_some()
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        // Argument slots win: `proj ` is both "command position" by word
        // count and the function's first-argument slot.
        if let Some((function_name, slot)) = self.function_arg_slot(context) {
            return self.complete_argument(context, &function_name, &slot);
        }
        if at_command_position(context) {
            return self.complete_commands(context);
        }
        ProviderOutput::default()
    }
}

impl AliasProvider {
    fn complete_commands(&self, context: &CompletionContext) -> ProviderOutput {
        let aliases = self.aliases(context.shell);
        if aliases.is_empty() {
            return ProviderOutput::default();
        }
        let candidates = aliases
            .names()
            .filter(|name| {
                crate::completion::match_quality(&context.parsed.current_prefix, name) > 0
            })
            .map(|name| {
                let entry = aliases.get(name).expect("name from the same map");
                let description = match (&entry.kind, &entry.expansion) {
                    (AliasKind::Alias, Some(expansion)) => truncate(expansion, 60),
                    (AliasKind::Abbreviation, Some(expansion)) => {
                        format!("缩写展开: {}", truncate(expansion, 52))
                    }
                    (AliasKind::Function, _) => "rc 文件中定义的 shell 函数".to_owned(),
                    _ => "rc 文件中定义的别名".to_owned(),
                };
                let risk = entry
                    .expansion
                    .as_deref()
                    .map_or(RiskLevel::Unknown, |expansion| {
                        crate::safety::classify_command(expansion).level
                    });
                let mut candidate = Candidate::new(
                    context.query_id,
                    name,
                    description,
                    Some(TextEdit {
                        range: context.parsed.replacement.clone(),
                        replacement: name.to_owned(),
                        cursor_after: CursorPlacement::End,
                    }),
                    CandidateAction::Insert,
                    CandidateSource::PathCommand,
                    CandidateKind::Command,
                    Completeness::Runnable,
                    risk,
                    format!("alias:{name}"),
                );
                // The user's own definition is more relevant than a generic
                // PATH row with the same name.
                candidate.score.spec_priority = 40;
                candidate
            })
            .collect();
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }

    /// First-argument slot inferred from a custom function's body
    /// (`proj() { cd ~/projects/$1 }` → directories under ~/projects).
    fn function_arg_slot(&self, context: &CompletionContext) -> Option<(String, FunctionSlot)> {
        let (_, position) = crate::providers::argument_progress(context)?;
        if position != 0 {
            return None;
        }
        let name = context.command()?;
        let aliases = self.aliases(context.shell);
        let entry = aliases.get(name)?;
        if entry.kind != AliasKind::Function {
            return None;
        }
        let body = entry.body.as_deref()?;
        crate::shell::infer_function_slot(context.shell, body).map(|slot| (name.to_owned(), slot))
    }

    fn complete_argument(
        &self,
        context: &CompletionContext,
        function_name: &str,
        slot: &FunctionSlot,
    ) -> ProviderOutput {
        let directory = slot
            .base
            .clone()
            .unwrap_or_else(|| context.cwd.as_ref().clone());
        let Ok(entries) = std::fs::read_dir(&directory) else {
            return ProviderOutput::default();
        };
        let prefix = context.parsed.current_prefix.as_str();
        let noun = match slot.kind {
            SlotKind::Directory => "目录",
            SlotKind::File => "文件",
            _ => "条目",
        };
        let whereabouts = slot
            .base
            .as_ref()
            .map_or_else(|| "当前目录".to_owned(), |base| base.display().to_string());
        let mut names: Vec<String> = Vec::new();
        for entry in entries.flatten().take(1_000) {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !prefix.starts_with('.') && name.starts_with('.') {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let is_dir = metadata.is_dir();
            let accepted = match slot.kind {
                SlotKind::Directory => is_dir,
                SlotKind::File => !is_dir,
                SlotKind::Executable => {
                    is_dir || metadata.permissions().mode() & 0o111 != 0 || name.ends_with(".sh")
                }
                _ => true,
            };
            if accepted {
                names.push(name);
            }
        }
        names.sort_unstable();
        names.truncate(500);
        let candidates = names
            .into_iter()
            .map(|name| {
                let is_dir = directory.join(&name).is_dir();
                Candidate::new(
                    context.query_id,
                    &name,
                    format!("{whereabouts} 下的{noun}（{function_name} 参数）"),
                    Some(TextEdit {
                        range: context.parsed.replacement.clone(),
                        replacement: escape_for_shell(&name, QuoteContext::Unquoted, context.shell),
                        cursor_after: CursorPlacement::End,
                    }),
                    CandidateAction::Insert,
                    CandidateSource::Filesystem,
                    if is_dir {
                        CandidateKind::Directory
                    } else {
                        CandidateKind::File
                    },
                    Completeness::Runnable,
                    RiskLevel::Low,
                    format!("fnarg:{function_name}:{name}"),
                )
            })
            .collect();
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }
}

/// Same command-word position the path-command provider uses.
fn at_command_position(context: &CompletionContext) -> bool {
    crate::providers::command_position_open(context)
}

fn truncate(value: &str, max_chars: usize) -> String {
    let sanitized: String = value
        .chars()
        .filter(|character| !character.is_control())
        .take(max_chars)
        .collect();
    if value.chars().count() > max_chars {
        format!("{sanitized}...")
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{
        completion::{BufferSnapshot, CompletionEngine, SyncQuality},
        shell::{ShellAliases, ShellKind},
        terminal::{BufferRevision, QueryId},
    };

    fn context(text: &str) -> CompletionContext {
        CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from("/tmp"),
            BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer"),
        )
        .expect("context")
    }

    fn test_aliases() -> ShellAliases {
        let mut aliases = ShellAliases::default();
        crate::shell::parse_rc_text(
            ShellKind::Zsh,
            "alias ll='ls -lah'\nalias gco='git checkout'\nmkcd() { true; }\n",
            &mut aliases,
        );
        aliases
    }

    #[test]
    fn fires_only_at_the_command_position() {
        let provider = AliasProvider::fixed(test_aliases());
        assert!(provider.applies(&context("l")));
        assert!(provider.applies(&context("")));
        // After a wrapper the command word is still open: `sudo l` completes
        // the command name, as does the bare `sudo ` slot.
        assert!(provider.applies(&context("sudo l")));
        assert!(provider.applies(&context("sudo ")));
        assert!(provider.applies(&context("FOO=bar ")));
        // At an argument position (`ls `) or on a path prefix, it stays out.
        assert!(!provider.applies(&context("ls ")));
        assert!(!provider.applies(&context("sudo ls ")));
        assert!(!provider.applies(&context("./l")));
    }

    #[test]
    fn offers_alias_rows_with_expansion_descriptions() {
        let provider = AliasProvider::fixed(test_aliases());
        let output = provider.complete(&context("l"));
        assert_eq!(output.candidates.len(), 1);
        let row = &output.candidates[0];
        assert_eq!(row.display.primary, "ll");
        assert_eq!(row.display.description, "ls -lah");
        assert_eq!(row.edit.as_ref().expect("edit").replacement, "ll");
        assert!(matches!(row.completeness, Completeness::Runnable));
        assert_eq!(row.score.spec_priority, 40);

        let functions = provider.complete(&context("mk"));
        assert_eq!(functions.candidates[0].display.primary, "mkcd");
        assert_eq!(
            functions.candidates[0].display.description,
            "rc 文件中定义的 shell 函数"
        );
    }

    #[test]
    fn engine_mixes_alias_rows_with_other_sources() {
        let mut engine = CompletionEngine::new(100, 12);
        engine.register(AliasProvider::fixed(test_aliases()));
        let output = engine.complete(&context("gc"));
        assert_eq!(
            output
                .candidates
                .first()
                .map(|c| c.display.primary.as_str()),
            Some("gco")
        );
    }

    #[test]
    fn function_argument_completes_entries_under_the_inferred_base() {
        let base = tempfile::tempdir().expect("base");
        std::fs::create_dir(base.path().join("api")).expect("api dir");
        std::fs::create_dir(base.path().join("web")).expect("web dir");
        std::fs::write(base.path().join("notes.txt"), b"n").expect("file");

        let mut aliases = ShellAliases::default();
        crate::shell::parse_rc_text(
            ShellKind::Zsh,
            &format!("proj() {{ cd {}/$1; }}\n", base.path().display()),
            &mut aliases,
        );
        let provider = AliasProvider::fixed(aliases);

        let context = CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from("/tmp"),
            BufferSnapshot::new("proj ", 5, BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer"),
        )
        .expect("context");
        assert!(provider.applies(&context));
        let output = provider.complete(&context);
        let names: Vec<_> = output
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect();
        assert_eq!(names, ["api", "web"], "directory slot rows: {names:?}");
        let row = &output.candidates[0];
        assert!(row.display.description.contains("proj 参数"));
        assert_eq!(
            row.edit.as_ref().expect("edit").replacement,
            "api",
            "the fill is the bare entry name — the function joins the base"
        );
    }

    #[test]
    fn file_slot_functions_complete_files_not_directories() {
        let base = tempfile::tempdir().expect("base");
        std::fs::create_dir(base.path().join("subdir")).expect("dir");
        std::fs::write(base.path().join("readme.md"), b"r").expect("file");

        let mut aliases = ShellAliases::default();
        crate::shell::parse_rc_text(
            ShellKind::Zsh,
            &format!("edit() {{ vim {}/$1; }}\n", base.path().display()),
            &mut aliases,
        );
        let provider = AliasProvider::fixed(aliases);
        let context = CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from("/tmp"),
            BufferSnapshot::new("edit ", 5, BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer"),
        )
        .expect("context");
        let output = provider.complete(&context);
        let names: Vec<_> = output
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect();
        assert_eq!(names, ["readme.md"]);
    }

    #[test]
    fn second_argument_does_not_fire() {
        let base = tempfile::tempdir().expect("base");
        let mut aliases = ShellAliases::default();
        crate::shell::parse_rc_text(
            ShellKind::Zsh,
            &format!("proj() {{ cd {}/$1; }}\n", base.path().display()),
            &mut aliases,
        );
        let provider = AliasProvider::fixed(aliases);
        let context = CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from("/tmp"),
            BufferSnapshot::new(
                "proj api extra",
                14,
                BufferRevision::new(1),
                SyncQuality::Exact,
            )
            .expect("buffer"),
        )
        .expect("context");
        assert!(provider.complete(&context).candidates.is_empty());
    }
}
