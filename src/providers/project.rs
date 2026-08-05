use std::sync::{Arc, RwLock};

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderDiagnostic, ProviderOutput,
        SlotKind, TextEdit,
    },
    history::HistoryIndex,
    parser::{QuoteContext, escape_for_shell},
    platform::CommandPathCache,
    project::{
        MakefileCache, ManifestKind, NodeWorkspaceCache, ProjectCache, WorkspaceMember,
        discover_makefile,
    },
    terminal::RiskLevel,
};

pub struct ProjectProvider {
    cache: Arc<ProjectCache>,
    makefiles: MakefileCache,
    commands: Arc<CommandPathCache>,
    history: Arc<RwLock<HistoryIndex>>,
    workspaces: NodeWorkspaceCache,
}

impl ProjectProvider {
    #[must_use]
    pub fn new(
        cache: Arc<ProjectCache>,
        commands: Arc<CommandPathCache>,
        history: Arc<RwLock<HistoryIndex>>,
    ) -> Self {
        Self {
            cache,
            makefiles: MakefileCache::default(),
            commands,
            history,
            workspaces: NodeWorkspaceCache::default(),
        }
    }
}

impl CandidateProvider for ProjectProvider {
    fn id(&self) -> &'static str {
        "project"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        // No PATH gate for package managers: hokan starts before the user's
        // rc files initialize nvm/volta/corepack, so pnpm & co. are often
        // missing from the startup PATH cache while working fine in the
        // child shell. The manifest itself is the ground truth.
        if filter_position(context).is_some() || manager_position(context).is_some() {
            return true;
        }
        rule_file_tool(context).is_some_and(|tool| {
            self.commands.contains(tool)
                && discover_makefile(&context.cwd, ManifestKind::for_tool(tool)).is_some()
        })
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        if let Some(position) = filter_position(context) {
            return self.complete_filtered(context, position);
        }
        if let Some((spec, position)) = manager_position(context) {
            return self.complete_scripts(context, spec, position);
        }
        if rule_file_tool(context).is_some() {
            return self.complete_targets(context);
        }
        ProviderOutput::default()
    }
}

impl ProjectProvider {
    fn complete_scripts(
        &self,
        context: &CompletionContext,
        spec: &ManagerSpec,
        position: Position,
    ) -> ProviderOutput {
        // npm and deno lead with their own subcommands at the bare position;
        // scripts only appear after `run` / `task`.
        if matches!(position, Position::Bare { .. }) && spec.keyword.is_some() {
            return self.complete_subcommands(context, spec, position);
        }
        let Some(manifest) = (match self.cache.load_nearest(&context.cwd) {
            Ok(manifest) => manifest,
            Err(error) => {
                return ProviderOutput {
                    candidates: Vec::new(),
                    diagnostics: vec![ProviderDiagnostic {
                        provider: self.id(),
                        code: "HK-PROJ-001",
                        message: error.to_string(),
                    }],
                };
            }
        }) else {
            // deno.json tasks still apply for deno even without package.json.
            return self.complete_deno_tasks(context, spec, position, None);
        };
        let relative = manifest
            .path
            .strip_prefix(context.cwd.as_ref())
            .unwrap_or(&manifest.path)
            .display()
            .to_string();
        let now_ms = crate::history_now_ms();
        let mut candidates: Vec<_> = manifest
            .scripts
            .iter()
            .map(|(name, script)| {
                self.script_candidate(context, spec, position, name, script, &relative, now_ms)
            })
            .collect();
        if spec.name == "deno" {
            let mut deno = self.complete_deno_tasks(context, spec, position, Some(now_ms));
            candidates.append(&mut deno.candidates);
        }
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }

