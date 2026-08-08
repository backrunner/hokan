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
        at_command_position(context)
            || crate::providers::shell_symbol_argument_position(context)
            || self.function_arg_slot(context).is_some()
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        // Argument slots win: `proj ` is both "command position" by word
        // count and the function's first-argument slot.
        if let Some((function_name, slot)) = self.function_arg_slot(context) {
            return self.complete_argument(context, &function_name, &slot);
        }
        if at_command_position(context) || crate::providers::shell_symbol_argument_position(context)
        {
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
        let query = context.parsed.current_prefix.as_str();
        let folded_query = query.to_lowercase();
        let has_direct_prefix = !folded_query.is_empty()
            && aliases
                .names()
                .any(|name| name.to_lowercase().starts_with(&folded_query));
        let candidates = aliases
            .names()
            .filter(|name| {
                if has_direct_prefix {
                    name.to_lowercase().starts_with(&folded_query)
                } else {
                    crate::completion::match_quality(query, name) > 0
                }
            })
            .map(|name| {
                let entry = aliases.get(name).expect("name from the same map");
                let querying = crate::providers::shell_symbol_argument_position(context);
                let replacement = name.to_owned();
                let resulting = crate::parser::apply_edit(
                    &context.buffer.text,
                    context.parsed.replacement.clone(),
                    &replacement,
                )
                .unwrap_or_else(|_| replacement.clone());
                let description = match (&entry.kind, &entry.expansion) {
                    (AliasKind::Alias, Some(expansion)) => truncate(expansion, 60),
                    (AliasKind::Abbreviation, Some(expansion)) => {
                        format!("缩写展开: {}", truncate(expansion, 52))
                    }
                    (AliasKind::Function, _) => "rc 文件中定义的 shell 函数".to_owned(),
                    _ => "rc 文件中定义的别名".to_owned(),
                };
                let risk = if querying {
                    crate::safety::classify_command(&resulting).level
                } else {
                    entry
                        .expansion
                        .as_deref()
                        .map_or(RiskLevel::Unknown, |expansion| {
                            crate::safety::classify_command(expansion).level
                        })
                };
                let mut candidate = Candidate::new(
                    context.query_id,
                    if querying { resulting } else { name.to_owned() },
                    description,
                    Some(TextEdit {
                        range: context.parsed.replacement.clone(),
                        replacement,
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
        if !crate::providers::effective_command_is_shell_command(context) {
            return None;
        }
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
        let fixed_base = slot.base.as_ref().map(|base| {
            if base.is_absolute() {
                base.clone()
            } else {
                context.cwd.join(base)
            }
        });
        let base = fixed_base
            .clone()
            .unwrap_or_else(|| context.cwd.as_ref().clone());
        let prefix = context.parsed.current_prefix.as_str();
        let (directory_prefix, basename) = super::filesystem::split_prefix(prefix);
        let scan_directory = fixed_base.as_ref().map_or_else(
            || super::filesystem::scan_directory_for(&base, directory_prefix),
            |base| base.join(directory_prefix.trim_start_matches('/')),
        );
        let Ok(entries) = std::fs::read_dir(&scan_directory) else {
            return ProviderOutput::default();
        };
        let noun = match slot.kind {
            SlotKind::Directory | SlotKind::NewFile => "目录",
            SlotKind::File => "文件",
            _ => "条目",
        };
        let whereabouts = fixed_base
            .as_ref()
            .map_or_else(|| "当前目录".to_owned(), |base| base.display().to_string());
        let mut names: Vec<(String, bool)> = Vec::new();
        for entry in entries.flatten().take(1_000) {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !basename.starts_with('.') && name.starts_with('.') {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let metadata = entry.metadata().ok();
            if metadata.is_none() && !file_type.is_symlink() {
                continue;
            }
            let is_dir =
                metadata.as_ref().is_some_and(std::fs::Metadata::is_dir) || file_type.is_dir();
            let accepted = match slot.kind {
                SlotKind::Directory | SlotKind::NewFile => is_dir,
                SlotKind::File | SlotKind::Path => true,
                SlotKind::Executable => {
                    is_dir
                        || metadata
                            .is_some_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
                }
                SlotKind::Process | SlotKind::Interface | SlotKind::Port | SlotKind::Value => false,
            };
            if accepted {
                names.push((name, is_dir));
            }
        }
        names.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        names.truncate(500);
        let candidates = names
            .into_iter()
            .map(|(name, is_dir)| {
                let mut logical = format!("{directory_prefix}{name}");
                let traversable = is_dir && slot.kind != SlotKind::Directory;
                if traversable {
                    logical.push('/');
                }
                let replacement = if fixed_base.is_none() {
                    super::filesystem::escape_path_for_shell(&logical, context.shell)
                } else {
                    escape_for_shell(&logical, QuoteContext::Unquoted, context.shell)
                };
                Candidate::new(
                    context.query_id,
                    &logical,
                    format!("{whereabouts} 下的{noun}（{function_name} 参数）"),
                    Some(TextEdit {
                        range: context.parsed.replacement.clone(),
                        replacement,
                        cursor_after: CursorPlacement::End,
                    }),
                    if traversable {
                        CandidateAction::InsertAndContinue {
                            next_slot: SlotKind::Path,
                        }
                    } else {
                        CandidateAction::Insert
                    },
                    CandidateSource::Filesystem,
                    if is_dir {
                        CandidateKind::Directory
                    } else {
                        CandidateKind::File
                    },
                    if traversable {
                        Completeness::NeedsInput {
                            slot: SlotKind::Path,
                        }
                    } else {
                        Completeness::Runnable
                    },
                    RiskLevel::Low,
                    format!(
                        "fnarg:{function_name}:{}",
                        scan_directory.join(&name).display()
                    ),
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
    crate::providers::shell_command_position_open(context)
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
    use std::{path::PathBuf, sync::RwLock};

    use super::*;
    use crate::{
        completion::{BufferSnapshot, CompletionEngine, SyncQuality},
        history::{HistoryIndex, HistoryPolicy},
        platform::CommandPathCache,
        providers::{CommandHelpCache, FilesystemProvider, HistoryProvider},
        shell::{ShellAliases, ShellKind},
        specs::SpecRegistry,
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
        // External wrappers take executable names, not shell aliases.
        assert!(!provider.applies(&context("sudo l")));
        assert!(!provider.applies(&context("sudo ")));
        assert!(!provider.applies(&context("command l")));
        assert!(!provider.applies(&context("builtin l")));
        assert!(!provider.applies(&context("exec l")));
        assert!(!provider.applies(&context("builtin command l")));
        assert!(!provider.applies(&context("builtin exec l")));
        assert!(!provider.applies(&context("sudo type l")));
        assert!(!provider.applies(&context("sudo command -v l")));
        assert!(!provider.applies(&context("exec type l")));
        // Shell precommand modifiers keep a real shell command position.
        assert!(provider.applies(&context("time l")));
        assert!(!provider.applies(&context("/usr/bin/time l")));
        assert!(provider.applies(&context("noglob l")));
        assert!(provider.applies(&context("! l")));
        assert!(provider.applies(&context("type l")));
        assert!(provider.applies(&context("command type l")));
        assert!(provider.applies(&context("builtin type l")));
        assert!(provider.applies(&context("which l")));
        assert!(provider.applies(&context("command -v g")));
        assert!(provider.applies(&context("command -pv g")));
        assert!(provider.applies(&context("builtin command -v g")));
        assert!(provider.applies(&context("FOO=bar ")));
        // At an argument position (`ls `) or on a path prefix, it stays out.
        assert!(!provider.applies(&context("ls ")));
        assert!(!provider.applies(&context("sudo ls ")));
        assert!(!provider.applies(&context("./l")));
    }

    #[test]
    fn wrapped_shell_functions_do_not_offer_function_argument_rows() {
        let base = tempfile::tempdir().expect("base");
        std::fs::create_dir(base.path().join("app")).expect("function directory");
        let mut aliases = ShellAliases::default();
        crate::shell::parse_rc_text(
            ShellKind::Zsh,
            &format!("proj() {{ cd {}/$1; }}\n", base.path().display()),
            &mut aliases,
        );
        let provider = AliasProvider::fixed(aliases);
        assert!(
            provider
                .complete(&context("sudo proj "))
                .candidates
                .is_empty()
        );
        assert!(
            provider
                .complete(&context("time proj "))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "app")
        );
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

        let queried = provider.complete(&context("type g"));
        let gco = queried
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "type gco")
            .expect("queried alias");
        assert_eq!(gco.edit.as_ref().expect("edit").replacement, "gco");
        assert_eq!(gco.risk, RiskLevel::Low);

        let which = provider.complete(&context("which g"));
        assert!(
            which
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "which gco")
        );

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
    fn direct_alias_prefixes_suppress_scattered_fuzzy_rows() {
        let mut aliases = ShellAliases::default();
        crate::shell::parse_rc_text(
            ShellKind::Zsh,
            "alias code='true'\nalias codex='true'\nalias cargo-doc='true'\n",
            &mut aliases,
        );
        let provider = AliasProvider::fixed(aliases);
        let rows: Vec<_> = provider
            .complete(&context("cod"))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert_eq!(rows, ["code", "codex"]);

        let rows: Vec<_> = provider
            .complete(&context("code"))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert_eq!(rows, ["code", "codex"]);
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
    fn full_line_history_outranks_inferred_function_argument_entries() {
        let base = tempfile::tempdir().expect("base");
        std::fs::create_dir(base.path().join("aipass")).expect("aipass dir");
        std::fs::create_dir(base.path().join("skillscat")).expect("skillscat dir");

        let mut shell_aliases = ShellAliases::default();
        crate::shell::parse_rc_text(
            ShellKind::Zsh,
            &format!(
                "proj() {{\n  if [ -n \"$1\" ]; then\n    cd \"{}/$1\"\n  else\n    cd \"{}\"\n  fi\n}}\n",
                base.path().display(),
                base.path().display()
            ),
            &mut shell_aliases,
        );
        let aliases = Arc::new(AliasCache::new_fixed(shell_aliases));
        let policy = HistoryPolicy::new(1024, &[]).expect("history policy");
        let mut index = HistoryIndex::default();
        index.ingest(
            "proj skillscat",
            crate::history_now_ms(),
            ShellKind::Zsh,
            None,
            Some(0),
            &policy,
        );

        let mut engine = CompletionEngine::new(100, 12);
        engine.register(AliasProvider::new(Arc::clone(&aliases)));
        engine.register(HistoryProvider::new(
            Arc::new(RwLock::new(index)),
            Arc::new(CommandPathCache::default()),
            aliases,
            Arc::new(crate::specs::SpecRegistry::default()),
            Arc::new(CommandHelpCache::default()),
        ));

        for text in ["proj ", "proj s"] {
            let output = engine.complete(&context(text));
            let first = output.candidates.first().expect("top candidate");
            assert_eq!(
                first.display.primary,
                "proj skillscat",
                "history should be the first continuation for {text:?}: {:?}",
                output
                    .candidates
                    .iter()
                    .map(|candidate| candidate.display.primary.as_str())
                    .collect::<Vec<_>>()
            );
            assert_eq!(first.source, CandidateSource::History);
            assert_eq!(first.score.continuation_priority, 1);
        }
    }

    #[test]
    fn file_slot_functions_include_directories_for_path_traversal() {
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
        assert_eq!(names, ["readme.md", "subdir/"]);
        let directory = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "subdir/")
            .expect("directory traversal row");
        assert!(matches!(
            directory.action,
            CandidateAction::InsertAndContinue { .. }
        ));
    }

    #[test]
    fn function_argument_paths_descend_and_resolve_relative_bases_from_cwd() {
        let cwd = tempfile::tempdir().expect("cwd");
        let projects = cwd.path().join("projects");
        std::fs::create_dir_all(projects.join("team/api")).expect("nested project");
        std::fs::create_dir_all(projects.join("team/web")).expect("nested project");

        let mut aliases = ShellAliases::default();
        crate::shell::parse_rc_text(ShellKind::Zsh, "proj() { cd projects/$1; }\n", &mut aliases);
        let aliases = Arc::new(AliasCache::new_fixed(aliases));
        let context = CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            cwd.path().to_path_buf(),
            BufferSnapshot::new(
                "proj team/a",
                "proj team/a".len(),
                BufferRevision::new(1),
                SyncQuality::Exact,
            )
            .expect("buffer"),
        )
        .expect("context");

        let mut engine = CompletionEngine::new(100, 12);
        engine.register(AliasProvider::new(Arc::clone(&aliases)));
        engine.register(FilesystemProvider::new(
            false,
            Arc::new(SpecRegistry::default()),
            Arc::new(CommandHelpCache::default()),
            aliases,
        ));
        let output = engine.complete(&context);
        assert_eq!(output.candidates.len(), 1);
        let api = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "team/api")
            .expect("nested api row");
        assert_eq!(api.edit.as_ref().expect("edit").replacement, "team/api");
        assert!(
            api.display
                .description
                .contains(&projects.display().to_string())
        );
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
