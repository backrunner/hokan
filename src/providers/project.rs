use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

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

use super::CommandHelpCache;

pub struct ProjectProvider {
    cache: Arc<ProjectCache>,
    makefiles: MakefileCache,
    commands: Arc<CommandPathCache>,
    history: Arc<RwLock<HistoryIndex>>,
    help: Option<Arc<CommandHelpCache>>,
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
            help: None,
            workspaces: NodeWorkspaceCache::default(),
        }
    }

    #[must_use]
    pub fn with_help(mut self, help: Arc<CommandHelpCache>) -> Self {
        self.help = Some(help);
        self
    }
}

impl CandidateProvider for ProjectProvider {
    fn id(&self) -> &'static str {
        "project"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        if node_run_position(context).is_some() {
            let Some(executable) =
                crate::providers::resolved_executable_path(context, &self.commands)
            else {
                return false;
            };
            return self.node_run_supported(context, executable);
        }
        if filter_position(context).is_some()
            || manager_option_position(context).is_some()
            || manager_position(context).is_some()
        {
            return if crate::providers::corepack_dispatch(context) {
                self.commands.contains("corepack")
            } else {
                crate::providers::resolved_executable_path(context, &self.commands).is_some()
            };
        }
        rule_file_invocation(context).is_some_and(|invocation| {
            self.commands.contains(invocation.tool)
                && discover_makefile(
                    &invocation.project_dir,
                    ManifestKind::for_tool(invocation.tool),
                )
                .is_some()
        })
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let mut output = if let Some(position) = node_run_position(context) {
            self.complete_node_run_scripts(context, &position)
        } else if let Some(position) = manager_option_position(context) {
            self.complete_manager_options(context, position)
        } else if let Some(position) = filter_position(context) {
            self.complete_filtered(context, position)
        } else if let Some(manager) = manager_position(context) {
            let project_dir = if manager.workspace_root || manager.recursive {
                self.workspaces.load(&manager.project_dir).map_or_else(
                    || manager.project_dir.clone(),
                    |workspace| workspace.root.clone(),
                )
            } else {
                manager.project_dir.clone()
            };
            if manager.recursive && matches!(manager.spec.name, "pnpm" | "npm") {
                self.complete_recursive_scripts(
                    context,
                    manager.spec,
                    manager.position,
                    &manager.project_dir,
                    manager.include_workspace_root,
                    manager.if_present,
                )
            } else {
                self.complete_scripts(context, manager.spec, manager.position, &project_dir)
            }
        } else if let Some(invocation) = rule_file_invocation(context) {
            self.complete_targets(context, &invocation)
        } else {
            ProviderOutput::default()
        };
        restrict_after_exact_edit(context, &mut output.candidates);
        output
    }
}

impl ProjectProvider {
    fn node_run_supported(
        &self,
        context: &CompletionContext,
        executable: std::path::PathBuf,
    ) -> bool {
        let Some(cache) = self.help.as_ref() else {
            return true;
        };
        let Some(command) = context.command() else {
            return false;
        };
        if let Some(help) = cache.peek(command) {
            return help.flags.iter().any(|entry| entry.name == "--run");
        }
        cache.request(command, Some(executable));
        false
    }