    /// deno.json(c) `tasks` for deno, in the same `deno task <name>` form.
    fn complete_deno_tasks(
        &self,
        context: &CompletionContext,
        spec: &ManagerSpec,
        position: Position,
        now_ms: Option<i64>,
    ) -> ProviderOutput {
        if spec.name != "deno" {
            return ProviderOutput::default();
        }
        let Ok(Some(manifest)) = self.cache.load_deno_nearest(&context.cwd) else {
            return ProviderOutput::default();
        };
        let relative = manifest
            .path
            .strip_prefix(context.cwd.as_ref())
            .unwrap_or(&manifest.path)
            .display()
            .to_string();
        let now_ms = now_ms.unwrap_or_else(crate::history_now_ms);
        ProviderOutput {
            candidates: manifest
                .tasks
                .iter()
                .map(|(name, command)| {
                    self.script_candidate(context, spec, position, name, command, &relative, now_ms)
                })
                .collect(),
            diagnostics: Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn script_candidate(
        &self,
        context: &CompletionContext,
        spec: &ManagerSpec,
        position: Position,
        name: &str,
        command: &str,
        origin: &str,
        now_ms: i64,
    ) -> Candidate {
        let escaped = escape_for_shell(name, QuoteContext::Unquoted, context.shell);
        let keyword = spec.keyword.unwrap_or("run");
        // Native form: pnpm/yarn/bun run scripts directly, npm needs `run`,
        // deno needs `task`. The keyword positions keep the keyword in place.
        let display = match spec.keyword {
            None => format!("{} {name}", spec.name),
            Some(keyword) => format!("{} {keyword} {name}", spec.name),
        };
        let replacement = match position {
            Position::ScriptToken => escaped.clone(),
            Position::KeywordWord => format!("{keyword} {escaped}"),
            Position::Bare {
                on_manager_word: false,
            } => match spec.keyword {
                None => escaped.clone(),
                Some(keyword) => format!("{keyword} {escaped}"),
            },
            Position::Bare {
                on_manager_word: true,
            } => match spec.keyword {
                None => format!("{} {escaped}", spec.name),
                Some(keyword) => format!("{} {keyword} {escaped}", spec.name),
            },
        };
        let mut candidate = Candidate::new(
            context.query_id,
            &display,
            truncate(command, 100),
            Some(TextEdit {
                range: context.parsed.replacement.clone(),
                replacement,
                cursor_after: CursorPlacement::End,
            }),
            CandidateAction::Insert,
            CandidateSource::Project,
            CandidateKind::ProjectScript,
            Completeness::Runnable,
            crate::safety::classify_command(command).level,
            format!("project:{origin}:{name}"),
        );
        candidate.display.annotation = Some(origin.to_owned());
        candidate.score.cwd_affinity = 100;
        // Scripts the user actually runs win: order by recorded usage of the
        // exact command line (e.g. `pnpm dev` × 40 above `pnpm build` × 2).
        if let Ok(index) = self.history.read() {
            candidate.score.frecency = index.usage_frecency(&display, now_ms);
        }
        candidate
    }

    /// Bare-position rows for npm/deno: their own subcommands first
    /// (`npm install`, `npm run`, `deno task`, …), scripts come later.
    fn complete_subcommands(
        &self,
        context: &CompletionContext,
        spec: &ManagerSpec,
        position: Position,
    ) -> ProviderOutput {
        let now_ms = crate::history_now_ms();
        let candidates = spec
            .subcommands
            .iter()
            .map(|(name, description)| {
                let display = format!("{} {name}", spec.name);
                let replacement = match position {
                    Position::Bare {
                        on_manager_word: true,
                    } => display.clone(),
                    _ => (*name).to_owned(),
                };
                let mut candidate = Candidate::new(
                    context.query_id,
                    &display,
                    *description,
                    Some(TextEdit {
                        range: context.parsed.replacement.clone(),
                        replacement,
                        cursor_after: CursorPlacement::End,
                    }),
                    CandidateAction::InsertAndContinue {
                        next_slot: SlotKind::Value,
                    },
                    CandidateSource::Project,
                    CandidateKind::Command,
                    Completeness::Runnable,
                    RiskLevel::Low,
                    format!("project:{}:{name}", spec.name),
                );
                if let Ok(index) = self.history.read() {
                    candidate.score.frecency = index.usage_frecency(&display, now_ms);
                }
                candidate
            })
            .collect();
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }

    /// `pnpm --filter` completions: member names at the value position, the
    /// member's own scripts after it.
    fn complete_filtered(
        &self,
        context: &CompletionContext,
        position: FilterPosition,
    ) -> ProviderOutput {
        let Some(workspace) = self.workspaces.load(&context.cwd) else {
            return ProviderOutput::default();
        };
        match position {
            FilterPosition::Value => {
                let relative_root = workspace
                    .root
                    .strip_prefix(context.cwd.as_ref())
                    .unwrap_or(&workspace.root)
                    .display()
                    .to_string();
                let candidates = workspace
                    .members
                    .iter()
                    .map(|member| {
                        let mut candidate = Candidate::new(
                            context.query_id,
                            &member.name,
                            format!(
                                "workspace 成员（{}）",
                                member
                                    .directory
                                    .strip_prefix(&workspace.root)
                                    .unwrap_or(&member.directory)
                                    .display()
                            ),
                            Some(TextEdit {
                                range: context.parsed.replacement.clone(),
                                replacement: member.name.clone(),
                                cursor_after: CursorPlacement::End,
                            }),
                            CandidateAction::InsertAndContinue {
                                next_slot: SlotKind::Value,
                            },
                            CandidateSource::Project,
                            CandidateKind::Command,
                            Completeness::Runnable,
                            RiskLevel::Low,
                            format!("project:{relative_root}:filter:{}", member.name),
                        );
                        candidate.display.annotation = Some(relative_root.clone());
                        candidate.score.cwd_affinity = 100;
                        candidate
                    })
                    .collect();
                ProviderOutput {
                    candidates,
                    diagnostics: Vec::new(),
                }
            }
            FilterPosition::MemberScripts { member, keyword } => {
                let Some(member) = workspace.members.iter().find(|m| m.name == member) else {
                    return ProviderOutput::default();
                };
                let position = if keyword {
                    Position::KeywordWord
                } else {
                    Position::ScriptToken
                };
                let now_ms = crate::history_now_ms();
                let relative = workspace
                    .root
                    .strip_prefix(context.cwd.as_ref())
                    .unwrap_or(&workspace.root)
                    .display()
                    .to_string();
                let candidates = member
                    .scripts
                    .iter()
                    .map(|(name, script)| {
                        self.member_script_candidate(
                            context, member, name, script, &relative, now_ms, position,
                        )
                    })
                    .collect();
                ProviderOutput {
                    candidates,
                    diagnostics: Vec::new(),
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn member_script_candidate(
        &self,
        context: &CompletionContext,
        member: &WorkspaceMember,
        name: &str,
        script: &str,
        origin: &str,
        now_ms: i64,
        position: Position,
    ) -> Candidate {
        let escaped = escape_for_shell(name, QuoteContext::Unquoted, context.shell);
        let display = format!("pnpm --filter {} {name}", member.name);
        let replacement = match position {
            Position::KeywordWord => format!("run {escaped}"),
            _ => escaped.clone(),
        };
        let mut candidate = Candidate::new(
            context.query_id,
            &display,
            truncate(script, 100),
            Some(TextEdit {
                range: context.parsed.replacement.clone(),
                replacement,
                cursor_after: CursorPlacement::End,
            }),
            CandidateAction::Insert,
            CandidateSource::Project,
            CandidateKind::ProjectScript,
            Completeness::Runnable,
            crate::safety::classify_command(script).level,
            format!("project:{origin}:filter:{}:{name}", member.name),
        );
        candidate.display.annotation = Some(origin.to_owned());
        candidate.score.cwd_affinity = 100;
        if let Ok(index) = self.history.read() {
            candidate.score.frecency = index.usage_frecency(&display, now_ms);
        }
        candidate
    }

    /// `make <target>` / `just <target>` rows from the nearest rule file. The
    /// description is the target's doc comment (the `# …` line directly above
    /// the rule) when present; targets carry no shell text, so they stay
    /// `RiskLevel::Low` and `Runnable`.
    fn complete_targets(&self, context: &CompletionContext) -> ProviderOutput {
        let Some(tool) = rule_file_tool(context) else {
            return ProviderOutput::default();
        };
        let manifest = match self
            .makefiles
            .load_nearest(&context.cwd, ManifestKind::for_tool(tool))
        {
            Ok(Some(manifest)) => manifest,
            Ok(None) => return ProviderOutput::default(),
            Err(error) => {
                return ProviderOutput {
                    candidates: Vec::new(),
                    diagnostics: vec![ProviderDiagnostic {
                        provider: self.id(),
                        code: "HK-PROJ-002",
                        message: error.to_string(),
                    }],
                };
            }
        };
        let relative = manifest
            .path
            .strip_prefix(context.cwd.as_ref())
            .unwrap_or(&manifest.path)
            .display()
            .to_string();
        let candidates = manifest
            .targets
            .iter()
            .map(|target| {
                let escaped = escape_for_shell(&target.name, QuoteContext::Unquoted, context.shell);
                let mut candidate = Candidate::new(
                    context.query_id,
                    format!("{tool} {}", target.name),
                    target
                        .doc
                        .as_deref()
                        .map_or_else(String::new, |doc| truncate(doc, 100)),
                    Some(TextEdit {
                        range: context.parsed.replacement.clone(),
                        replacement: escaped,
                        cursor_after: CursorPlacement::End,
                    }),
                    CandidateAction::Insert,
                    CandidateSource::Project,
                    CandidateKind::ProjectScript,
                    Completeness::Runnable,
                    RiskLevel::Low,
                    format!("project:{relative}:{}", target.name),
                );
                candidate.display.annotation = Some(relative.clone());
                candidate.score.cwd_affinity = 100;
                candidate
            })
            .collect();
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }
}

/// Node package-manager positions (`manager_position`, `filter_position`,
/// `ManagerSpec`, `Position`, `FilterPosition`, `segment_words`) live in
/// `crate::providers` so the filesystem provider can suppress its rows at the
/// same slots the project provider owns.
use super::{
    FilterPosition, ManagerSpec, Position, filter_position, manager_position, segment_words,
};

/// Matches the `make`/`just` first-argument position: the tool word alone
/// (`make `), one target word being typed (`make bu`), or attached-value flag
/// words before the target (`make -j4 bu`). Deeper words and flag positions
/// that take a separate value (`make -f <value>`) are left to other
/// providers.
fn rule_file_tool(context: &CompletionContext) -> Option<&'static str> {
    let words = segment_words(context);
    let tool = match words.first() {
        Some(&"make") => "make",
        Some(&"just") => "just",
        _ => return None,
    };
    // Attached-value flag words (`make -j4`) carry their own value, so the
    // word after them is still the target slot.
    let mut index = 1;
    while words
        .get(index)
        .is_some_and(|word| is_attached_value_flag(tool, word))
    {
        index += 1;
    }
    match &words[index..] {
        [] => Some(tool),
        [target] if !target.starts_with('-') => Some(tool),
        _ => None,
    }
}

/// Flags whose value is attached to the flag word itself (`make -j4`), as
/// opposed to a separate following word (`make -f Makefile`).
fn is_attached_value_flag(tool: &str, word: &str) -> bool {
    tool == "make"
        && word.len() > 2
        && word.starts_with("-j")
        && word[2..]
            .chars()
            .all(|character| character.is_ascii_digit())
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
    use std::{ffi::OsString, fs, os::unix::fs::PermissionsExt, path::PathBuf};

    use super::*;
    use crate::{
        completion::{BufferSnapshot, CompletionEngine, SyncQuality},
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };

    #[test]
    fn replaces_only_the_script_token() {
        let directory = tempfile::tempdir().expect("project");
        fs::write(
            directory.path().join("package.json"),
            r#"{"scripts":{"build docs":"vite build"}}"#,
        )
        .expect("manifest");
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).expect("bin");
        let pnpm = bin.join("pnpm");
        fs::write(&pnpm, b"#!/bin/sh\n").expect("fake pnpm");
        fs::set_permissions(&pnpm, fs::Permissions::from_mode(0o700)).expect("pnpm mode");
        let context = CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from(directory.path()),
            BufferSnapshot::new(
                "pnpm run bu",
                11,
                BufferRevision::new(1),
                SyncQuality::Exact,
            )
            .expect("buffer"),
        )
        .expect("context");
        let path = OsString::from(bin);
        let commands = Arc::new(CommandPathCache::from_path(Some(&path)));
        let mut engine = CompletionEngine::new(100, 12);
        engine.register(ProjectProvider::new(
            Arc::new(ProjectCache::default()),
            commands,
            Arc::new(RwLock::new(HistoryIndex::default())),
        ));
        let output = engine.complete(&context);
        assert_eq!(
            output.candidates[0].edit.as_ref().expect("edit").range,
            9..11
        );
        assert_eq!(
            output.candidates[0]
                .edit
                .as_ref()
                .expect("edit")
                .replacement,
            "'build docs'"
        );
    }

    fn bare_prefix_setup(buffer: &str) -> (tempfile::TempDir, CompletionContext, CompletionEngine) {
        let directory = tempfile::tempdir().expect("project");
        fs::write(
            directory.path().join("package.json"),
            r#"{"scripts":{"build docs":"vite build"}}"#,
        )
        .expect("manifest");
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).expect("bin");
        let pnpm = bin.join("pnpm");
        fs::write(&pnpm, b"#!/bin/sh\n").expect("fake pnpm");
        fs::set_permissions(&pnpm, fs::Permissions::from_mode(0o700)).expect("pnpm mode");
        let context = CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from(directory.path()),
            BufferSnapshot::new(
                buffer,
                buffer.len(),
                BufferRevision::new(1),
                SyncQuality::Exact,
            )
            .expect("buffer"),
        )
        .expect("context");
        let path = OsString::from(bin);
        let commands = Arc::new(CommandPathCache::from_path(Some(&path)));
        let mut engine = CompletionEngine::new(100, 12);
        engine.register(ProjectProvider::new(
            Arc::new(ProjectCache::default()),
            commands,
            Arc::new(RwLock::new(HistoryIndex::default())),
        ));
        (directory, context, engine)
    }

    #[test]
    fn bare_manager_prefix_uses_the_native_script_form() {
        // `pnpm bu` mixes package.json scripts into the list; pnpm executes
        // scripts directly, so the fill is the bare script name.
        let (_directory, context, engine) = bare_prefix_setup("pnpm bu");
        let output = engine.complete(&context);
        let candidate = output.candidates.first().expect("script candidate");
        assert_eq!(candidate.display.primary, "pnpm build docs");
        let edit = candidate.edit.as_ref().expect("edit");
        assert_eq!(edit.range, 5..7);
        assert_eq!(edit.replacement, "'build docs'");
    }

    #[test]
    fn npm_bare_prefix_leads_with_subcommands_not_scripts() {
        // npm cannot run scripts directly: the bare position leads with its
        // own subcommands; scripts wait behind `run`.
        let directory = tempfile::tempdir().expect("project");
        fs::write(
            directory.path().join("package.json"),
            r#"{"scripts":{"build":"vite build"}}"#,
        )
        .expect("manifest");
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).expect("bin");
        let npm = bin.join("npm");
        fs::write(&npm, b"#!/bin/sh\n").expect("fake npm");
        fs::set_permissions(&npm, fs::Permissions::from_mode(0o700)).expect("npm mode");
        let context_for = |text: &str| {
            CompletionContext::new(
                QueryId::new(1),
                ShellKind::Zsh,
                PathBuf::from(directory.path()),
                BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
                    .expect("buffer"),
            )
            .expect("context")
        };
        let path = OsString::from(bin);
        let commands = Arc::new(CommandPathCache::from_path(Some(&path)));
        let mut engine = CompletionEngine::new(100, 12);
        engine.register(ProjectProvider::new(
            Arc::new(ProjectCache::default()),
            commands,
            Arc::new(RwLock::new(HistoryIndex::default())),
        ));
        // `npm bu`: subcommands only — no `bu*` subcommand, and crucially no
        // script rows at the bare position.
        let output = engine.complete(&context_for("npm bu"));
        assert!(
            output.candidates.is_empty(),
            "bare npm must not offer scripts: {:?}",
            output
                .candidates
                .iter()
                .map(|candidate| candidate.display.primary.as_str())
                .collect::<Vec<_>>()
        );
        // `npm ru` → the `run` subcommand; `npm run bu` → the script.
        let output = engine.complete(&context_for("npm ru"));
        assert_eq!(output.candidates[0].display.primary, "npm run");
        let output = engine.complete(&context_for("npm run bu"));
        let candidate = output.candidates.first().expect("script candidate");
        assert_eq!(candidate.display.primary, "npm run build");
        assert_eq!(candidate.edit.as_ref().expect("edit").replacement, "build");
    }

