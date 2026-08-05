use std::sync::Arc;

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderDiagnostic, ProviderOutput,
        TextEdit,
    },
    parser::{QuoteContext, escape_for_shell},
    platform::CommandPathCache,
    project::{MakefileCache, ManifestKind, ProjectCache, discover_makefile},
    terminal::RiskLevel,
};

pub struct ProjectProvider {
    cache: Arc<ProjectCache>,
    makefiles: MakefileCache,
    commands: Arc<CommandPathCache>,
}

impl ProjectProvider {
    #[must_use]
    pub fn new(cache: Arc<ProjectCache>, commands: Arc<CommandPathCache>) -> Self {
        Self {
            cache,
            makefiles: MakefileCache::default(),
            commands,
        }
    }
}

impl CandidateProvider for ProjectProvider {
    fn id(&self) -> &'static str {
        "project"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        if package_manager(context).is_some_and(|(manager, _)| self.commands.contains(manager)) {
            return true;
        }
        rule_file_tool(context).is_some_and(|tool| {
            self.commands.contains(tool)
                && discover_makefile(&context.cwd, ManifestKind::for_tool(tool)).is_some()
        })
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        if package_manager(context).is_some() {
            return self.complete_scripts(context);
        }
        if rule_file_tool(context).is_some() {
            return self.complete_targets(context);
        }
        ProviderOutput::default()
    }
}

impl ProjectProvider {
    fn complete_scripts(&self, context: &CompletionContext) -> ProviderOutput {
        let Some((manager, insert_run)) = package_manager(context) else {
            return ProviderOutput::default();
        };
        let manifest = match self.cache.load_nearest(&context.cwd) {
            Ok(Some(manifest)) => manifest,
            Ok(None) => return ProviderOutput::default(),
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
        };
        let relative = manifest
            .path
            .strip_prefix(context.cwd.as_ref())
            .unwrap_or(&manifest.path)
            .display()
            .to_string();
        let candidates = manifest
            .scripts
            .iter()
            .map(|(name, script)| {
                let escaped = escape_for_shell(name, QuoteContext::Unquoted, context.shell);
                // Bare `pnpm <prefix>` fills `run <script>` into the active
                // word so every manager ends up with the explicit run form;
                // `pnpm run <prefix>` only replaces the script token.
                let replacement = if insert_run {
                    format!("run {escaped}")
                } else {
                    escaped
                };
                let mut candidate = Candidate::new(
                    context.query_id,
                    format!("{manager} run {name}"),
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
                    format!("project:{relative}:{name}"),
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

/// Matches the package-manager script position: returns the manager and
/// whether the fill must insert the `run` keyword itself. Fires both on
/// `pnpm run <prefix>` (script token only) and on a bare `pnpm <prefix>` so
/// package.json scripts mix into the list alongside history rows — the
/// explicit `run` form works for pnpm, npm, yarn, and bun alike.
fn package_manager(context: &CompletionContext) -> Option<(&str, bool)> {
    let words = segment_words(context);
    let manager = *words.first()?;
    if !matches!(manager, "pnpm" | "npm" | "yarn" | "bun") {
        return None;
    }
    let trailing_space = context.buffer.text[..context.buffer.cursor]
        .chars()
        .next_back()
        .is_some_and(char::is_whitespace);
    match words.as_slice() {
        // The script position: `pnpm run <prefix>` — replace only the script
        // token. A bare `run` word being typed is NOT this position.
        [_, "run", ..] if trailing_space || words.len() > 2 => Some((manager, false)),
        // Bare `pnpm <prefix>` (or `pnpm run` still being typed): fill
        // `run <script>` into the active word.
        [_] | [_, _] => Some((manager, true)),
        _ => None,
    }
}

/// Matches the `make`/`just` first-argument position: the tool word alone
/// (`make `) or one target word being typed (`make bu`). Deeper words and
/// flag positions (`make -f <value>`, `make -j4 bu`) are left to other
/// providers.
fn rule_file_tool(context: &CompletionContext) -> Option<&'static str> {
    let words = segment_words(context);
    let tool = match words.first() {
        Some(&"make") => "make",
        Some(&"just") => "just",
        _ => return None,
    };
    match words.as_slice() {
        [_] => Some(tool),
        [_, second] if !second.starts_with('-') => Some(tool),
        _ => None,
    }
}

/// Word tokens of the active pipeline segment up to the cursor.
fn segment_words(context: &CompletionContext) -> Vec<&str> {
    context
        .parsed
        .tokens
        .iter()
        .filter(|token| {
            token.kind == crate::parser::TokenKind::Word
                && token.range.start >= context.parsed.active_segment.start
                && token.range.start <= context.buffer.cursor
        })
        .map(|token| token.cooked_prefix.as_str())
        .collect()
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
        ));
        (directory, context, engine)
    }

    #[test]
    fn bare_manager_prefix_inserts_the_run_keyword() {
        // `pnpm bu` mixes package.json scripts into the list; accepting one
        // rewrites the active word to the explicit `run <script>` form.
        let (_directory, context, engine) = bare_prefix_setup("pnpm bu");
        let output = engine.complete(&context);
        let candidate = output.candidates.first().expect("script candidate");
        assert_eq!(candidate.display.primary, "pnpm run build docs");
        let edit = candidate.edit.as_ref().expect("edit");
        assert_eq!(edit.range, 5..7);
        assert_eq!(edit.replacement, "run 'build docs'");
    }

    #[test]
    fn trailing_space_after_manager_offers_scripts() {
        let (_directory, context, engine) = bare_prefix_setup("pnpm ");
        let output = engine.complete(&context);
        let candidate = output.candidates.first().expect("script candidate");
        assert_eq!(candidate.display.primary, "pnpm run build docs");
        assert_eq!(
            candidate.edit.as_ref().expect("edit").replacement,
            "run 'build docs'"
        );
    }

    #[test]
    fn run_word_being_typed_keeps_the_run_keyword() {
        // Cursor still on `run`: the fill must produce `pnpm run <script>`,
        // not drop the keyword.
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
        for buffer in ["make -f ", "make -f Mak", "make -j4 bu", "make build extra"] {
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
        ));
        assert!(engine.complete(&context).candidates.is_empty());
    }
}