    fn complete_node_run_scripts(
        &self,
        context: &CompletionContext,
        position: &NodeRunPosition<'_>,
    ) -> ProviderOutput {
        let Some(manifest) = (match self.cache.load_nearest(&position.project_dir) {
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
            return ProviderOutput::default();
        };
        let origin = manifest
            .path
            .strip_prefix(context.cwd.as_ref())
            .unwrap_or(&manifest.path)
            .display()
            .to_string();
        let folded_prefix = position.prefix.to_lowercase();
        let now_ms = crate::history_now_ms();
        let invocation = context.command().unwrap_or("node");
        let candidates = manifest
            .scripts
            .iter()
            .filter(|(name, _)| name.to_lowercase().starts_with(&folded_prefix))
            .map(|(name, command)| {
                let escaped = escape_for_shell(name, QuoteContext::Unquoted, context.shell);
                let replacement = format!("--run={escaped}");
                let display =
                    resulting_primary(context, &replacement, &format!("{invocation} --run={name}"));
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
                    format!("project:{origin}:node-run:{name}"),
                );
                candidate.display.annotation = Some(origin.clone());
                candidate.score.cwd_affinity = 100;
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

    fn complete_manager_options(
        &self,
        context: &CompletionContext,
        position: ManagerOptionPosition,
    ) -> ProviderOutput {
        let candidates = manager_options(position.spec.name)
            .iter()
            .map(|option| {
                let replacement = option.name.to_owned();
                let display = resulting_primary(context, &replacement, option.name);
                Candidate::new(
                    context.query_id,
                    display,
                    option.description,
                    Some(TextEdit {
                        range: context.parsed.replacement.clone(),
                        replacement,
                        cursor_after: CursorPlacement::End,
                    }),
                    CandidateAction::InsertAndContinue {
                        next_slot: option.next_slot,
                    },
                    CandidateSource::Project,
                    CandidateKind::Command,
                    Completeness::NeedsInput {
                        slot: option.next_slot,
                    },
                    RiskLevel::Low,
                    format!(
                        "project:{}:option:{}:{}",
                        position.spec.name, position.after_script_keyword, option.name
                    ),
                )
            })
            .collect();
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }

    fn complete_recursive_scripts(
        &self,
        context: &CompletionContext,
        spec: &ManagerSpec,
        position: Position,
        project_dir: &std::path::Path,
        include_root: bool,
        if_present: bool,
    ) -> ProviderOutput {
        let Some(workspace) = self.workspaces.load(project_dir) else {
            return self.complete_scripts(context, spec, position, project_dir);
        };
        let command_position = matches!(position, Position::ManagerWord | Position::CommandToken);
        let mut output = if command_position {
            self.complete_subcommands(context, spec, position)
        } else {
            ProviderOutput::default()
        };
        if command_position && spec.keyword.is_some() {
            return output;
        }

        let mut scripts: BTreeMap<String, RecursiveScript> = BTreeMap::new();
        let mut target_count = workspace.members.len();
        for member in &workspace.members {
            for (name, command) in &member.scripts {
                add_recursive_script(&mut scripts, name, command);
            }
        }
        if include_root {
            match self.cache.load_nearest(&workspace.root) {
                Ok(Some(manifest)) if manifest.path.parent() == Some(workspace.root.as_path()) => {
                    target_count += 1;
                    for (name, command) in &manifest.scripts {
                        add_recursive_script(&mut scripts, name, command);
                    }
                }
                Ok(Some(_)) | Ok(None) => {}
                Err(error) => output.diagnostics.push(ProviderDiagnostic {
                    provider: self.id(),
                    code: "HK-PROJ-001",
                    message: error.to_string(),
                }),
            }
        }

        let origin = workspace
            .root
            .strip_prefix(context.cwd.as_ref())
            .unwrap_or(&workspace.root)
            .display()
            .to_string();
        let now_ms = crate::history_now_ms();
        output
            .candidates
            .extend(scripts.into_iter().filter_map(|(name, recursive)| {
                // `npm run --workspaces` fails when any selected workspace is
                // missing the script unless `--if-present` is enabled. pnpm
                // recursively skips missing scripts, so its useful set is the
                // union while npm's default set is the intersection.
                if spec.name == "npm" && !if_present && recursive.count < target_count {
                    return None;
                }
                if command_position
                    && spec
                        .subcommands
                        .iter()
                        .any(|(subcommand, _)| *subcommand == name)
                {
                    return None;
                }
                if !script_name_matches(context, position, &name) {
                    return None;
                }
                let mut candidate = self.script_candidate(
                    context,
                    spec,
                    position,
                    &name,
                    &recursive.command,
                    &origin,
                    now_ms,
                );
                candidate.risk = recursive.risk;
                if recursive.count > 1 {
                    candidate.display.description =
                        format!("{} 个 workspace 定义此脚本", recursive.count);
                }
                Some(candidate)
            }));
        output
    }

    fn complete_scripts(
        &self,
        context: &CompletionContext,
        spec: &ManagerSpec,
        position: Position,
        project_dir: &std::path::Path,
    ) -> ProviderOutput {
        let command_position = matches!(position, Position::ManagerWord | Position::CommandToken);
        let mut output = if command_position {
            self.complete_subcommands(context, spec, position)
        } else {
            ProviderOutput::default()
        };
        // npm and deno require `run` / `task`; their command position never
        // leaks package scripts. Direct managers mix native commands and
        // non-conflicting project scripts at the first argument.
        if command_position && spec.keyword.is_some() {
            return output;
        }
        let Some(manifest) = (match self.cache.load_nearest(project_dir) {
            Ok(manifest) => manifest,
            Err(error) => {
                output.diagnostics.push(ProviderDiagnostic {
                    provider: self.id(),
                    code: "HK-PROJ-001",
                    message: error.to_string(),
                });
                return output;
            }
        }) else {
            // deno.json tasks still apply for deno even without package.json.
            let mut deno = self.complete_deno_tasks(context, spec, position, project_dir, None);
            output.candidates.append(&mut deno.candidates);
            output.diagnostics.append(&mut deno.diagnostics);
            return output;
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
            .filter(|(name, _)| {
                script_name_matches(context, position, name)
                    && (!command_position
                        || !spec
                            .subcommands
                            .iter()
                            .any(|(subcommand, _)| *subcommand == name.as_str()))
            })
            .map(|(name, script)| {
                self.script_candidate(context, spec, position, name, script, &relative, now_ms)
            })
            .collect();
        if spec.name == "deno" {
            let mut deno =
                self.complete_deno_tasks(context, spec, position, project_dir, Some(now_ms));
            candidates.append(&mut deno.candidates);
        }
        output.candidates.append(&mut candidates);
        output
    }

    /// deno.json(c) `tasks` for deno, in the same `deno task <name>` form.
    fn complete_deno_tasks(
        &self,
        context: &CompletionContext,
        spec: &ManagerSpec,
        position: Position,
        project_dir: &std::path::Path,
        now_ms: Option<i64>,
    ) -> ProviderOutput {
        if spec.name != "deno" {
            return ProviderOutput::default();
        }
        let Ok(Some(manifest)) = self.cache.load_deno_nearest(project_dir) else {
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
                .filter(|(name, _)| script_name_matches(context, position, name))
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
        let invocation = context.command().unwrap_or(spec.name);
        // Native form: pnpm/yarn/bun run scripts directly, npm needs `run`,
        // deno needs `task`. The keyword positions keep the keyword in place.
        let canonical = match (spec.keyword, position) {
            (None, Position::KeywordWord | Position::ScriptToken) => {
                format!("{invocation} run {name}")
            }
            (None, _) => format!("{invocation} {name}"),
            (Some(keyword), _) => format!("{invocation} {keyword} {name}"),
        };
        let replacement = match position {
            Position::ScriptToken => escaped.clone(),
            Position::KeywordWord => format!("{keyword} {escaped}"),
            Position::CommandToken => escaped.clone(),
            Position::ManagerWord => format!("{invocation} {escaped}"),
        };
        let display = resulting_primary(context, &replacement, &canonical);
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

    /// First-argument rows for each manager's own command surface.
    fn complete_subcommands(
        &self,
        context: &CompletionContext,
        spec: &ManagerSpec,
        position: Position,
    ) -> ProviderOutput {
        let now_ms = crate::history_now_ms();
        let invocation = context.command().unwrap_or(spec.name);
        let candidates = spec
            .subcommands
            .iter()
            .enumerate()
            .map(|(index, (name, description))| {
                let replacement = match position {
                    Position::ManagerWord => format!("{invocation} {name}"),
                    _ => (*name).to_owned(),
                };
                let display =
                    resulting_primary(context, &replacement, &format!("{invocation} {name}"));
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
                candidate.score.spec_priority =
                    i16::try_from(spec.subcommands.len().saturating_sub(index)).unwrap_or_default();
                candidate
            })
            .collect();
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }

    /// Workspace completions: member names at selector values, then native
    /// manager commands plus non-conflicting direct scripts (or scripts after
    /// an explicit `run`).
    fn complete_filtered(
        &self,
        context: &CompletionContext,
        position: FilterPosition,
    ) -> ProviderOutput {
        match position {
            FilterPosition::Value {
                style,
                project_dir,
                edit_prefix,
            } => {
                let Some(workspace) = self.workspaces.load(&project_dir) else {
                    return ProviderOutput::default();
                };
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
                                replacement: format!("{edit_prefix}{}", member.name),
                                cursor_after: CursorPlacement::End,
                            }),
                            CandidateAction::InsertAndContinue {
                                next_slot: SlotKind::Value,
                            },
                            CandidateSource::Project,
                            CandidateKind::Command,
                            Completeness::NeedsInput {
                                slot: SlotKind::Value,
                            },
                            RiskLevel::Low,
                            format!(
                                "project:{relative_root}:{}:{}",
                                workspace_style_name(style),
                                member.name
                            ),
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
            FilterPosition::MemberCommand {
                style,
                project_dir,
                member,
            } => {
                let Some(mut scripts) = self.member_script_candidates(
                    context,
                    style,
                    &project_dir,
                    &member,
                    Position::ScriptToken,
                    false,
                ) else {
                    return ProviderOutput::default();
                };
                let spec = manager_for_workspace_style(style);
                let mut output = self.complete_subcommands(context, spec, Position::CommandToken);
                if spec.keyword.is_none() {
                    scripts.retain(|candidate| {
                        let replacement = candidate
                            .edit
                            .as_ref()
                            .map(|edit| edit.replacement.as_str())
                            .unwrap_or_default();
                        !spec
                            .subcommands
                            .iter()
                            .any(|(subcommand, _)| *subcommand == replacement)
                    });
                    output.candidates.extend(scripts);
                }
                output
            }
            FilterPosition::MemberScripts {
                style,
                project_dir,
                member,
                on_keyword,
                explicit_run,
            } => {
                let position = if on_keyword {
                    Position::KeywordWord
                } else {
                    Position::ScriptToken
                };
                let Some(candidates) = self.member_script_candidates(
                    context,
                    style,
                    &project_dir,
                    &member,
                    position,
                    explicit_run,
                ) else {
                    return ProviderOutput::default();
                };
                ProviderOutput {
                    candidates,
                    diagnostics: Vec::new(),
                }
            }
        }
    }

    fn member_script_candidates(
        &self,
        context: &CompletionContext,
        style: WorkspaceStyle,
        project_dir: &std::path::Path,
        member_name: &str,
        position: Position,
        explicit_run: bool,
    ) -> Option<Vec<Candidate>> {
        let workspace = self.workspaces.load(project_dir)?;
        let member = workspace
            .members
            .iter()
            .find(|member| member.name == member_name)?;
        let now_ms = crate::history_now_ms();
        let relative = workspace
            .root
            .strip_prefix(context.cwd.as_ref())
            .unwrap_or(&workspace.root)
            .display()
            .to_string();
        Some(
            member
                .scripts
                .iter()
                .filter(|(name, _)| script_name_matches(context, position, name))
                .map(|(name, script)| {
                    self.member_script_candidate(
                        context,
                        member,
                        name,
                        script,
                        &relative,
                        now_ms,
                        position,
                        explicit_run,
                        style,
                    )
                })
                .collect(),
        )
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
        explicit_run: bool,
        style: WorkspaceStyle,
    ) -> Candidate {
        let escaped = escape_for_shell(name, QuoteContext::Unquoted, context.shell);
        let canonical = format!(
            "{} {name}",
            workspace_prefix(style, &member.name, explicit_run)
        );
        let replacement = match position {
            Position::KeywordWord => format!("run {escaped}"),
            _ => escaped.clone(),
        };
        let display = resulting_primary(context, &replacement, &canonical);
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
            format!(
                "project:{origin}:{}:{}:{name}",
                workspace_style_name(style),
                member.name
            ),
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
    fn complete_targets(
        &self,
        context: &CompletionContext,
        invocation: &RuleFileInvocation,
    ) -> ProviderOutput {
        let tool = invocation.tool;
        let manifest = match self
            .makefiles
            .load_nearest(&invocation.project_dir, ManifestKind::for_tool(tool))
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
            .filter(|target| token_name_matches(context, &target.name))
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

fn script_name_matches(context: &CompletionContext, position: Position, name: &str) -> bool {
    matches!(position, Position::ManagerWord | Position::KeywordWord)
        || token_name_matches(context, name)
}

fn token_name_matches(context: &CompletionContext, name: &str) -> bool {
    let query = context.parsed.current_prefix.as_str();
    query.is_empty() || name.to_lowercase().starts_with(&query.to_lowercase())
}

struct NodeRunPosition<'a> {
    prefix: &'a str,
    project_dir: std::path::PathBuf,
}

fn node_run_position(context: &CompletionContext) -> Option<NodeRunPosition<'_>> {
    if crate::providers::redirect_target(context)
        || !crate::providers::effective_command_accepts_external(context)
    {
        return None;
    }
    let command = crate::providers::executable_basename(context.command()?);
    if !matches!(command, "node" | "nodejs") {
        return None;
    }
    let prefix = context.parsed.current_prefix.strip_prefix("--run=")?;
    if prefix
        .chars()
        .any(|character| matches!(character, '$' | '`' | '*' | '?' | '[' | '{'))
    {
        return None;
    }
    let (words, position) = crate::providers::argument_progress(context)?;
    let before = words.get(1..=position).unwrap_or_default();
    node_options_allow_run(before).then(|| NodeRunPosition {
        prefix,
        project_dir: crate::providers::invocation_working_directory(context),
    })
}

fn node_options_allow_run(arguments: &[&str]) -> bool {
    let mut index = 0;
    while let Some(word) = arguments.get(index).copied() {
        if word == "--"
            || word == "-"
            || word == "--run"
            || word.starts_with("--run=")
            || matches!(
                word,
                "-c" | "--check"
                    | "-e"
                    | "--eval"
                    | "-p"
                    | "--print"
                    | "--completion-bash"
                    | "-h"
                    | "--help"
                    | "-v"
                    | "--version"
                    | "--v8-options"
                    | "--prof-process"
            )
            || word.starts_with("--eval=")
            || word.starts_with("--print=")
            || word.len() > 2 && matches!(&word[..2], "-e" | "-p")
        {
            return false;
        }
        if node_option_takes_separate_value(word) {
            if index + 1 >= arguments.len() {
                return false;
            }
            index += 2;
            continue;
        }
        if !word.starts_with('-') {
            return false;
        }
        index += 1;
    }
    true
}

fn node_option_takes_separate_value(word: &str) -> bool {
    matches!(
        word,
        "-C" | "--conditions"
            | "--cpu-prof-dir"
            | "--cpu-prof-interval"
            | "--cpu-prof-name"
            | "--diagnostic-dir"
            | "--disable-proto"
            | "--disable-warning"
            | "--dns-result-order"
            | "--env-file"
            | "--env-file-if-exists"
            | "--experimental-config-file"
            | "--experimental-default-type"
            | "--experimental-loader"
            | "--loader"
            | "--heap-prof-dir"
            | "--heap-prof-interval"
            | "--heap-prof-name"
            | "--icu-data-dir"
            | "--import"
            | "--input-type"
            | "--inspect-port"
            | "--localstorage-file"
            | "--openssl-config"
            | "--redirect-warnings"
            | "--report-directory"
            | "--report-filename"
            | "-r"
            | "--require"
            | "--snapshot-blob"
            | "--test-name-pattern"
            | "--test-reporter"
            | "--test-reporter-destination"
            | "--test-shard"
            | "--test-skip-pattern"
            | "--test-timeout"
            | "--title"
            | "--tls-cipher-list"
            | "--tls-keylog"
            | "--trace-event-categories"
            | "--trace-event-file-pattern"
            | "--unhandled-rejections"
            | "--use-largepages"
            | "--v8-pool-size"
            | "--watch-kill-signal"
            | "--watch-path"
    )
}

struct RecursiveScript {
    command: String,
    count: usize,
    risk: RiskLevel,
}

fn add_recursive_script(
    scripts: &mut BTreeMap<String, RecursiveScript>,
    name: &str,
    command: &str,
) {
    let risk = crate::safety::classify_command(command).level;
    scripts
        .entry(name.to_owned())
        .and_modify(|script| {
            script.count += 1;
            if risk_weight(risk) > risk_weight(script.risk) {
                script.risk = risk;
            }
        })
        .or_insert_with(|| RecursiveScript {
            command: command.to_owned(),
            count: 1,
            risk,
        });
}

const fn risk_weight(risk: RiskLevel) -> u8 {
    match risk {
        RiskLevel::ReadOnly => 0,
        RiskLevel::Low => 1,
        RiskLevel::Medium => 2,
        RiskLevel::High => 3,
        RiskLevel::Unknown => 4,
    }
}

/// Node package-manager positions (`manager_position`, `filter_position`,
/// `ManagerSpec`, `Position`, `FilterPosition`, `segment_words`) live in
/// `crate::providers` so the filesystem provider can suppress its rows at the
/// same slots the project provider owns.
use super::{
    FilterPosition, ManagerOptionPosition, ManagerSpec, Position, WorkspaceStyle, filter_position,
    manager_option_position, manager_position, segment_words,
};

#[derive(Clone, Copy)]
struct ManagerOptionSpec {
    name: &'static str,
    description: &'static str,
    next_slot: SlotKind,
}

macro_rules! manager_option {
    ($name:literal, $description:literal, $slot:ident) => {
        ManagerOptionSpec {
            name: $name,
            description: $description,
            next_slot: SlotKind::$slot,
        }
    };
}

const PNPM_OPTIONS: &[ManagerOptionSpec] = &[
    manager_option!("-C", "在指定目录运行", Directory),
    manager_option!("--dir", "在指定目录运行", Directory),
    manager_option!("-F", "筛选 workspace 成员", Value),
    manager_option!("--filter", "筛选 workspace 成员", Value),
    manager_option!("-w", "从 workspace 根目录运行", Value),
    manager_option!("--workspace-root", "从 workspace 根目录运行", Value),
    manager_option!("-r", "递归运行 workspace 命令", Value),
    manager_option!("--recursive", "递归运行 workspace 命令", Value),
    manager_option!("--include-workspace-root", "包含 workspace 根包", Value),
    manager_option!("--if-present", "缺少脚本时不报错", Value),
    manager_option!("--offline", "仅使用本地缓存", Value),
    manager_option!("--prefer-offline", "优先使用本地缓存", Value),
    manager_option!("--parallel", "并行执行 workspace 命令", Value),
    manager_option!("--stream", "实时输出 workspace 日志", Value),
    manager_option!("--silent", "减少命令输出", Value),
    manager_option!("--reporter", "选择日志 reporter", Value),
    manager_option!("--workspace-concurrency", "设置 workspace 并发数", Value),
    manager_option!("--store-dir", "选择全局 store 目录", Directory),
    manager_option!("--cache-dir", "选择缓存目录", Directory),
    manager_option!("--config-dir", "选择配置目录", Directory),
];

const NPM_OPTIONS: &[ManagerOptionSpec] = &[
    manager_option!("--prefix", "在指定项目目录运行", Directory),
    manager_option!("-w", "选择 workspace 成员", Value),
    manager_option!("--workspace", "选择 workspace 成员", Value),
    manager_option!("--workspaces", "在全部 workspaces 中运行", Value),
    manager_option!("--include-workspace-root", "包含 workspace 根包", Value),
    manager_option!("--if-present", "缺少脚本时不报错", Value),
    manager_option!("--ignore-scripts", "禁用生命周期脚本", Value),
    manager_option!("--foreground-scripts", "前台运行生命周期脚本", Value),
    manager_option!("--json", "输出 JSON", Value),
    manager_option!("--silent", "减少命令输出", Value),
    manager_option!("--userconfig", "选择 npm 配置文件", Path),
    manager_option!("--cache", "选择缓存目录", Directory),
    manager_option!("--registry", "选择 npm registry", Value),
    manager_option!("--location", "选择配置位置", Value),
    manager_option!("--script-shell", "选择脚本 shell", Value),
];

const YARN_OPTIONS: &[ManagerOptionSpec] = &[
    manager_option!("--cwd", "在指定目录运行", Directory),
    manager_option!("--verbose", "显示详细日志", Value),
    manager_option!("--json", "输出 JSON", Value),
    manager_option!("--silent", "减少命令输出", Value),
    manager_option!("--offline", "仅使用本地缓存", Value),
    manager_option!("--immutable", "禁止修改 lockfile", Value),
    manager_option!("--mutex", "设置 Yarn 互斥方式", Value),
    manager_option!("--cache-folder", "选择缓存目录", Directory),
    manager_option!("--modules-folder", "选择依赖安装目录", Directory),
    manager_option!("--use-yarnrc", "选择 yarnrc 文件", Path),
];

const BUN_OPTIONS: &[ManagerOptionSpec] = &[
    manager_option!("--cwd", "在指定目录运行", Directory),
    manager_option!("--config", "选择 Bun 配置文件", Path),
    manager_option!("--silent", "减少命令输出", Value),
    manager_option!("--backend", "选择运行后端", Value),
];

const DENO_OPTIONS: &[ManagerOptionSpec] = &[
    manager_option!("-q", "减少命令输出", Value),
    manager_option!("--quiet", "减少命令输出", Value),
    manager_option!("--config", "选择 Deno 配置文件", Path),
    manager_option!("--no-config", "禁用配置文件", Value),
    manager_option!("--import-map", "选择 import map", Path),
    manager_option!("--lock", "选择 lockfile", Path),
    manager_option!("--env-file", "选择环境变量文件", Path),
    manager_option!("--node-modules-dir", "设置 node_modules 模式", Value),
    manager_option!("--unstable", "启用不稳定 API", Value),
];

fn manager_options(manager: &str) -> &'static [ManagerOptionSpec] {
    match manager {
        "pnpm" => PNPM_OPTIONS,
        "npm" => NPM_OPTIONS,
        "yarn" => YARN_OPTIONS,
        "bun" => BUN_OPTIONS,
        "deno" => DENO_OPTIONS,
        _ => &[],
    }
}

fn resulting_primary(context: &CompletionContext, replacement: &str, fallback: &str) -> String {
    crate::parser::apply_edit(
        &context.buffer.text,
        context.parsed.replacement.clone(),
        replacement,
    )
    .map(|result| result.trim_end().to_owned())
    .unwrap_or_else(|_| fallback.trim_end().to_owned())
}

/// Once the active token exactly names one offered choice, unrelated fuzzy
/// siblings must disappear. Keep only genuine longer-prefix continuations:
/// `pnpm install` stays quiet instead of surfacing `uninstall`, while
/// `pnpm run` can still continue to `run <script>` and `test` to `test:unit`.
fn restrict_after_exact_edit(context: &CompletionContext, candidates: &mut Vec<Candidate>) {
    let query = context.parsed.current_prefix.as_str();
    if query.is_empty() {
        return;
    }
    let escaped = escape_for_shell(query, QuoteContext::Unquoted, context.shell);
    let exact_choice = candidates.iter().find(|candidate| {
        candidate
            .edit
            .as_ref()
            .is_some_and(|edit| edit.replacement == escaped)
    });
    if exact_choice.is_some_and(|candidate| candidate.kind == CandidateKind::Command) {
        candidates.clear();
        return;
    }
    let exact = exact_choice.is_some();
    if !exact {
        return;
    }
    candidates.retain(|candidate| {
        candidate.edit.as_ref().is_some_and(|edit| {
            edit.replacement.len() > escaped.len() && edit.replacement.starts_with(&escaped)
        })
    });
}

fn workspace_style_name(style: WorkspaceStyle) -> &'static str {
    match style {
        WorkspaceStyle::PnpmFilter => "pnpm-filter",
        WorkspaceStyle::NpmWorkspace => "npm-workspace",
        WorkspaceStyle::YarnWorkspace => "yarn-workspace",
    }
}

fn manager_for_workspace_style(style: WorkspaceStyle) -> &'static ManagerSpec {
    let name = match style {
        WorkspaceStyle::PnpmFilter => "pnpm",
        WorkspaceStyle::NpmWorkspace => "npm",
        WorkspaceStyle::YarnWorkspace => "yarn",
    };
    super::MANAGERS
        .iter()
        .find(|spec| spec.name == name)
        .expect("workspace style must have a package-manager spec")
}

fn workspace_prefix(style: WorkspaceStyle, member: &str, explicit_run: bool) -> String {
    match (style, explicit_run) {
        (WorkspaceStyle::PnpmFilter, false) => format!("pnpm --filter {member}"),
        (WorkspaceStyle::PnpmFilter, true) => format!("pnpm --filter {member} run"),
        (WorkspaceStyle::NpmWorkspace, _) => format!("npm --workspace {member} run"),
        (WorkspaceStyle::YarnWorkspace, false) => format!("yarn workspace {member}"),
        (WorkspaceStyle::YarnWorkspace, true) => format!("yarn workspace {member} run"),
    }
}

struct RuleFileInvocation {
    tool: &'static str,
    project_dir: std::path::PathBuf,
}

/// Matches the `make`/`just` first-target position and resolves global
/// working-directory options before loading the rule file. Explicit rule-file
/// options (`make -f`, `just --justfile`) deliberately block recommendations:
/// falling back to the default file would return valid-looking targets from
/// the wrong source.
fn rule_file_invocation(context: &CompletionContext) -> Option<RuleFileInvocation> {
    if super::redirect_target(context) || !super::effective_command_accepts_external(context) {
        return None;
    }
    let words = segment_words(context);
    let tool = match words.first() {
        Some(&"make") => "make",
        Some(&"just") => "just",
        _ => return None,
    };
    let trailing_space = context.buffer.text[..context.buffer.cursor]
        .chars()
        .next_back()
        .is_some_and(char::is_whitespace);
    let mut project_dir = super::invocation_working_directory(context);
    // Attached-value flag words (`make -j4`) carry their own value, so the
    // word after them is still the target slot.
    let mut index = 1;
    while let Some(word) = words.get(index).copied() {
        if let Some(directory) = attached_rule_directory(tool, word) {
            project_dir = super::resolve_directory(&project_dir, directory);
            index += 1;
            continue;
        }
        if rule_directory_option(tool, word) {
            let directory = words.get(index + 1).copied()?;
            project_dir = super::resolve_directory(&project_dir, directory);
            index += 2;
            continue;
        }
        if explicit_rule_file_option(tool, word) {
            return None;
        }
        if is_rule_modifier_flag(tool, word) {
            index += 1;
            continue;
        }
        if is_rule_assignment(word) {
            index += 1;
            continue;
        }
        break;
    }
    match &words[index..] {
        [] if trailing_space => Some(RuleFileInvocation { tool, project_dir }),
        [target] if !target.starts_with('-') => Some(RuleFileInvocation { tool, project_dir }),
        _ => None,
    }
}

fn is_rule_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn rule_directory_option(tool: &str, word: &str) -> bool {
    matches!((tool, word), ("make", "-C" | "--directory"))
        || matches!((tool, word), ("just", "-d" | "--working-directory"))
}

fn attached_rule_directory<'a>(tool: &str, word: &'a str) -> Option<&'a str> {
    if let Some((flag, value)) = word.split_once('=')
        && rule_directory_option(tool, flag)
        && !value.is_empty()
    {
        return Some(value);
    }
    let short = match tool {
        "make" => "-C",
        "just" => "-d",
        _ => return None,
    };
    (word.len() > short.len() && word.starts_with(short)).then_some(&word[short.len()..])
}

fn explicit_rule_file_option(tool: &str, word: &str) -> bool {
    let flag = word.split_once('=').map_or(word, |(flag, _)| flag);
    matches!(
        (tool, flag),
        ("make", "-f" | "--file" | "--makefile") | ("just", "-f" | "--justfile")
    ) || (tool == "make" && word.len() > 2 && word.starts_with("-f"))
        || (tool == "just" && word.len() > 2 && word.starts_with("-f"))
}

/// Global modifiers that do not change which rule file supplies targets.
/// Unknown flags stay conservative instead of guessing whether their next
/// word is a value or a target.
fn is_rule_modifier_flag(tool: &str, word: &str) -> bool {
    match tool {
        "make" => {
            matches!(
                word,
                "-B" | "--always-make"
                    | "-e"
                    | "--environment-overrides"
                    | "-i"
                    | "--ignore-errors"
                    | "-k"
                    | "--keep-going"
                    | "-L"
                    | "--check-symlink-times"
                    | "-n"
                    | "--just-print"
                    | "--dry-run"
                    | "--recon"
                    | "-q"
                    | "--question"
                    | "-r"
                    | "--no-builtin-rules"
                    | "-R"
                    | "--no-builtin-variables"
                    | "-s"
                    | "--silent"
                    | "--quiet"
                    | "-S"
                    | "--no-keep-going"
                    | "--stop"
                    | "-t"
                    | "--touch"
                    | "--trace"
                    | "-w"
                    | "--print-directory"
                    | "--no-print-directory"
                    | "--warn-undefined-variables"
                    | "-j"
                    | "--jobs"
            ) || (word.len() > 2
                && word.starts_with("-j")
                && word[2..]
                    .chars()
                    .all(|character| character.is_ascii_digit()))
                || word
                    .strip_prefix("--jobs=")
                    .is_some_and(|value| value.chars().all(|character| character.is_ascii_digit()))
        }
        "just" => matches!(
            word,
            "--dry-run"
                | "--no-deps"
                | "--no-dotenv"
                | "--no-highlight"
                | "-q"
                | "--quiet"
                | "--unstable"
                | "-y"
                | "--yes"
        ),
        _ => false,
    }
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
    use std::{
        ffi::OsString,
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
    };

    use super::*;
    use crate::{
        completion::{BufferSnapshot, CompletionEngine, SyncQuality},
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };

    fn command_cache(directory: &Path, names: &[&str]) -> Arc<CommandPathCache> {
        let bin = directory.join("bin");
        fs::create_dir_all(&bin).expect("bin");
        for name in names {
            let executable = bin.join(name);
            fs::write(&executable, b"#!/bin/sh\n").expect("fake executable");
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
                .expect("executable mode");
        }
        Arc::new(CommandPathCache::from_path(Some(&OsString::from(bin))))
    }

    #[test]
    fn replaces_only_the_script_token() {
        let directory = tempfile::tempdir().expect("project");
        fs::write(
            directory.path().join("package.json"),
            r#"{"scripts":{"build docs":"vite build"}}"#,
        )
        .expect("manifest");
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
        let commands = command_cache(directory.path(), &["pnpm"]);
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

    #[test]
    fn node_run_completes_only_attached_package_script_values() {
        let directory = tempfile::tempdir().expect("project");
        fs::write(
            directory.path().join("package.json"),
            r#"{"scripts":{"build":"vite build","build docs":"vitepress build","test":"vitest"}}"#,
        )
        .expect("manifest");
        fs::create_dir(directory.path().join("app")).expect("nested project");
        fs::write(
            directory.path().join("app/package.json"),
            r#"{"scripts":{"bundle":"vite build"}}"#,
        )
        .expect("nested manifest");
        let commands = command_cache(directory.path(), &["node", "nodejs"]);
        let mut engine = CompletionEngine::new(100, 20);
        engine.register(ProjectProvider::new(
            Arc::new(ProjectCache::default()),
            commands,
            Arc::new(RwLock::new(HistoryIndex::default())),
        ));
        let rows = |text: &str, query: u64| {
            let context = CompletionContext::new(
                QueryId::new(query),
                ShellKind::Zsh,
                directory.path().to_owned(),
                BufferSnapshot::new(
                    text,
                    text.len(),
                    BufferRevision::new(query),
                    SyncQuality::Exact,
                )
                .expect("buffer"),
            )
            .expect("context");
            engine
                .complete(&context)
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            rows("node --run=bu", 30),
            ["node --run=build", "node --run='build docs'"]
        );
        assert_eq!(
            rows("node -r preload --run=bu", 31),
            [
                "node -r preload --run=build",
                "node -r preload --run='build docs'",
            ]
        );
        assert_eq!(rows("nodejs --run=te", 32), ["nodejs --run=test"]);
        assert_eq!(
            rows("env -C app node --run=bu", 33),
            ["env -C app node --run=bundle"]
        );
        for text in [
            "node --run bu",
            "node app.js --run=bu",
            "node --eval code --run=bu",
            "node --run=missing",
        ] {
            assert!(rows(text, 34).is_empty(), "unexpected row for {text:?}");
        }
    }

    #[test]
    fn node_run_requires_the_installed_node_help_to_expose_the_flag() {
        let directory = tempfile::tempdir().expect("project");
        let commands = command_cache(directory.path(), &["node"]);
        let help = Arc::new(CommandHelpCache::default());
        let provider = ProjectProvider::new(
            Arc::new(ProjectCache::default()),
            commands,
            Arc::new(RwLock::new(HistoryIndex::default())),
        )
        .with_help(Arc::clone(&help));
        let context = CompletionContext::new(
            QueryId::new(40),
            ShellKind::Zsh,
            directory.path().to_owned(),
            BufferSnapshot::new(
                "node --run=bu",
                "node --run=bu".len(),
                BufferRevision::new(40),
                SyncQuality::Exact,
            )
            .expect("buffer"),
        )
        .expect("context");

        help.seed(
            "node",
            crate::providers::command_help::CommandHelp::default(),
        );
        assert!(!provider.applies(&context));

        help.seed(
            "node",
            crate::providers::command_help::CommandHelp {
                flags: vec![crate::providers::command_help::HelpEntry {
                    name: "--run".into(),
                    description: "Run a package.json script".into(),
                    takes_value: false,
                }],
                ..crate::providers::command_help::CommandHelp::default()
            },
        );
        assert!(provider.applies(&context));
    }

    fn bare_prefix_setup(buffer: &str) -> (tempfile::TempDir, CompletionContext, CompletionEngine) {
        let directory = tempfile::tempdir().expect("project");
        fs::write(
            directory.path().join("package.json"),
            r#"{"scripts":{"build docs":"vite build"}}"#,
        )
        .expect("manifest");
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
        let commands = command_cache(
            directory.path(),
            &["pnpm", "yarn", "bun", "npm", "deno", "corepack"],
        );
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
        assert_eq!(candidate.display.primary, "pnpm 'build docs'");
        let edit = candidate.edit.as_ref().expect("edit");
        assert_eq!(edit.range, 5..7);
        assert_eq!(edit.replacement, "'build docs'");

        let (_directory, context, engine) = bare_prefix_setup("npm run-script bu");
        let output = engine.complete(&context);
        let candidate = output.candidates.first().expect("npm script candidate");
        assert_eq!(candidate.display.primary, "npm run-script 'build docs'");
        assert_eq!(
            candidate.edit.as_ref().expect("edit").replacement,
            "'build docs'"
        );
    }

    #[test]
    fn explicit_manager_paths_preserve_the_invocation() {
        let (directory, _, engine) = bare_prefix_setup("pnpm");
        let rows = |text: &str, query: u64| {
            let context = CompletionContext::new(
                QueryId::new(query),
                ShellKind::Zsh,
                directory.path().to_owned(),
                BufferSnapshot::new(
                    text,
                    text.len(),
                    BufferRevision::new(query),
                    SyncQuality::Exact,
                )
                .expect("buffer"),
            )
            .expect("context");
            engine
                .complete(&context)
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect::<Vec<_>>()
        };

        assert!(
            rows("./bin/pnpm", 20)
                .iter()
                .any(|row| row == "./bin/pnpm install")
        );
        assert_eq!(rows("./bin/pnpm bu", 21), ["./bin/pnpm 'build docs'"]);
    }

    #[test]
    fn project_scripts_require_a_real_name_prefix() {
        for buffer in ["pnpm bid", "pnpm run bid", "npm run bid"] {
            let (_directory, context, engine) = bare_prefix_setup(buffer);
            assert!(
                engine.complete(&context).candidates.is_empty(),
                "a scattered script-name match leaked for {buffer:?}"
            );
        }
    }

    #[test]
    fn manager_command_surfaces_cover_common_prefixes() {
        for (buffer, expected) in [
            ("npm ca", "npm cache"),
            ("pnpm pat", "pnpm patch"),
            ("yarn upgr", "yarn upgrade"),
            ("bun rep", "bun repl"),
            ("deno cov", "deno coverage"),
        ] {
            let (_directory, context, engine) = bare_prefix_setup(buffer);
            let rows: Vec<_> = engine
                .complete(&context)
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect();
            assert!(
                rows.iter().any(|row| row == expected),
                "missing {expected:?} for {buffer:?}: {rows:?}"
            );
        }

        let (_directory, context, engine) = bare_prefix_setup("npm ");
        assert_eq!(
            engine
                .complete(&context)
                .candidates
                .first()
                .map(|candidate| candidate.display.primary.as_str()),
            Some("npm install")
        );
    }

    #[test]
    fn corepack_dispatch_continues_into_the_manager_state_machine() {
        let (_directory, context, engine) = bare_prefix_setup("corepack pnpm bu");
        let candidate = engine
            .complete(&context)
            .candidates
            .into_iter()
            .find(|candidate| candidate.display.primary == "corepack pnpm 'build docs'")
            .expect("corepack-dispatched script");
        let edit = candidate.edit.as_ref().expect("edit");
        assert_eq!(
            crate::parser::apply_edit(&context.buffer.text, edit.range.clone(), &edit.replacement)
                .expect("apply edit"),
            "corepack pnpm 'build docs'"
        );
    }

    #[test]
    fn corepack_dispatch_requires_the_outer_executable() {
        let project = tempfile::tempdir().expect("project");
        fs::write(
            project.path().join("package.json"),
            r#"{"scripts":{"build":"vite build"}}"#,
        )
        .expect("manifest");
        let context = CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            project.path().to_owned(),
            BufferSnapshot::new(
                "corepack pnpm bu",
                16,
                BufferRevision::new(1),
                SyncQuality::Exact,
            )
            .expect("buffer"),
        )
        .expect("context");

        let manager_bin = tempfile::tempdir().expect("manager bin");
        let mut missing_corepack = CompletionEngine::new(100, 12);
        missing_corepack.register(ProjectProvider::new(
            Arc::new(ProjectCache::default()),
            command_cache(manager_bin.path(), &["pnpm"]),
            Arc::new(RwLock::new(HistoryIndex::default())),
        ));
        assert!(missing_corepack.complete(&context).candidates.is_empty());

        let corepack_bin = tempfile::tempdir().expect("corepack bin");
        let mut available = CompletionEngine::new(100, 12);
        available.register(ProjectProvider::new(
            Arc::new(ProjectCache::default()),
            command_cache(corepack_bin.path(), &["corepack"]),
            Arc::new(RwLock::new(HistoryIndex::default())),
        ));
        assert!(
            available
                .complete(&context)
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "corepack pnpm build")
        );
    }