    #[test]
    fn bare_manager_word_keeps_the_manager_in_the_fill() {
        // Cursor still on `pnpm`: the fill must rewrite the line to
        // `pnpm <script>` — never replace the manager word itself.
        let (_directory, context, engine) = bare_prefix_setup("pnpm");
        let output = engine.complete(&context);
        let candidate = output.candidates.first().expect("script candidate");
        let edit = candidate.edit.as_ref().expect("edit");
        assert_eq!(edit.range, 0..4);
        assert_eq!(edit.replacement, "pnpm 'build docs'");
    }

    #[test]
    fn trailing_space_after_manager_offers_scripts() {
        let (_directory, context, engine) = bare_prefix_setup("pnpm ");
        let output = engine.complete(&context);
        let candidate = output.candidates.first().expect("script candidate");
        assert_eq!(candidate.display.primary, "pnpm build docs");
        assert_eq!(
            candidate.edit.as_ref().expect("edit").replacement,
            "'build docs'"
        );
    }

    #[test]
    fn run_word_being_typed_keeps_the_run_keyword() {
        // Cursor still on `run`: the word matches no script name, so the fill
        // keeps the explicit run form — `pnpm run 'build docs'`.
        let (_directory, context, engine) = bare_prefix_setup("pnpm run");
        let output = engine.complete(&context);
        let candidate = output.candidates.first().expect("script candidate");
        let edit = candidate.edit.as_ref().expect("edit");
        assert_eq!(edit.range, 5..8);
        assert_eq!(edit.replacement, "run 'build docs'");
    }

    #[test]
    fn other_subcommands_do_not_fire_script_completion() {
        let (_directory, context, engine) = bare_prefix_setup("pnpm install vit");
        let output = engine.complete(&context);
        assert!(output.candidates.is_empty());
    }

    #[test]
    fn deno_leads_with_subcommands_and_tasks_behind_task_keyword() {
        let directory = tempfile::tempdir().expect("project");
        fs::write(
            directory.path().join("deno.json"),
            r#"{"tasks":{"dev":"deno run --watch main.ts","build":"deno compile main.ts"}}"#,
        )
        .expect("deno.json");
        let context_for = |text: &str| {
            CompletionContext::new(
                QueryId::new(1),
                ShellKind::Zsh,
                PathBuf::from(directory.path()),
                BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
                    .expect("buffer"),
            )
            .expect("context")
        };
        let mut engine = CompletionEngine::new(100, 12);
        engine.register(ProjectProvider::new(
            Arc::new(ProjectCache::default()),
            Arc::new(CommandPathCache::default()),
            Arc::new(RwLock::new(HistoryIndex::default())),
        ));
        // Bare deno: subcommands, no script rows.
        let output = engine.complete(&context_for("deno ta"));
        assert_eq!(output.candidates[0].display.primary, "deno task");
        // Behind `task`: deno.json tasks in `deno task <name>` form.
        let output = engine.complete(&context_for("deno task "));
        let mut names: Vec<_> = output
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["deno task build", "deno task dev"]);
        let dev = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "deno task dev")
            .expect("dev task");
        assert_eq!(dev.display.description, "deno run --watch main.ts");
    }

    #[test]
    fn scripts_are_ordered_by_recorded_usage() {
        let directory = tempfile::tempdir().expect("project");
        fs::write(
            directory.path().join("package.json"),
            r#"{"scripts":{"build":"vite build","dev":"vite dev"}}"#,
        )
        .expect("manifest");
        let policy = crate::history::HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        // `pnpm dev` was used many times, `pnpm build` once — dev must rank
        // above build even though alphabetical order says otherwise.
        index.ingest("pnpm build", 1_000, ShellKind::Zsh, None, Some(0), &policy);
        for round in 0..30 {
            index.ingest(
                "pnpm dev",
                2_000 + round,
                ShellKind::Zsh,
                None,
                Some(0),
                &policy,
            );
        }
        let mut engine = CompletionEngine::new(100, 12);
        engine.register(ProjectProvider::new(
            Arc::new(ProjectCache::default()),
            Arc::new(CommandPathCache::default()),
            Arc::new(RwLock::new(index)),
        ));
        let context = CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from(directory.path()),
            BufferSnapshot::new("pnpm ", 5, BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer"),
        )
        .expect("context");
        let output = engine.complete(&context);
        let names: Vec<_> = output
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect();
        assert_eq!(names, ["pnpm dev", "pnpm build"]);
    }

    #[test]
    fn pnpm_filter_offers_members_then_member_scripts() {
        let root = tempfile::tempdir().expect("root");
        fs::create_dir_all(root.path().join("packages/api")).expect("api");
        fs::create_dir_all(root.path().join("packages/web")).expect("web");
        fs::write(
            root.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n",
        )
        .expect("workspace yaml");
        fs::write(
            root.path().join("package.json"),
            r#"{"name":"root","scripts":{}}"#,
        )
        .expect("root manifest");
        fs::write(
            root.path().join("packages/api/package.json"),
            r#"{"name":"@acme/api","scripts":{"start":"node index.js"}}"#,
        )
        .expect("api manifest");
        fs::write(
            root.path().join("packages/web/package.json"),
            r#"{"name":"@acme/web","scripts":{"dev":"vite dev"}}"#,
        )
        .expect("web manifest");
        let context_for = |text: &str| {
            CompletionContext::new(
                QueryId::new(1),
                ShellKind::Zsh,
                root.path().canonicalize().expect("canonical"),
                BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
                    .expect("buffer"),
            )
            .expect("context")
        };
        let mut engine = CompletionEngine::new(100, 12);
        engine.register(ProjectProvider::new(
            Arc::new(ProjectCache::default()),
            Arc::new(CommandPathCache::default()),
            Arc::new(RwLock::new(HistoryIndex::default())),
        ));
        // Value position: workspace members.
        let output = engine.complete(&context_for("pnpm --filter "));
        let names: Vec<_> = output
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect();
        assert_eq!(names, ["@acme/api", "@acme/web"]);
        // After the member: that member's own scripts.
        let output = engine.complete(&context_for("pnpm --filter @acme/web "));
        let names: Vec<_> = output
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect();
        assert_eq!(names, ["pnpm --filter @acme/web dev"]);
        let edit = output.candidates[0].edit.as_ref().expect("edit");
        assert_eq!(edit.replacement, "dev");
    }

    fn rule_file_setup(
        manifest_name: &str,
        manifest: &str,
        tool: &str,
        buffer: &str,
    ) -> (tempfile::TempDir, CompletionContext, CompletionEngine) {
        let directory = tempfile::tempdir().expect("project");
        fs::write(directory.path().join(manifest_name), manifest).expect("rule file");
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).expect("bin");
        let tool_path = bin.join(tool);
        fs::write(&tool_path, b"#!/bin/sh\n").expect("fake tool");
        fs::set_permissions(&tool_path, fs::Permissions::from_mode(0o700)).expect("tool mode");
        let context = CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            // Canonicalized: discovery canonicalizes the cwd before walking
            // up, so the annotation strip-prefix must compare like with like
            // (macOS tempdirs live behind /var → /private/var).
            directory.path().canonicalize().expect("canonical cwd"),
            BufferSnapshot::new(
                buffer,
                buffer.len(),
                BufferRevision::new(1),
                SyncQuality::Exact,
            )
            .expect("buffer"),
        )
        .expect("context");
        let path = OsString::from(bin);
        let commands = Arc::new(CommandPathCache::from_path(Some(&path)));
        let mut engine = CompletionEngine::new(100, 12);
        engine.register(ProjectProvider::new(
            Arc::new(ProjectCache::default()),
            commands,
            Arc::new(RwLock::new(HistoryIndex::default())),
        ));
        (directory, context, engine)
    }

    const MAKEFILE: &str = "\
# Build the release binary.
build: deps
	cargo build --release