    #[test]
    fn fish_command_modifiers_preserve_project_script_completion() {
        let directory = tempfile::tempdir().expect("project");
        fs::write(
            directory.path().join("package.json"),
            r#"{"scripts":{"dev":"vite"}}"#,
        )
        .expect("manifest");
        let context_for = |shell, text: &str| {
            CompletionContext::new(
                QueryId::new(1),
                shell,
                directory.path().to_owned(),
                BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
                    .expect("buffer"),
            )
            .expect("context")
        };
        let mut engine = CompletionEngine::new(100, 12);
        engine.register(ProjectProvider::new(
            Arc::new(ProjectCache::default()),
            command_cache(directory.path(), &["pnpm"]),
            Arc::new(RwLock::new(HistoryIndex::default())),
        ));

        let fish = engine.complete(&context_for(ShellKind::Fish, "not pnpm de"));
        assert!(
            fish.candidates
                .iter()
                .any(|candidate| candidate.display.primary == "not pnpm dev")
        );
        assert!(
            engine
                .complete(&context_for(ShellKind::Bash, "not pnpm de"))
                .candidates
                .is_empty(),
            "fish-only modifiers must not alter Bash command parsing"
        );
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
        assert!(
            engine
                .complete(&context_for("npm install "))
                .candidates
                .is_empty(),
            "a completed native command must not restart top-level npm suggestions"
        );
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
        assert_eq!(candidate.display.primary, "pnpm 'build docs'");
        assert_eq!(
            candidate.edit.as_ref().expect("edit").replacement,
            "'build docs'"
        );
        assert!(
            output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "pnpm install"),
            "direct-script managers should also expose their native commands"
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
    fn manager_flag_prefixes_complete_and_exact_choices_stay_quiet() {
        let (_directory, _, engine) = bare_prefix_setup("pnpm --f");
        let context_for = |text: &str| {
            CompletionContext::new(
                QueryId::new(1),
                ShellKind::Zsh,
                PathBuf::from("/tmp"),
                BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
                    .expect("buffer"),
            )
            .expect("context")
        };

        for (typed, expected) in [
            ("pnpm --f", "pnpm --filter"),
            ("pnpm run --if", "pnpm run --if-present"),
            ("npm --work", "npm --workspace"),
            ("deno --conf", "deno --config"),
        ] {
            let output = engine.complete(&context_for(typed));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == expected),
                "missing {expected:?} for {typed:?}: {:?}",
                output
                    .candidates
                    .iter()
                    .map(|candidate| candidate.display.primary.as_str())
                    .collect::<Vec<_>>()
            );
        }