test: build
	cargo test

.PHONY: build test
";

    #[test]
    fn make_trailing_space_offers_targets_with_doc_comments() {
        let (_directory, context, engine) = rule_file_setup("Makefile", MAKEFILE, "make", "make ");
        let output = engine.complete(&context);
        let names: Vec<&str> = output
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect();
        assert_eq!(names, ["make build", "make test"]);
        let build = &output.candidates[0];
        assert_eq!(build.display.description, "Build the release binary.");
        assert_eq!(build.display.annotation.as_deref(), Some("Makefile"));
        assert_eq!(build.source, CandidateSource::Project);
        assert!(matches!(build.completeness, Completeness::Runnable));
        let edit = build.edit.as_ref().expect("edit");
        assert_eq!(edit.range, 5..5);
        assert_eq!(edit.replacement, "build");
        // Undocumented target: empty description, still offered.
        assert_eq!(output.candidates[1].display.description, "");
    }

    #[test]
    fn make_target_prefix_replaces_only_the_active_word() {
        let (_directory, context, engine) =
            rule_file_setup("Makefile", MAKEFILE, "make", "make bu");
        let output = engine.complete(&context);
        let build = output.candidates.first().expect("target candidate");
        let edit = build.edit.as_ref().expect("edit");
        assert_eq!(edit.range, 5..7);
        assert_eq!(edit.replacement, "build");
    }

    #[test]
    fn just_trailing_space_offers_justfile_targets() {
        let justfile = "# Serve the site.\n@serve:\n    python3 -m http.server\n";
        let (_directory, context, engine) = rule_file_setup("justfile", justfile, "just", "just ");
        let output = engine.complete(&context);
        let serve = output.candidates.first().expect("target candidate");
        assert_eq!(serve.display.primary, "just serve");
        assert_eq!(serve.display.description, "Serve the site.");
        assert_eq!(serve.display.annotation.as_deref(), Some("justfile"));
        assert_eq!(serve.edit.as_ref().expect("edit").replacement, "serve");
    }

    #[test]
    fn make_flag_and_deeper_positions_do_not_fire() {
        for buffer in ["make -f ", "make -f Mak", "make build extra"] {
            let (_directory, context, engine) =
                rule_file_setup("Makefile", MAKEFILE, "make", buffer);
            let output = engine.complete(&context);
            assert!(
                output.candidates.is_empty(),
                "no target rows expected for `{buffer}`"
            );
        }
    }

    #[test]
    fn make_attached_jobs_flag_still_offers_targets() {
        // `make -j4 bu`: the flag carries its own value, so the word after it
        // is still the target slot.
        let (_directory, context, engine) =
            rule_file_setup("Makefile", MAKEFILE, "make", "make -j4 bu");
        let output = engine.complete(&context);
        let build = output.candidates.first().expect("target candidate");
        assert_eq!(build.display.primary, "make build");
        let edit = build.edit.as_ref().expect("edit");
        assert_eq!(edit.range, 9..11);
        assert_eq!(edit.replacement, "build");

        // Trailing space after the flag: the full target list.
        let (_directory, context, engine) =
            rule_file_setup("Makefile", MAKEFILE, "make", "make -j4 ");
        let output = engine.complete(&context);
        let names: Vec<&str> = output
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect();
        assert_eq!(names, ["make build", "make test"]);
    }

    #[test]
    fn missing_rule_file_does_not_fire() {
        let directory = tempfile::tempdir().expect("project");
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).expect("bin");
        let tool = bin.join("make");
        fs::write(&tool, b"#!/bin/sh\n").expect("fake make");
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o700)).expect("make mode");
        let context = CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from(directory.path()),
            BufferSnapshot::new("make ", 5, BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer"),
        )
        .expect("context");
        let path = OsString::from(bin);
        let commands = Arc::new(CommandPathCache::from_path(Some(&path)));
        let mut engine = CompletionEngine::new(100, 12);
        engine.register(ProjectProvider::new(
            Arc::new(ProjectCache::default()),
            commands,
            Arc::new(RwLock::new(HistoryIndex::default())),
        ));
        assert!(engine.complete(&context).candidates.is_empty());
    }
}