        for complete in ["pnpm --filter", "pnpm install"] {
            assert!(
                engine
                    .complete(&context_for(complete))
                    .candidates
                    .is_empty(),
                "completed choice leaked fuzzy siblings for {complete:?}"
            );
        }
    }

    #[test]
    fn other_subcommands_do_not_fire_script_completion() {
        for buffer in ["pnpm install", "pnpm install ", "pnpm install vit"] {
            let (_directory, context, engine) = bare_prefix_setup(buffer);
            let output = engine.complete(&context);
            assert!(
                output.candidates.is_empty(),
                "completed native command leaked rows for {buffer:?}: {:?}",
                output
                    .candidates
                    .iter()
                    .map(|candidate| candidate.display.primary.as_str())
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn direct_script_names_do_not_override_reserved_native_commands() {
        let directory = tempfile::tempdir().expect("project");
        fs::write(
            directory.path().join("package.json"),
            r#"{"scripts":{"install":"echo custom","dev":"vite"}}"#,
        )
        .expect("manifest");
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
            command_cache(directory.path(), &["pnpm"]),
            Arc::new(RwLock::new(HistoryIndex::default())),
        ));

        let direct = engine.complete(&context_for("pnpm ins"));
        assert_eq!(direct.candidates.len(), 1);
        assert_eq!(direct.candidates[0].display.primary, "pnpm install");
        assert_eq!(direct.candidates[0].display.description, "安装项目依赖");

        let explicit = engine.complete(&context_for("pnpm run ins"));
        assert_eq!(explicit.candidates.len(), 1);
        assert_eq!(explicit.candidates[0].display.primary, "pnpm run install");
        assert_eq!(explicit.candidates[0].display.description, "echo custom");
    }

    #[test]
    fn native_manager_commands_require_a_runnable_manager() {
        let directory = tempfile::tempdir().expect("project");
        let context = CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from(directory.path()),
            BufferSnapshot::new("pnpm ins", 8, BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer"),
        )
        .expect("context");
        let mut unavailable = CompletionEngine::new(100, 12);
        unavailable.register(ProjectProvider::new(
            Arc::new(ProjectCache::default()),
            Arc::new(CommandPathCache::default()),
            Arc::new(RwLock::new(HistoryIndex::default())),
        ));
        assert!(unavailable.complete(&context).candidates.is_empty());

        let mut available = CompletionEngine::new(100, 12);
        available.register(ProjectProvider::new(
            Arc::new(ProjectCache::default()),
            command_cache(directory.path(), &["pnpm"]),
            Arc::new(RwLock::new(HistoryIndex::default())),
        ));
        let output = available.complete(&context);
        assert_eq!(output.candidates[0].display.primary, "pnpm install");
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
            command_cache(directory.path(), &["deno"]),
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
            command_cache(directory.path(), &["pnpm"]),
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
        assert_eq!(&names[..2], ["pnpm dev", "pnpm build"]);
        assert!(names.contains(&"pnpm install"));
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
            r#"{"name":"@acme/web","scripts":{"dev":"vite dev","install":"echo custom"}}"#,
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
            command_cache(root.path(), &["pnpm", "npm", "yarn"]),
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
        let output = engine.complete(&context_for("pnpm --filter=@acme/w"));
        let member = output.candidates.first().expect("attached filter member");
        assert_eq!(member.display.primary, "@acme/web");
        assert_eq!(
            member.edit.as_ref().expect("edit").replacement,
            "--filter=@acme/web"
        );
        // After the member: native commands plus that member's own scripts.
        let output = engine.complete(&context_for("pnpm --filter @acme/web "));
        let names: Vec<_> = output
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect();
        assert!(names.contains(&"pnpm --filter @acme/web dev"));
        assert!(names.contains(&"pnpm --filter @acme/web install"));
        let dev = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "pnpm --filter @acme/web dev")
            .expect("filtered dev script");
        let edit = dev.edit.as_ref().expect("edit");
        assert_eq!(edit.replacement, "dev");

        let output = engine.complete(&context_for("pnpm --filter @acme/web ins"));
        assert_eq!(output.candidates.len(), 1);
        assert_eq!(output.candidates[0].display.description, "安装项目依赖");
        let output = engine.complete(&context_for("pnpm --filter @acme/web run ins"));
        assert_eq!(output.candidates.len(), 1);
        assert_eq!(output.candidates[0].display.description, "echo custom");

        // Explicit `run` stays visible in the row but only the active script
        // token is replaced once the keyword has been completed.
        let output = engine.complete(&context_for("pnpm --filter @acme/web run de"));
        assert_eq!(
            output.candidates[0].display.primary,
            "pnpm --filter @acme/web run dev"
        );
        assert_eq!(
            output.candidates[0]
                .edit
                .as_ref()
                .expect("edit")
                .replacement,
            "dev"
        );
        let output = engine.complete(&context_for("pnpm run -F @acme/web de"));
        assert_eq!(
            output.candidates[0].display.primary,
            "pnpm run -F @acme/web dev"
        );
        assert_eq!(
            output.candidates[0]
                .edit
                .as_ref()
                .expect("edit")
                .replacement,
            "dev"
        );

        // npm exposes native commands and requires `run` only for arbitrary
        // package scripts; yarn mixes native commands with direct scripts.
        let output = engine.complete(&context_for("npm --workspace @acme/web ru"));
        assert_eq!(
            output.candidates[0].display.primary,
            "npm --workspace @acme/web run"
        );
        assert_eq!(
            output.candidates[0]
                .edit
                .as_ref()
                .expect("edit")
                .replacement,
            "run"
        );
        let output = engine.complete(&context_for("npm --workspace @acme/web ins"));
        assert_eq!(
            output.candidates[0].display.primary,
            "npm --workspace @acme/web install"
        );
        let output = engine.complete(&context_for("npm --workspace @acme/web run de"));
        assert_eq!(
            output.candidates[0].display.primary,
            "npm --workspace @acme/web run dev"
        );
        let output = engine.complete(&context_for("npm run --workspace @acme/web de"));
        assert_eq!(
            output.candidates[0].display.primary,
            "npm run --workspace @acme/web dev"
        );
        assert_eq!(
            output.candidates[0]
                .edit
                .as_ref()
                .expect("edit")
                .replacement,
            "dev"
        );
        let output = engine.complete(&context_for(
            "npm --workspace @acme/web run --if-present de",
        ));
        assert_eq!(
            output.candidates[0].display.primary,
            "npm --workspace @acme/web run --if-present dev"
        );
        let output = engine.complete(&context_for("yarn workspace @acme/web de"));
        assert_eq!(
            output.candidates[0].display.primary,
            "yarn workspace @acme/web dev"
        );
        let output = engine.complete(&context_for("yarn workspace @acme/web ad"));
        assert_eq!(
            output.candidates[0].display.primary,
            "yarn workspace @acme/web add"
        );

        // Repeating selectors targets more than one workspace. Until the
        // selected set is modeled as a set, stay quiet instead of borrowing
        // scripts from only the last selector and presenting them as valid
        // for the whole invocation.
        for text in [
            "pnpm --filter @acme/api --filter @acme/web de",
            "npm --workspace @acme/api --workspace @acme/web run de",
        ] {
            assert!(
                engine.complete(&context_for(text)).candidates.is_empty(),
                "multiple selectors leaked a single-member script for {text:?}"
            );
        }
    }

    #[test]
    fn manager_directory_options_load_the_selected_project() {
        let root = tempfile::tempdir().expect("root");
        fs::create_dir(root.path().join("app")).expect("app");
        fs::write(
            root.path().join("package.json"),
            r#"{"scripts":{"root-only":"echo root"}}"#,
        )
        .expect("root manifest");
        fs::write(
            root.path().join("app/package.json"),
            r#"{"scripts":{"inside":"echo app"}}"#,
        )
        .expect("app manifest");
        let context_for = |text: &str| {
            CompletionContext::new(
                QueryId::new(1),
                ShellKind::Zsh,
                root.path().to_owned(),
                BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
                    .expect("buffer"),
            )
            .expect("context")
        };
        let mut engine = CompletionEngine::new(100, 12);
        engine.register(ProjectProvider::new(
            Arc::new(ProjectCache::default()),
            command_cache(root.path(), &["pnpm", "npm", "yarn"]),
            Arc::new(RwLock::new(HistoryIndex::default())),
        ));

        for text in [
            "pnpm -C app run in",
            "npm --prefix app run in",
            "yarn --cwd app in",
            "env -C app pnpm run in",
            "sudo -D app pnpm run in",
        ] {
            let output = engine.complete(&context_for(text));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary.ends_with("inside")),
                "selected project script missing for {text:?}"
            );
            assert!(
                !output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary.contains("root-only")),
                "root script leaked for {text:?}"
            );
        }
        assert!(
            engine
                .complete(&context_for("pnpm -C "))
                .candidates
                .is_empty()
        );
    }

    #[test]
    fn workspace_root_and_recursive_modes_use_the_correct_manifests() {
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
            r#"{"scripts":{"root-only":"echo root","common":"echo root common"}}"#,
        )
        .expect("root manifest");
        fs::write(
            root.path().join("packages/api/package.json"),
            r#"{"name":"api","scripts":{"api-only":"echo api","common":"echo api common"}}"#,
        )
        .expect("api manifest");
        fs::write(
            root.path().join("packages/web/package.json"),
            r#"{"name":"web","scripts":{"web-only":"echo web","common":"echo web common"}}"#,
        )
        .expect("web manifest");

        let cwd = root
            .path()
            .join("packages/api")
            .canonicalize()
            .expect("canonical member");
        let context_for = |text: &str| {
            CompletionContext::new(
                QueryId::new(1),
                ShellKind::Zsh,
                cwd.clone(),
                BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
                    .expect("buffer"),
            )
            .expect("context")
        };
        let mut engine = CompletionEngine::new(100, 12);
        engine.register(ProjectProvider::new(
            Arc::new(ProjectCache::default()),
            command_cache(root.path(), &["pnpm", "npm"]),
            Arc::new(RwLock::new(HistoryIndex::default())),
        ));
        let names = |text: &str| {
            engine
                .complete(&context_for(text))
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect::<Vec<_>>()
        };

        let root_rows = names("pnpm -w run ");
        assert!(root_rows.iter().any(|row| row.ends_with("root-only")));
        assert!(!root_rows.iter().any(|row| row.ends_with("api-only")));

        let recursive_rows = names("pnpm -r run ");
        assert!(recursive_rows.iter().any(|row| row.ends_with("api-only")));
        assert!(recursive_rows.iter().any(|row| row.ends_with("web-only")));
        assert!(!recursive_rows.iter().any(|row| row.ends_with("root-only")));

        let included_rows = names("pnpm -r --include-workspace-root run ");
        assert!(included_rows.iter().any(|row| row.ends_with("root-only")));

        let npm_rows = names("npm --workspaces run ");
        assert!(npm_rows.iter().any(|row| row.ends_with("common")));
        assert!(!npm_rows.iter().any(|row| row.ends_with("api-only")));
        assert!(!npm_rows.iter().any(|row| row.ends_with("web-only")));

        let npm_optional_rows = names("npm --workspaces --if-present run ");
        assert!(
            npm_optional_rows
                .iter()
                .any(|row| row.ends_with("api-only"))
        );
        assert!(
            npm_optional_rows
                .iter()
                .any(|row| row.ends_with("web-only"))
        );

        let npm_root_rows = names("npm --workspaces --include-workspace-root run ");
        assert!(npm_root_rows.iter().any(|row| row.ends_with("common")));
        assert!(!npm_root_rows.iter().any(|row| row.ends_with("root-only")));
        let npm_optional_root_rows =
            names("npm --workspaces --include-workspace-root --if-present run ");
        assert!(
            npm_optional_root_rows
                .iter()
                .any(|row| row.ends_with("root-only"))
        );

        let common = engine
            .complete(&context_for("pnpm -r run co"))
            .candidates
            .into_iter()
            .find(|candidate| candidate.display.primary.ends_with("common"))
            .expect("common recursive script");
        assert_eq!(common.display.description, "2 个 workspace 定义此脚本");
        assert!(
            engine
                .complete(&context_for("pnpm -r run common "))
                .candidates
                .is_empty()
        );
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
        for buffer in [
            "make",
            "make -j4",
            "make -C app",
            "make -f ",
            "make -f Mak",
            "make -fCustom",
            "make --file=Custom",
            "make build extra",
            "make > ",
        ] {
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

        // GNU make's `-j` has an optional attached argument. A separate word
        // is a target, not the jobs value.
        let (_directory, context, engine) =
            rule_file_setup("Makefile", MAKEFILE, "make", "make -j bu");
        let output = engine.complete(&context);
        assert_eq!(output.candidates[0].display.primary, "make build");

        let (_directory, context, engine) =
            rule_file_setup("Makefile", MAKEFILE, "make", "make MODE=release bu");
        let output = engine.complete(&context);
        assert_eq!(output.candidates[0].display.primary, "make build");
    }

    #[test]
    fn rule_directory_options_load_targets_from_the_selected_directory() {
        let directory = tempfile::tempdir().expect("project");
        fs::write(directory.path().join("Makefile"), "root-only:\n").expect("root makefile");
        let app = directory.path().join("app");
        let nested = app.join("nested");
        fs::create_dir_all(&nested).expect("nested project");
        fs::write(app.join("Makefile"), "inside:\n").expect("app makefile");
        fs::write(nested.join("Makefile"), "deep:\n").expect("nested makefile");
        fs::write(app.join("justfile"), "serve:\n    echo serve\n").expect("app justfile");

        let bin = directory.path().join("bin");
        fs::create_dir(&bin).expect("bin");
        for tool in ["make", "just"] {
            let tool_path = bin.join(tool);
            fs::write(&tool_path, b"#!/bin/sh\n").expect("fake tool");
            fs::set_permissions(&tool_path, fs::Permissions::from_mode(0o700)).expect("tool mode");
        }
        let cwd = directory.path().canonicalize().expect("canonical cwd");
        let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(&bin))));
        let context_for = |buffer: &str| {
            CompletionContext::new(
                QueryId::new(1),
                ShellKind::Zsh,
                cwd.clone(),
                BufferSnapshot::new(
                    buffer,
                    buffer.len(),
                    BufferRevision::new(1),
                    SyncQuality::Exact,
                )
                .expect("buffer"),
            )
            .expect("context")
        };
        let mut engine = CompletionEngine::new(100, 12);
        engine.register(ProjectProvider::new(
            Arc::new(ProjectCache::default()),
            commands,
            Arc::new(RwLock::new(HistoryIndex::default())),
        ));

        for buffer in [
            "make -C app in",
            "make -Capp ",
            "make --directory app ",
            "make --directory=app ",
        ] {
            let output = engine.complete(&context_for(buffer));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "make inside"),
                "selected target missing for {buffer:?}"
            );
            assert!(
                !output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "make root-only"),
                "root target leaked for {buffer:?}"
            );
        }

        let output = engine.complete(&context_for("make -C app -C nested "));
        assert_eq!(output.candidates[0].display.primary, "make deep");

        for buffer in ["just -d app ", "just --working-directory=app "] {
            let output = engine.complete(&context_for(buffer));
            assert_eq!(output.candidates[0].display.primary, "just serve");
        }
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
