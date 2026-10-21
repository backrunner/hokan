use std::{fs, sync::Arc, time::Instant};

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderDiagnostic, ProviderOutput,
        SlotKind, TextEdit,
    },
    config::CdEnterBehavior,
    parser::escape_for_shell,
    providers::{CommandHelpCache, argument_progress},
    shell::{AliasCache, AliasKind},
    specs::SpecRegistry,
    terminal::RiskLevel,
};

const MAX_DIRECTORY_ENTRIES: usize = 5_000;
const DIRECTORY_BUDGET_MS: u128 = 80;

mod recovery;
use super::argument::FilesystemSlot;

pub struct FilesystemProvider {
    show_hidden: bool,
    cd_enter_behavior: CdEnterBehavior,
    specs: Arc<SpecRegistry>,
    help: Arc<CommandHelpCache>,
    aliases: Arc<AliasCache>,
}

impl FilesystemProvider {
    #[must_use]
    pub fn new(
        show_hidden: bool,
        specs: Arc<SpecRegistry>,
        help: Arc<CommandHelpCache>,
        aliases: Arc<AliasCache>,
    ) -> Self {
        Self {
            show_hidden,
            cd_enter_behavior: CdEnterBehavior::default(),
            specs,
            help,
            aliases,
        }
    }

    #[must_use]
    pub fn with_cd_enter_behavior(mut self, behavior: CdEnterBehavior) -> Self {
        self.cd_enter_behavior = behavior;
        self
    }
}

impl CandidateProvider for FilesystemProvider {
    fn id(&self) -> &'static str {
        "filesystem"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        let Some(path) = context.argument.local_path() else {
            return false;
        };
        if self.inferred_function_argument(context)
            || super::command_help::dynamic_help_owns_position(context, &self.help)
        {
            return false;
        }
        !(self
            .specs
            .get(context.command().unwrap_or_default())
            .is_some()
            && context.parsed.current_prefix.is_empty()
            && path.defer_to_recipes)
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        if !self.applies(context) {
            return ProviderOutput::default();
        }
        let Some(request) = context.argument.local_path() else {
            return ProviderOutput::default();
        };
        let slot = super::argument::FilesystemSlot {
            defer_to_recipes: request.defer_to_recipes,
            kind: request.kind,
            prefix: &request.prefix,
            edit_prefix: &request.edit_prefix,
            base_dir: Some(request.base_dir.clone()),
        };
        let prefix = slot.prefix;
        let (directory_prefix, basename) = split_prefix(prefix);
        let literal_root = request.literal_root;
        let scan_directory = &request.directory;
        let started = Instant::now();
        let entries = match fs::read_dir(scan_directory) {
            Ok(entries) => entries,
            Err(error) => {
                return recovery::read_error(self, context, &slot, &error, started, literal_root);
            }
        };
        let file_next_slot = request.file_next_slot;
        let execute_cd = self.cd_enter_behavior == CdEnterBehavior::Execute && request.cd_target;
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
            let path = entry.path();
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if (!show_hidden && name.starts_with('.')) || name.contains(['\0', '\n', '\r']) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let metadata = entry.metadata().ok();
            if metadata.is_none() && !file_type.is_symlink() {
                continue;
            }
            let directory =
                metadata.as_ref().is_some_and(fs::Metadata::is_dir) || file_type.is_dir();
            if !slot_accepts(slot.kind, &path, directory) {
                continue;
            }
            let mut logical = format!("{directory_prefix}{name}");
            // A leading dash makes the path look like a flag (`cd -foo/`);
            // prefix `./` for directories and files alike.
            if directory_prefix.is_empty() && logical.starts_with('-') {
                logical.insert_str(0, "./");
            }
            if directory {
                logical.push('/');
            }
            // The edit replaces the complete raw token, including any open quote.
            let escaped = escape_path_with_root(&logical, context.shell, literal_root);
            let replacement = format!("{}{}", slot.edit_prefix, escaped);
            if replacement.is_empty() {
                continue;
            }
            let executable = crate::platform::is_executable(&path);
            let next_slot = if directory {
                (!execute_cd).then_some(SlotKind::Path)
            } else {
                file_next_slot
            };
            let mut candidate = Candidate::new(
                context.query_id,
                &logical,
                if directory && execute_cd {
                    "切换到此目录"
                } else if directory {
                    "进入目录继续补全"
                } else if matches!(slot.kind, SlotKind::Executable) {
                    "当前目录中的可执行文件"
                } else {
                    "当前目录中的文件"
                },
                Some(TextEdit {
                    range: context.parsed.replacement.clone(),
                    replacement,
                    cursor_after: CursorPlacement::End,
                }),
                next_slot.map_or(CandidateAction::Insert, |next_slot| {
                    CandidateAction::InsertAndContinue { next_slot }
                }),
                CandidateSource::Filesystem,
                if directory {
                    CandidateKind::Directory
                } else {
                    CandidateKind::File
                },
                next_slot.map_or(Completeness::Runnable, |slot| Completeness::NeedsInput {
                    slot,
                }),
                RiskLevel::Low,
                format!("fs:{}", path.display()),
            );
            candidate.score.cwd_affinity = 100;
            candidate.score.spec_priority = match (slot.kind, directory, executable) {
                (SlotKind::Executable, false, true) => 120,
                (SlotKind::Executable, true, _) => 60,
                // Descending is the norm at path-like slots: keep
                // directories level with files instead of sinking them
                // below the siblings the user drills through.
                (SlotKind::Path | SlotKind::Directory | SlotKind::NewFile, true, _) => 80,
                (_, true, _) => 50,
                _ => 80,
            };
            candidates.push(candidate);
        }
        recovery::repair_basename(context, &slot, &mut candidates, started);
        ProviderOutput {
            candidates,
            diagnostics: partial
                .then_some(ProviderDiagnostic {
                    provider: self.id(),
                    code: "HK-FS-002",
                    level: crate::completion::DiagnosticLevel::Info,
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

fn directory_read_error(directory: &std::path::Path, error: &std::io::Error) -> String {
    let reason = match error.kind() {
        std::io::ErrorKind::PermissionDenied => "无权访问目录，请检查访问权限".to_owned(),
        std::io::ErrorKind::NotFound => "找不到目录".to_owned(),
        std::io::ErrorKind::NotADirectory => "此路径是文件，不是目录".to_owned(),
        _ => "暂时无法读取目录，请稍后重试".to_owned(),
    };
    format!("{reason} · {}", directory.display())
}

impl FilesystemProvider {
    fn inferred_function_argument(&self, context: &CompletionContext) -> bool {
        if !super::effective_command_is_shell_command(context)
            || argument_progress(context).is_none_or(|(_, position)| position != 0)
        {
            return false;
        }
        let Some(command) = context.command() else {
            return false;
        };
        let aliases = self.aliases.load(context.shell);
        let Some(entry) = aliases.get(command) else {
            return false;
        };
        entry.kind == AliasKind::Function
            && entry.body.as_deref().is_some_and(|body| {
                crate::shell::infer_function_slot(context.shell, body).is_some()
            })
    }
}

pub(super) fn split_prefix(prefix: &str) -> (&str, &str) {
    prefix
        .rfind('/')
        .map_or(("", prefix), |index| prefix.split_at(index + 1))
}

/// Keep a known shell expansion marker outside quotes. Quoting the entire
/// value (`'~/Projects'`, `'$HOME/Projects'`) turns the marker into a literal;
/// quoting only the candidate-controlled remainder is safe and expandable.
pub(super) fn escape_path_for_shell(value: &str, shell: crate::shell::ShellKind) -> String {
    for marker in ["~/", "$HOME/", "${HOME}/", "$PWD/", "${PWD}/"] {
        if let Some(rest) = value.strip_prefix(marker) {
            return format!(
                "{marker}{}",
                escape_for_shell(rest, crate::parser::QuoteContext::Unquoted, shell)
            );
        }
    }
    escape_for_shell(value, crate::parser::QuoteContext::Unquoted, shell)
}

fn escape_path_with_root(
    value: &str,
    shell: crate::shell::ShellKind,
    literal_root: bool,
) -> String {
    if literal_root {
        escape_for_shell(value, crate::parser::QuoteContext::Unquoted, shell)
    } else {
        escape_path_for_shell(value, shell)
    }
}

pub(super) fn scan_directory_with_root(
    cwd: &std::path::Path,
    prefix: &str,
    literal_root: bool,
) -> std::path::PathBuf {
    if literal_root {
        cwd.join(prefix)
    } else {
        scan_directory_for(cwd, prefix)
    }
}

/// The directory to scan for a typed path prefix. Known home/PWD expansions
/// are resolved for scanning while their literal spelling is kept in edits.
pub(super) fn scan_directory_for(
    cwd: &std::path::Path,
    directory_prefix: &str,
) -> std::path::PathBuf {
    if directory_prefix.is_empty() {
        return cwd.to_owned();
    }
    if let Some(rest) = directory_prefix.strip_prefix("~/")
        && let Some(home) = std::env::home_dir()
    {
        return home.join(rest);
    }
    if let Some(rest) = directory_prefix
        .strip_prefix("$HOME/")
        .or_else(|| directory_prefix.strip_prefix("${HOME}/"))
        && let Some(home) = std::env::home_dir()
    {
        return home.join(rest);
    }
    if let Some(rest) = directory_prefix
        .strip_prefix("$PWD/")
        .or_else(|| directory_prefix.strip_prefix("${PWD}/"))
    {
        return cwd.join(rest);
    }
    cwd.join(directory_prefix)
}

fn slot_accepts(slot: SlotKind, path: &std::path::Path, directory: bool) -> bool {
    match slot {
        SlotKind::Directory => directory,
        SlotKind::Executable => directory || crate::platform::is_executable(path),
        SlotKind::File => true,
        SlotKind::Path => true,
        SlotKind::NewFile => directory,
        SlotKind::Process | SlotKind::Interface | SlotKind::Port | SlotKind::Value => false,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        os::unix::fs::{PermissionsExt, symlink},
        path::PathBuf,
        time::Duration,
    };

    use super::*;
    use crate::{
        completion::{BufferSnapshot, CompletionEngine, SyncQuality},
        providers::command_help::{CommandHelp, HelpEntry},
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };

    #[test]
    fn directory_failures_explain_the_cause_before_the_path() {
        let path = std::path::Path::new("/private/example");
        for (kind, cause) in [
            (std::io::ErrorKind::PermissionDenied, "无权访问目录"),
            (std::io::ErrorKind::NotFound, "找不到目录"),
            (std::io::ErrorKind::NotADirectory, "此路径是文件，不是目录"),
        ] {
            let message = directory_read_error(path, &std::io::Error::from(kind));
            assert!(message.starts_with(cause), "{message}");
            assert!(message.ends_with("/private/example"), "{message}");
        }
        let message = directory_read_error(path, &std::io::Error::other("device unavailable"));
        assert!(message.contains("暂时无法读取目录"));
        assert!(message.contains("稍后重试"));
    }

    #[test]
    fn bash_prefers_scripts_and_escapes_spaces() {
        let directory = tempfile::tempdir().expect("directory");
        fs::File::create(directory.path().join("hello world.sh"))
            .expect("script")
            .write_all(b"echo ok\n")
            .expect("write script");
        fs::write(directory.path().join("script"), b"echo plain\n").expect("non-executable script");
        fs::create_dir(directory.path().join("nested")).expect("nested directory");
        let backslash_directory = directory.path().join(r"dir\name");
        fs::create_dir(&backslash_directory).expect("backslash directory");
        fs::write(backslash_directory.join("run.sh"), b"echo quoted\n").expect("nested script");
        let context = CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from(directory.path()),
            BufferSnapshot::new("bash ", 5, BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer"),
        )
        .expect("context");
        let mut engine = CompletionEngine::new(100, 20);
        engine.register(provider(Arc::new(SpecRegistry::default())));
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
        assert!(
            output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "script"),
            "shell script files do not need an executable bit or extension"
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

        let quoted_backslash = CompletionContext::new(
            QueryId::new(3),
            ShellKind::Zsh,
            PathBuf::from(directory.path()),
            BufferSnapshot::new(
                r"bash 'dir\name/ru",
                r"bash 'dir\name/ru".len(),
                BufferRevision::new(3),
                SyncQuality::Exact,
            )
            .expect("quoted backslash buffer"),
        )
        .expect("quoted backslash context");
        let output = engine.complete(&quoted_backslash);
        let candidate = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == r"dir\name/run.sh")
            .expect("backslash path candidate");
        assert_eq!(
            candidate.edit.as_ref().expect("edit").replacement,
            r"'dir\name/run.sh'"
        );
    }

    #[test]
    fn explicit_command_paths_complete_executables_and_preserve_wrappers() {
        let directory = tempfile::tempdir().expect("directory");
        let executable = directory.path().join("runner");
        fs::write(&executable, b"#!/bin/sh\n").expect("executable");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .expect("executable mode");
        fs::write(directory.path().join("regular.txt"), b"plain").expect("regular file");
        fs::write(directory.path().join("not-executable.sh"), b"#!/bin/sh\n")
            .expect("non-executable script");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in [
            "./ru",
            "sudo ./ru",
            "which ./ru",
            "command -v ./ru",
            "npx ./ru",
            "npx -w app ./ru",
            "npm exec -- ./ru",
            "npm exec --workspace app -- ./ru",
            "pnpm exec ./ru",
            "yarn exec ./ru",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            let runner = output
                .candidates
                .iter()
                .find(|candidate| candidate.display.primary == "./runner")
                .unwrap_or_else(|| panic!("explicit executable missing for {buffer:?}"));
            assert_eq!(runner.edit.as_ref().expect("edit").replacement, "./runner");
            assert!(
                !output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "./regular.txt")
            );
            assert!(
                !output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "./not-executable.sh")
            );
        }
    }

    #[test]
    fn tar_slots_distinguish_new_and_existing_archives() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("archive.tgz"), b"archive").expect("archive");
        fs::create_dir(directory.path().join("src")).expect("source directory");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in ["tar -czf ", "tar -c -f ", "tar cf "] {
            let create_output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                create_output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "src/"),
                "new archive directory missing for {buffer:?}"
            );
            assert!(
                !create_output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "archive.tgz"),
                "existing archive leaked into new-file slot for {buffer:?}"
            );
        }

        for buffer in ["tar -xzf ", "tar --extract --file "] {
            let extract_output = provider.complete(&context(directory.path(), buffer, 2));
            assert!(
                extract_output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "archive.tgz"),
                "input archive missing for {buffer:?}"
            );
        }

        let sources = provider.complete(&context(directory.path(), "tar -czf out.tgz sr", 3));
        assert!(
            sources
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "src/")
        );
        assert!(
            provider
                .complete(&context(directory.path(), "tar -xzf archive.tgz member", 4))
                .candidates
                .is_empty()
        );
    }

    #[test]
    fn tar_slots_cover_attached_values_chdir_chains_and_literal_values() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("archive.tgz"), b"archive").expect("archive");
        fs::write(directory.path().join("list.txt"), b"list").expect("list");
        fs::write(directory.path().join("exclude.txt"), b"exclude").expect("exclude");
        fs::create_dir(directory.path().join("archives")).expect("archives");
        fs::create_dir_all(directory.path().join("base/nested/source")).expect("nested source");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in ["tar -C ba", "tar -Cba", "tar --directory=ba"] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "base/"),
                "directory value missing for {buffer:?}"
            );
        }

        let chained = provider.complete(&context(
            directory.path(),
            "tar -czf out.tgz -C base -C nested so",
            2,
        ));
        assert!(
            chained
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "source/"),
            "consecutive -C values must resolve successively"
        );

        for buffer in ["tar cvzf ", "tar -c --file=ar"] {
            let output = provider.complete(&context(directory.path(), buffer, 3));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "archives/"),
                "new archive slot missing for {buffer:?}"
            );
            assert!(
                !output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "archive.tgz"),
                "existing archive leaked for {buffer:?}"
            );
        }

        let attached = provider.complete(&context(directory.path(), "tar -x -farc", 4));
        let archive = attached
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "archive.tgz")
            .expect("attached archive input");
        assert_eq!(
            archive.edit.as_ref().expect("edit").replacement,
            "-farchive.tgz"
        );

        for (buffer, expected) in [("tar -T li", "list.txt"), ("tar -X ex", "exclude.txt")] {
            let output = provider.complete(&context(directory.path(), buffer, 5));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == expected),
                "file-list value missing for {buffer:?}"
            );
        }

        for buffer in ["tar -c --owner ", "tar -c --format ", "tar -c -H "] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 6))
                    .candidates
                    .is_empty(),
                "literal tar value leaked filesystem rows for {buffer:?}"
            );
        }
        for buffer in ["tar -xf -", "tar -T -", "tar --file=host:archive"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 7))
                    .candidates
                    .is_empty(),
                "stdio or remote tar values must stay out of local filesystem rows: {buffer:?}"
            );
        }
    }

    #[test]
    fn five_thousand_entry_directory_returns_within_the_first_batch_budget() {
        let directory = tempfile::tempdir().expect("directory");
        for index in 0..5_000 {
            fs::write(directory.path().join(format!("entry-{index:04}")), b"")
                .expect("directory fixture");
        }
        let context = context(directory.path(), "cat ", 3);
        let provider = provider(Arc::new(SpecRegistry::default()));
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

    #[test]
    fn dash_prefixed_directories_get_a_dot_slash_prefix_like_files() {
        let directory = tempfile::tempdir().expect("directory");
        fs::create_dir(directory.path().join("-dashdir")).expect("dash directory");
        fs::write(directory.path().join("-dashfile"), b"").expect("dash file");
        let provider = provider(Arc::new(SpecRegistry::default()));
        let output = provider.complete(&context(directory.path(), "cd ", 1));
        assert!(
            output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "./-dashdir/")
        );
        let output = provider.complete(&context(directory.path(), "cat ", 2));
        assert!(
            output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "./-dashfile")
        );
    }

    #[test]
    fn dashed_active_word_produces_no_filesystem_candidates() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("-dash"), b"").expect("dash file");
        let provider = provider(Arc::new(SpecRegistry::default()));
        let output = provider.complete(&context(directory.path(), "cat -", 1));
        assert!(output.candidates.is_empty());
        assert!(!provider.applies(&context(directory.path(), "cat -d", 2)));
    }

    #[test]
    fn spec_covered_commands_own_the_fallback_path_slot() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"").expect("file");
        let provider = provider(Arc::new(SpecRegistry::load(None)));
        // `ls` is spec-covered: the fallback path slot is suppressed.
        assert!(
            provider
                .complete(&context(directory.path(), "ls ", 1))
                .candidates
                .is_empty()
        );
        // `cat` is not: file completion still works.
        assert!(
            provider
                .complete(&context(directory.path(), "cat ", 2))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "plain.txt")
        );
    }

    #[test]
    fn help_subcommands_suppress_files_at_the_real_top_level_command_slot() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"").expect("file");
        fs::write(directory.path().join("plain.swift"), b"").expect("swift file");
        let help = Arc::new(CommandHelpCache::default());
        help.seed(
            "git",
            CommandHelp {
                flags: vec![HelpEntry {
                    name: "-C".into(),
                    description: String::new(),
                    takes_value: true,
                }],
                subcommands: vec![HelpEntry {
                    name: "add".into(),
                    description: String::new(),
                    takes_value: false,
                }],
                subcommand_aliases: Vec::new(),
                accepts_positionals: false,
                subcommands_exhaustive: false,
            },
        );
        let provider = FilesystemProvider::new(
            false,
            Arc::new(SpecRegistry::load(None)),
            Arc::clone(&help),
            Arc::new(AliasCache::default()),
        );
        // Subcommand position with a positive help result: no file rows.
        assert!(
            provider
                .complete(&context(directory.path(), "git ", 1))
                .candidates
                .is_empty()
        );
        assert!(
            provider
                .complete(&context(directory.path(), "git -C . ", 2))
                .candidates
                .is_empty(),
            "global flag values must not shift the top-level subcommand slot"
        );
        // Past the first argument file completion still works.
        assert!(
            provider
                .complete(&context(directory.path(), "git -C . add ", 3))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "plain.txt")
        );
        help.seed(
            "swift",
            CommandHelp {
                flags: Vec::new(),
                subcommands: vec![HelpEntry {
                    name: "build".into(),
                    description: String::new(),
                    takes_value: false,
                }],
                subcommand_aliases: Vec::new(),
                accepts_positionals: false,
                subcommands_exhaustive: true,
            },
        );
        assert!(
            provider
                .complete(&context(directory.path(), "swift ", 30))
                .candidates
                .is_empty(),
            "SwiftPM subcommands own the empty slot"
        );
        assert!(
            provider
                .complete(&context(directory.path(), "swift b", 31))
                .candidates
                .is_empty(),
            "a matching SwiftPM subcommand prefix must suppress files"
        );
        assert!(
            provider
                .complete(&context(directory.path(), "swift pl", 32))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "plain.swift"),
            "a non-subcommand Swift prefix must fall back to source files"
        );
        // Help without subcommands (e.g. `cp`) never suppresses files.
        help.seed(
            "cp",
            CommandHelp {
                flags: vec![HelpEntry {
                    name: "-R".into(),
                    description: String::new(),
                    takes_value: false,
                }],
                subcommands: Vec::new(),
                subcommand_aliases: Vec::new(),
                accepts_positionals: false,
                subcommands_exhaustive: false,
            },
        );
        assert!(
            provider
                .complete(&context(directory.path(), "cp ", 4))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "plain.txt")
        );
    }

    #[test]
    fn cp_and_mv_first_file_candidates_require_a_destination() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("source.txt"), b"source").expect("source file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in [
            "cp ",
            "cp -R ",
            "cp -S .backup ",
            "cp -S -tdir ",
            "cp --no-preserve -tdir ",
            "cp --sparse -tdir ",
            "mv ",
            "mv --suffix=-tdir ",
            "sudo mv ",
        ] {
            let candidate = provider
                .complete(&context(directory.path(), buffer, 1))
                .candidates
                .into_iter()
                .find(|candidate| candidate.display.primary == "source.txt")
                .unwrap_or_else(|| panic!("source candidate missing for {buffer:?}"));
            assert_eq!(
                candidate.action,
                CandidateAction::InsertAndContinue {
                    next_slot: SlotKind::Path
                },
                "first operand must fill without executing for {buffer:?}"
            );
            assert_eq!(
                candidate.completeness,
                Completeness::NeedsInput {
                    slot: SlotKind::Path
                },
                "first operand must still require a destination for {buffer:?}"
            );
        }
    }

    #[test]
    fn cp_and_mv_destination_file_candidates_are_runnable() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("source.txt"), b"source").expect("source file");
        fs::write(directory.path().join("target.txt"), b"target").expect("target file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in [
            "cp source.txt ",
            "cp source.txt -R ",
            "cp -- -tdir ",
            "mv source.txt ",
            "sudo mv source.txt ",
        ] {
            let candidate = provider
                .complete(&context(directory.path(), buffer, 2))
                .candidates
                .into_iter()
                .find(|candidate| candidate.display.primary == "target.txt")
                .unwrap_or_else(|| panic!("destination candidate missing for {buffer:?}"));
            assert_eq!(
                candidate.action,
                CandidateAction::Insert,
                "destination file should complete the invocation for {buffer:?}"
            );
            assert_eq!(
                candidate.completeness,
                Completeness::Runnable,
                "destination file should be runnable for {buffer:?}"
            );
        }
    }

    #[test]
    fn cp_and_mv_target_directory_forms_need_only_a_source() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("source.txt"), b"source").expect("source file");
        fs::create_dir(directory.path().join("dest")).expect("destination directory");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in [
            "cp -t dest ",
            "cp --target-directory=dest -S .backup ",
            "mv -tdest ",
            "mv --target-directory dest ",
        ] {
            let candidate = provider
                .complete(&context(directory.path(), buffer, 3))
                .candidates
                .into_iter()
                .find(|candidate| candidate.display.primary == "source.txt")
                .unwrap_or_else(|| panic!("source candidate missing for {buffer:?}"));
            assert_eq!(
                candidate.action,
                CandidateAction::Insert,
                "target-directory form is complete after its first source for {buffer:?}"
            );
            assert_eq!(
                candidate.completeness,
                Completeness::Runnable,
                "target-directory form should be runnable for {buffer:?}"
            );
        }
    }

    #[test]
    fn cp_and_mv_non_path_option_values_do_not_offer_filesystem_rows() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("source.txt"), b"source").expect("source file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in [
            "cp -S ",
            "cp -S.backup",
            "cp --suffix ",
            "cp --no-preserve ",
            "cp --sparse=",
            "mv -S ",
            "mv --suffix=-backup",
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 4))
                    .candidates
                    .is_empty(),
                "non-path option value leaked filesystem rows for {buffer:?}"
            );
        }
    }

    #[test]
    fn shell_expansion_prefixes_scan_the_resolved_directory() {
        let cwd = tempfile::tempdir().expect("cwd");
        let home = std::env::home_dir().expect("home directory");
        assert_eq!(scan_directory_for(cwd.path(), "~/"), home.clone());
        assert_eq!(
            scan_directory_for(cwd.path(), "~/Documents/"),
            home.join("Documents")
        );
        assert_eq!(scan_directory_for(cwd.path(), "$HOME/"), home.clone());
        assert_eq!(
            scan_directory_for(cwd.path(), "${HOME}/Documents/"),
            home.join("Documents")
        );
        assert_eq!(scan_directory_for(cwd.path(), "$PWD/"), cwd.path());
        assert_eq!(
            scan_directory_for(cwd.path(), "${PWD}/src/"),
            cwd.path().join("src")
        );
        assert_eq!(
            scan_directory_for(cwd.path(), "src/"),
            cwd.path().join("src")
        );
        assert_eq!(scan_directory_for(cwd.path(), ""), cwd.path().to_owned());
    }

    #[test]
    fn path_edits_preserve_known_shell_expansions() {
        for shell in [ShellKind::Zsh, ShellKind::Bash, ShellKind::Fish] {
            assert_eq!(
                escape_path_for_shell("~/Projects/app/", shell),
                "~/Projects/app/"
            );
            assert_eq!(
                escape_path_for_shell("~/My Projects/app/", shell),
                "~/'My Projects/app/'"
            );
            assert_eq!(
                escape_path_for_shell("$HOME/My Projects/app/", shell),
                "$HOME/'My Projects/app/'"
            );
            assert_eq!(
                escape_path_for_shell("${PWD}/src/my file.rs", shell),
                "${PWD}/'src/my file.rs'"
            );
        }
    }

    #[test]
    fn pwd_prefixed_candidates_scan_and_fill_without_quoting_the_variable() {
        let cwd = tempfile::tempdir().expect("cwd");
        fs::create_dir(cwd.path().join("nested project")).expect("nested directory");
        let output = provider(Arc::new(SpecRegistry::default())).complete(&context(
            cwd.path(),
            "cd $PWD/n",
            1,
        ));
        let candidate = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "$PWD/nested project/")
            .expect("PWD-prefixed candidate");
        assert_eq!(
            candidate.edit.as_ref().expect("edit").replacement,
            "$PWD/'nested project/'"
        );
    }

    #[test]
    fn broken_symlinks_are_paths_but_not_executables_or_directories() {
        let directory = tempfile::tempdir().expect("directory");
        symlink("missing-target", directory.path().join("missing-link")).expect("broken symlink");
        let provider = provider(Arc::new(SpecRegistry::default()));

        let path_rows = provider
            .complete(&context(directory.path(), "cat m", 1))
            .candidates;
        assert!(
            path_rows
                .iter()
                .any(|candidate| candidate.display.primary == "missing-link")
        );

        for buffer in ["cd m", "./m"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 2))
                    .candidates
                    .iter()
                    .all(|candidate| candidate.display.primary != "missing-link"),
                "broken symlink leaked into {buffer:?}"
            );
        }
    }

    fn provider(specs: Arc<SpecRegistry>) -> FilesystemProvider {
        FilesystemProvider::new(
            false,
            specs,
            Arc::new(CommandHelpCache::default()),
            Arc::new(AliasCache::default()),
        )
    }

    #[test]
    fn value_flags_suppress_file_rows_and_path_flags_offer_them() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        fs::write(directory.path().join("main.go"), b"package main\n").expect("go file");
        fs::write(directory.path().join("main.rs"), b"fn main() {}\n").expect("rust file");
        fs::create_dir(directory.path().join("nested")).expect("nested directory");
        let provider = provider(Arc::new(SpecRegistry::default()));

        // Literal-text slots: no filesystem rows at all.
        for buffer in [
            "git commit -m ",
            "ssh -p ",
            "curl -H ",
            "python -W ",
            "bash -o ",
            "bash -O pl",
            "zsh -o pl",
            "grep -A ",
            "curl --data-binary literal",
            "gcc -D ",
            "clang -std ",
            "rustc --edition ",
            "cargo build --target ",
            "go test -run ",
            "cmake -G ",
            "gradle -P ",
            "mvn -P ",
            "kotlinc -jvm-target ",
            "swiftc -module-name ",
            "xcodebuild -scheme ",
            "rustfmt --edition ",
            "clang-format --style ",
            "goimports -local ",
            "ktlint --reporter ",
        ] {
            let context = context(directory.path(), buffer, 1);
            let output = provider.complete(&context);
            assert!(
                output.candidates.is_empty(),
                "{buffer:?} must not offer file rows: {:?}",
                output
                    .candidates
                    .iter()
                    .map(|candidate| candidate.display.primary.as_str())
                    .collect::<Vec<_>>()
            );
        }

        // Path slots: files and directories are offered (trailing space and
        // mid-typing alike).
        for buffer in [
            "curl -o ",
            "curl -o pl",
            "sftp -b pl",
            "ssh -E pl",
            "bash --rcfile pl",
            "bash --init-file=pl",
            "clang -include pl",
            "cargo --manifest-path pl",
            "go build -modfile pl",
            "ctest --resource-spec-file pl",
            "cpack --config pl",
            "meson --cross-file pl",
            "conan --lockfile pl",
            "gradle -b pl",
            "mvn -f pl",
            "kotlinc -cp pl",
            "swiftc -import-bridging-header pl",
            "xcodebuild -xcconfig pl",
            "java -jar pl",
            "swift-format --configuration pl",
            "clang-tidy --config-file pl",
            "ktlint --editorconfig pl",
        ] {
            let context = context(directory.path(), buffer, 1);
            let output = provider.complete(&context);
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "{buffer:?} must offer files"
            );
        }

        // Directory-only slots.
        for buffer in [
            "make -C ",
            "just -d ",
            "just -dne",
            "just --working-directory=ne",
            "gcc -I ne",
            "gcc -Ine",
            "clang --sysroot=ne",
            "cmake -S ne",
            "cmake -Bne",
            "cmake ne",
            "ctest --test-dir ne",
            "cpack -B ne",
            "ninja -C ne",
            "meson setup ne",
            "meson -C ne",
            "conan --output-folder ne",
            "vcpkg --overlay-ports ne",
            "cargo install --path ne",
            "go work use ne",
            "rustup toolchain link custom ne",
            "gradle -p ne",
            "swiftc -I ne",
            "clang-cl /I ne",
            "rustfmt --config-path ne",
            "clang-tidy -p ne",
            "goimports -srcdir ne",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "nested/"),
                "directory slot missing for {buffer:?}"
            );
            assert!(
                !output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt")
            );
        }

        for buffer in ["just -f pl", "just --justfile=pl", "just --dotenv-path pl"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 2))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "just path slot missing for {buffer:?}"
            );
        }

        for buffer in [
            "env -C ne",
            "env -Cne",
            "sudo -D ne",
            "sudo -Dne",
            "sudo --chdir=ne",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 2));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "nested/"),
                "wrapper directory missing for {buffer:?}"
            );
            assert!(
                !output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt")
            );
        }
        for buffer in ["sudo -C ", "sudo -C3 ", "sudo --close-from=3 "] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 3))
                    .candidates
                    .is_empty(),
                "sudo close-from is numeric, not a directory slot: {buffer:?}"
            );
        }
        for buffer in ["sudo make -Dne", "time make -opl", "time -o pl"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 4))
                    .candidates
                    .is_empty(),
                "nested command flags must not be claimed by an outer wrapper: {buffer:?}"
            );
        }
        for buffer in [
            "/usr/bin/time -o pl",
            "command time -o pl",
            "sudo time -o pl",
            "xargs -apl",
            "doas -C pl",
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 3))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "wrapper file missing for {buffer:?}"
            );
        }

        // The flag word itself is still a flag position, not its value.
        let flag_context = context(directory.path(), "curl -", 1);
        let output = provider.complete(&flag_context);
        assert!(output.candidates.is_empty());

        for buffer in [
            "cargo build --target ./pl",
            "rustc --target=./pl",
            "rustdoc --target ./pl",
            "cargo --config=./pl",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 5));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "./plain.txt"),
                "explicit hybrid path missing for {buffer:?}"
            );
        }

        let response = provider.complete(&context(directory.path(), "rustc @pl", 6));
        assert!(response.candidates.iter().any(|candidate| {
            candidate
                .edit
                .as_ref()
                .is_some_and(|edit| edit.replacement == "@plain.txt")
        }));
        for (buffer, replacement) in [
            ("kotlinc -cp nested:pl", "nested:plain.txt"),
            (
                "javac --class-path=nested:pl",
                "--class-path=nested:plain.txt",
            ),
            ("rustc -Ldependency=ne", "-Ldependency=nested/"),
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 6))
                    .candidates
                    .iter()
                    .any(|candidate| candidate
                        .edit
                        .as_ref()
                        .is_some_and(|edit| edit.replacement == replacement)),
                "structured path value missing for {buffer:?}"
            );
        }
        for buffer in [
            "go run main.go pl",
            "go doc . pl",
            "go build ./cmd ",
            "rustc main.rs ",
            "rustc +nightly main.rs ",
            "rustc - ",
            "rustdoc main.rs ",
            "kotlin MainKt ",
            "kotlin -e expression ",
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 7))
                    .candidates
                    .is_empty(),
                "literal Go argument position leaked filesystem rows: {buffer:?}"
            );
        }
        for buffer in ["go build main.go pl", "rustc +nightly ma", "rustdoc ma"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 8))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "main.rs"
                        || candidate.display.primary == "plain.txt"),
                "compiler/source input path missing for {buffer:?}"
            );
        }
        for buffer in [
            "gofmt pl",
            "rustfmt pl",
            "swift-format format pl",
            "ktlint pl",
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 8))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "formatter input path missing for {buffer:?}"
            );
        }
    }

    #[test]
    fn versioned_python_and_compiler_path_slots_follow_the_command_family() {
        let directory = tempfile::tempdir().expect("directory");
        for name in ["script.py", "app.jar", "Main.java", "Main.kt", "main.rs"] {
            fs::write(directory.path().join(name), b"").expect("file");
        }
        fs::create_dir(directory.path().join("classes")).expect("classes directory");
        fs::write(directory.path().join("plain.txt"), b"").expect("plain file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for (buffer, expected) in [
            ("python3.14 sc", "script.py"),
            ("pypy3.11 sc", "script.py"),
            ("./bin/python3.14 sc", "script.py"),
            ("java -jar ap", "app.jar"),
            ("javac Ma", "Main.java"),
            ("rustc ma", "main.rs"),
            ("rustdoc +nightly ma", "main.rs"),
            ("gcc-14 ma", "main.rs"),
            ("aarch64-linux-gnu-g++-13 ma", "main.rs"),
            ("kotlin Ma", "Main.java"),
            ("kotlinc-native Ma", "Main.java"),
            ("rustc -o cl", "classes/"),
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 10))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == expected),
                "path slot missing for {buffer:?}"
            );
        }

        assert!(
            provider
                .complete(&context(directory.path(), "javac -d cl", 11))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "classes/")
        );
        for buffer in ["python3.14 -m sc", "pypy3.11 -W ", "./bin/ssh host pl"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 12))
                    .candidates
                    .is_empty(),
                "literal or remote slot leaked local files for {buffer:?}"
            );
        }
        for buffer in ["rustc main.rs ", "rustdoc main.rs ", "kotlin MainKt "] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 13))
                    .candidates
                    .is_empty(),
                "program argument slot leaked filesystem rows for {buffer:?}"
            );
        }
    }

    #[test]
    fn directory_creation_removal_and_pushd_use_directory_slots() {
        let directory = tempfile::tempdir().expect("directory");
        fs::create_dir(directory.path().join("nested")).expect("nested directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("plain file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in ["mkdir ", "rmdir ", "pushd ", "command pushd "] {
            let rows: Vec<_> = provider
                .complete(&context(directory.path(), buffer, 1))
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect();
            assert_eq!(rows, ["nested/"], "directory rows for {buffer:?}");
        }

        assert!(
            provider
                .complete(&context(directory.path(), "sudo pushd ", 2))
                .candidates
                .is_empty(),
            "an external wrapper cannot invoke the shell-only pushd builtin"
        );
        assert!(
            provider
                .complete(&context(directory.path(), "pushd +", 3))
                .candidates
                .is_empty(),
            "directory-stack indices are values, not filesystem paths"
        );
    }

    #[test]
    fn cd_directories_are_runnable_by_default_and_can_continue_instead() {
        let directory = tempfile::tempdir().expect("directory");
        fs::create_dir(directory.path().join("nested folder")).expect("nested directory");
        for behavior in [CdEnterBehavior::Execute, CdEnterBehavior::Continue] {
            let provider =
                provider(Arc::new(SpecRegistry::default())).with_cd_enter_behavior(behavior);
            for buffer in [
                "cd ne",
                "cd -- ne",
                "command cd ne",
                "builtin cd ne",
                "time cd ne",
            ] {
                let context = context(directory.path(), buffer, 1);
                let output = provider.complete(&context);
                let candidate = output
                    .candidates
                    .iter()
                    .find(|candidate| candidate.display.primary == "nested folder/")
                    .expect("cd directory");
                if behavior == CdEnterBehavior::Execute {
                    assert_eq!(candidate.action, CandidateAction::Insert, "{buffer}");
                    assert_eq!(candidate.completeness, Completeness::Runnable, "{buffer}");
                } else {
                    assert!(matches!(
                        candidate.action,
                        CandidateAction::InsertAndContinue { .. }
                    ));
                    assert!(matches!(
                        candidate.completeness,
                        Completeness::NeedsInput { .. }
                    ));
                }
                let activation =
                    crate::completion::activate_candidate(candidate, &context, &context.buffer)
                        .expect("activate cd directory");
                let crate::completion::Activation::ReplaceBuffer { text, cursor } = activation
                else {
                    panic!("expected directory edit");
                };
                let parsed = crate::parser::parse_line(&text, cursor).expect("parse filled cd");
                assert_eq!(parsed.current_prefix, "nested folder/", "{buffer}");
            }
        }
    }

    #[test]
    fn other_directory_slots_still_continue_with_the_default_cd_preference() {
        let directory = tempfile::tempdir().expect("directory");
        fs::create_dir(directory.path().join("nested")).expect("nested directory");
        let provider = provider(Arc::new(SpecRegistry::default()));
        for buffer in [
            "ls ne",
            "pushd ne",
            "cd > ne",
            "env -C ne cd",
            "sudo -D ne cd",
        ] {
            let cursor = buffer.find("ne").expect("directory prefix") + 2;
            let context = CompletionContext::new(
                QueryId::new(1),
                ShellKind::Zsh,
                directory.path().to_owned(),
                BufferSnapshot::new(buffer, cursor, BufferRevision::new(1), SyncQuality::Exact)
                    .expect("buffer"),
            )
            .expect("context");
            let output = provider.complete(&context);
            let candidate = output
                .candidates
                .iter()
                .find(|candidate| candidate.display.primary == "nested/")
                .unwrap_or_else(|| panic!("directory missing for {buffer}"));
            assert!(
                matches!(candidate.action, CandidateAction::InsertAndContinue { .. }),
                "{buffer}"
            );
            assert!(
                matches!(candidate.completeness, Completeness::NeedsInput { .. }),
                "{buffer}"
            );
        }
    }

    #[test]
    fn shell_builtin_path_slots_respect_the_resolution_domain() {
        let directory = tempfile::tempdir().expect("directory");
        fs::create_dir(directory.path().join("nested")).expect("nested directory");
        fs::write(directory.path().join("plain.sh"), b"echo ok\n").expect("source file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in ["cd ne", "command cd ne", "builtin cd ne", "time cd ne"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 1))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "nested/"),
                "shell cd directory missing for {buffer:?}"
            );
        }
        for buffer in [
            "source pl",
            "command source pl",
            "builtin source pl",
            "time source pl",
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 2))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.sh"),
                "shell source file missing for {buffer:?}"
            );
        }
        for buffer in [
            "sudo cd ne",
            "exec cd ne",
            "sudo source pl",
            "exec source pl",
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 3))
                    .candidates
                    .is_empty(),
                "external wrapper borrowed shell-builtin path semantics for {buffer:?}"
            );
        }
    }

    #[test]
    fn completed_flag_values_do_not_shift_later_positional_slots() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in ["python -W ignore pl", "grep -A 2 pattern pl"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 1))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "later file slot missing for {buffer:?}"
            );
        }
    }

    #[test]
    fn expression_options_advance_to_input_files_in_all_value_forms() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in [
            "grep -e pattern pl",
            "grep -epattern pl",
            "grep --regexp=pattern pl",
            "rg -f patterns.txt pl",
            "rg -fpatterns.txt pl",
            "sed -e s/a/b/ pl",
            "sed -es/a/b/ pl",
            "sed -f rules.sed pl",
            "awk -f program.awk pl",
            "jq --from-file=filter.jq pl",
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 1))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "input-file slot missing for {buffer:?}"
            );
        }

        for buffer in [
            "grep -e pattern",
            "sed -e script",
            "awk -v name=value program",
            "python -mhttp.server pl",
            "node --eval=code pl",
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 2))
                    .candidates
                    .is_empty(),
                "literal/program slot leaked files for {buffer:?}"
            );
        }
    }

    #[test]
    fn curl_data_file_syntax_preserves_the_at_prefix() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for (buffer, replacement) in [
            ("curl --data-binary @pl", "@plain.txt"),
            ("curl --data-binary=@pl", "--data-binary=@plain.txt"),
            ("curl -d@pl", "-d@plain.txt"),
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            let file = output
                .candidates
                .iter()
                .find(|candidate| candidate.display.primary == "plain.txt")
                .unwrap_or_else(|| panic!("curl data file missing for {buffer:?}"));
            assert_eq!(file.edit.as_ref().expect("edit").replacement, replacement);
        }
    }

    #[test]
    fn attached_flag_values_and_double_dash_use_the_correct_path_slice() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        fs::create_dir(directory.path().join("nested")).expect("nested directory");
        let provider = provider(Arc::new(SpecRegistry::default()));

        let output = provider.complete(&context(directory.path(), "curl --output=pl", 1));
        let file = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "plain.txt")
            .expect("attached output path");
        assert_eq!(
            file.edit.as_ref().expect("edit").replacement,
            "--output=plain.txt"
        );

        let output = provider.complete(&context(directory.path(), "make -Cne", 2));
        let nested = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "nested/")
            .expect("attached directory path");
        assert_eq!(nested.edit.as_ref().expect("edit").replacement, "-Cnested/");

        assert!(
            provider
                .complete(&context(directory.path(), "curl --header=pl", 3))
                .candidates
                .is_empty(),
            "literal attached values must not scan the cwd"
        );
        assert!(
            provider
                .complete(&context(directory.path(), "cat -- pl", 4))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "plain.txt")
        );
    }

    #[test]
    fn unknown_arguments_never_gain_path_semantics_from_spelling_or_local_files() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        fs::create_dir(directory.path().join("src")).expect("source directory");
        fs::write(directory.path().join("src/plain.txt"), b"plain").expect("nested file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in ["claude ", "codex ", "echo ", "curl ", "ssh host "] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 1))
                    .candidates
                    .is_empty(),
                "literal slot leaked cwd files for {buffer:?}"
            );
        }
        for buffer in [
            "claude ./pl",
            "claude src/pl",
            "custom-command ./missing/file",
            "pnpm dev ./pl",
            "npx tool src/pl",
            "npm exec tool src/pl",
        ] {
            let context = context(directory.path(), buffer, 2);
            assert!(context.argument.local_path().is_none(), "{buffer}");
            assert!(!provider.applies(&context), "{buffer}");
            let output = provider.complete(&context);
            assert!(
                output.candidates.is_empty() && output.diagnostics.is_empty(),
                "{buffer}"
            );
        }
        for buffer in ["claude https://example.com/a", "claude host:/remote/a"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 4))
                    .candidates
                    .is_empty(),
                "non-local path syntax must stay quiet for {buffer:?}"
            );
        }
        assert!(
            provider
                .complete(&context(directory.path(), "cat pl", 3))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "plain.txt")
        );
    }

    #[test]
    fn redirect_targets_are_paths_but_fd_duplication_is_not() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in ["kill 123 > pl", "pnpm dev 2> pl", "echo hi &>> pl", "> pl"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 1))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "redirect path missing for {buffer:?}"
            );
        }
        for buffer in [
            "echo hi 2>&1",
            "echo hi 2>&",
            "cat <& pl",
            "cat << EO",
            "cat <<-EO",
            "cat <<< pl",
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 2))
                    .candidates
                    .is_empty(),
                "fd duplication must not offer files for {buffer:?}"
            );
        }
    }

    #[test]
    fn sudo_edit_operands_are_file_slots_not_executable_slots() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in ["sudo -e pl", "sudo --edit pl", "sudo -u root -e pl"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 1))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "sudo edit file missing for {buffer:?}"
            );
        }
    }

    #[test]
    fn command_working_directory_options_change_the_scan_root() {
        let root = tempfile::tempdir().expect("root");
        fs::create_dir(root.path().join("repo")).expect("repo");
        fs::create_dir(root.path().join("app")).expect("app");
        fs::write(root.path().join("repo/inside.txt"), b"git").expect("git file");
        fs::write(root.path().join("app/input.ts"), b"node").expect("node file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        assert!(
            provider
                .complete(&context(root.path(), "git -C repo add in", 1))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "inside.txt")
        );
        assert!(
            provider
                .complete(&context(root.path(), "pnpm -C app install ./in", 2))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "./input.ts")
        );
    }

    #[test]
    fn ref_host_and_target_slots_suppress_file_rows() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        // Ref-taking git slots, the ssh host slot, and build-tool target
        // slots produce no filesystem rows.
        for buffer in [
            "git checkout ma",
            "git checkout ",
            "git log ",
            "ssh ",
            "ssh de",
            "sftp ",
            "mosh ",
            "make ",
            "make b",
            "just ",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output.candidates.is_empty(),
                "{buffer:?} must not offer file rows"
            );
        }

        for buffer in ["git diff ./pl", "git checkout ./pl", "git log ./pl"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 2))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "./plain.txt"),
                "explicit git path missing for {buffer:?}"
            );
        }
        assert!(
            provider
                .complete(&context(directory.path(), "git switch -c feature/pl", 3))
                .candidates
                .is_empty(),
            "new branch names containing slashes must not become paths"
        );
        for buffer in ["ssh host ./pl", "sftp host ./pl", "mosh host ./pl"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 4))
                    .candidates
                    .is_empty(),
                "remote operand leaked local paths for {buffer:?}"
            );
        }

        // Path-oriented slots still offer files.
        for buffer in ["git add ", "git add ma", "ssh -i ", "scp "] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "{buffer:?} must offer files"
            );
        }
        assert!(
            provider
                .complete(&context(directory.path(), "scp user@host:/pa", 2))
                .candidates
                .is_empty(),
            "remote scp paths must not scan the local cwd"
        );
    }

    #[test]
    fn package_managers_only_offer_files_at_real_path_slots() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("main.ts"), b"").expect("source file");
        fs::create_dir(directory.path().join("src")).expect("source directory");
        fs::write(directory.path().join("src/main.ts"), b"").expect("nested source file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in [
            "deno run ma",
            "bun build ma",
            "bun build src/ma",
            "pnpm install ./ma",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary.ends_with("main.ts")),
                "{buffer:?} must offer source paths"
            );
        }

        for buffer in ["pnpm ", "pnpm install ", "npm run ", "bun run "] {
            let output = provider.complete(&context(directory.path(), buffer, 2));
            assert!(
                output.candidates.is_empty(),
                "{buffer:?} must not leak cwd files into command/script selection"
            );
        }

        for buffer in [
            "pnpm add @scope/pkg",
            "pnpm dlx @scope/pkg",
            "npm exec @scope/pkg",
            "npx @scope/pkg",
            "deno run https://deno.land/x/main.ts",
            "deno run jsr:@scope/pkg",
            "rsync host:/remote/ma",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 3));
            assert!(
                output.candidates.is_empty(),
                "package/URL/remote syntax must not scan the local cwd: {buffer:?}"
            );
            assert!(
                output.diagnostics.is_empty(),
                "package/URL/remote syntax must not emit filesystem diagnostics: {buffer:?}"
            );
        }

        assert!(
            provider
                .complete(&context(directory.path(), "cat src/ma", 4))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "src/main.ts"),
            "known file-taking commands keep relative nested paths"
        );
    }

    #[test]
    fn semantic_identifiers_stay_non_paths_even_when_local_names_collide() {
        let directory = tempfile::tempdir().expect("directory");
        fs::create_dir_all(directory.path().join("owner/repo")).expect("repo-shaped directory");
        fs::create_dir_all(directory.path().join("pods/pod")).expect("resource-shaped directory");
        fs::create_dir_all(directory.path().join("feature/topic")).expect("ref-shaped directory");
        fs::create_dir_all(directory.path().join("example.com/team"))
            .expect("module-shaped directory");
        let provider = provider(Arc::new(SpecRegistry::default()));
        for buffer in [
            "docker pull owner/repo/",
            "podman run owner/repo/",
            "gh repo view owner/repo/",
            "kubectl get pods/pod/",
            "git fetch origin feature/topic/",
            "npm install owner/repo/",
            "go run example.com/team/tool",
            "gh api /repos/owner/repo",
            "grep /pattern/",
            "sed s/old/new/",
        ] {
            assert!(
                !provider.applies(&context(directory.path(), buffer, 1)),
                "local directory overrides semantic slot: {buffer}"
            );
        }
    }

    #[test]
    fn real_paths_still_complete_after_semantic_argument_guards() {
        let directory = tempfile::tempdir().expect("directory");
        fs::create_dir_all(directory.path().join("src")).expect("source directory");
        fs::write(directory.path().join("src/main.go"), b"").expect("source file");
        let provider = provider(Arc::new(SpecRegistry::default()));
        for buffer in [
            "cat src/ma",
            "grep pattern src/ma",
            "sed s/a/b/ src/ma",
            "awk '1' src/ma",
            "jq . src/ma",
            "go build ./src/ma",
            "go run src/main.go",
            "npm install ./src/ma",
            "docker build src/ma",
            "git fetch --upload-pack src/ma",
            "git fetch ./src/ma",
            "kubectl get --filename src/ma",
            "kubectl get -f src/ma",
            "gh api --input src/ma",
            "docker run --env-file src/ma image",
        ] {
            // The active cursor is at the path, before a later image operand.
            let cursor = buffer.find(" image").unwrap_or(buffer.len());
            let context = CompletionContext::new(
                QueryId::new(1),
                ShellKind::Zsh,
                directory.path().into(),
                crate::completion::BufferSnapshot::new(
                    buffer,
                    cursor,
                    BufferRevision::new(1),
                    SyncQuality::Exact,
                )
                .expect("snapshot"),
            )
            .expect("context");
            let output = provider.complete(&context);
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|row| row.display.primary.ends_with("main.go")),
                "real path suppressed: {buffer}"
            );
            assert!(output.diagnostics.is_empty(), "{buffer}");
        }
        for buffer in [
            "cat missing/file",
            "cd missing/",
            "kubectl get -f missing/file",
            "go run ./missing/main.go",
            "npm install ./missing/pkg",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 2));
            assert!(
                output
                    .diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.code == "HK-FS-001"),
                "real directory failure suppressed: {buffer}"
            );
        }
    }

    #[test]
    fn unresolved_shell_directories_stay_quiet_but_literal_metacharacters_work() {
        let directory = tempfile::tempdir().expect("directory");
        let provider = provider(Arc::new(SpecRegistry::default()));
        for buffer in [
            "cd $UNRESOLVED/path/",
            "cd ${UNRESOLVED}/path/",
            "cd \"$UNRESOLVED/path/\"",
            "cat src/*/file",
            "cat src/?/file",
            "cat src/[ab]/file",
            "cat {one,two}/file",
            "cat $(pwd)/file",
            "cat `pwd`/file",
            "cat ~unknown-hokan-user/file",
            "custom-command missing/identifier",
            "open https://example.com/path",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output.candidates.is_empty() && output.diagnostics.is_empty(),
                "unresolved path: {buffer}"
            );
        }
        for name in [
            "*",
            "?",
            "[ab]",
            "{one,two}",
            "$UNRESOLVED",
            "$HOME",
            "${PWD}",
            "~",
        ] {
            fs::create_dir(directory.path().join(name)).expect("literal directory");
            fs::write(directory.path().join(name).join("file.txt"), b"").expect("literal file");
        }
        for buffer in [
            "cat '*'/fi",
            "cat \\*/fi",
            "cat '?'/fi",
            "cat '[ab]'/fi",
            "cat '{one,two}'/fi",
            "cat '$UNRESOLVED'/fi",
            "cat '$HOME'/fi",
            "cat \\$HOME/fi",
            "cat '${PWD}'/fi",
            "cat '~'/fi",
            "cat \"~/fi\"",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 2));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|row| row.display.primary.ends_with("file.txt")),
                "literal path suppressed: {buffer}"
            );
        }
        assert!(
            !provider.applies(&context(directory.path(), "cat $UNRESOLVED/fi", 3)),
            "unknown variables must not scan a coincidentally named literal directory"
        );
        let output = provider.complete(&context(directory.path(), "cat '$HOME'/fi", 4));
        let candidate = output
            .candidates
            .iter()
            .find(|row| row.display.primary == "$HOME/file.txt")
            .expect("literal home file");
        assert_eq!(
            candidate.edit.as_ref().expect("edit").replacement,
            "'$HOME/file.txt'"
        );
    }

    #[test]
    fn semantic_arguments_do_not_trigger_directory_errors() {
        let directory = tempfile::tempdir().expect("directory");
        let provider = provider(Arc::new(SpecRegistry::default()));
        let mut failures = Vec::new();
        for buffer in [
            "docker pull ghcr.io/anomalyco/app:latest",
            "docker run anomalyco/app",
            "gh repo view anomalyco/tap",
            "gh api repos/anomalyco/tap",
            "kubectl get pods/my-pod",
            "go install example.com/team/tool@latest",
            "go get example.com/team/tool",
            "go run example.com/team/tool",
            "go test example.com/team/tool",
            "npm install owner/repo",
            "pnpm add github:owner/repo",
            "git fetch origin feature/topic",
            "git push origin HEAD:refs/heads/main",
            "git branch feature/topic",
            "git log feature/topic",
            "grep src/part",
            "sed s/old/new/",
            "awk /pattern/",
            "jq .items/2",
            "curl https://example.com/path",
            "open https://example.com/path",
            "cat $HOKAN_TEST_UNKNOWN/file",
            "cat src/*/file",
            "cat ~hokan-unknown-user/file",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            if !output.diagnostics.is_empty() || !output.candidates.is_empty() {
                failures.push(buffer);
            }
        }
        assert!(
            failures.is_empty(),
            "non-path arguments started filesystem completion: {failures:?}"
        );
    }

    #[test]
    fn homebrew_identifiers_do_not_scan_local_directories() {
        let directory = tempfile::tempdir().expect("directory");
        let provider = provider(Arc::new(SpecRegistry::default()));
        for buffer in [
            "brew uninstall anomalyco/tap/",
            "brew uninstall --cask anomalyco/tap/opencode",
            "brew remove --force -- anomalyco/tap/opencode",
            "brew rm anomalyco/tap/opencode",
            "brew upgrade anomalyco/tap/opencode",
            "brew install anomalyco/tap/opencode",
            "brew reinstall anomalyco/tap/opencode",
            "brew info anomalyco/tap/opencode",
            "brew tap anomalyco/",
            "brew untap anomalyco/tap/",
            "brew search /anomalyco/",
            "brew search --formula /anomalyco/",
            "sudo brew uninstall anomalyco/tap/opencode",
            "/opt/homebrew/bin/brew uninstall anomalyco/tap/opencode",
        ] {
            let context = context(directory.path(), buffer, 1);
            assert!(!provider.applies(&context), "filesystem owns {buffer:?}");
            let output = provider.complete(&context);
            assert!(output.candidates.is_empty(), "rows for {buffer:?}");
            assert!(output.diagnostics.is_empty(), "diagnostics for {buffer:?}");
        }

        // Even when a tap-shaped directory exists, it must not supply rows.
        fs::create_dir_all(directory.path().join("anomalyco/tap")).expect("local directory");
        fs::write(directory.path().join("anomalyco/tap/unrelated"), b"").expect("file");
        assert!(!provider.applies(&context(
            directory.path(),
            "brew uninstall anomalyco/tap/",
            2,
        )));
    }

    #[test]
    fn homebrew_explicit_local_paths_keep_filesystem_completion() {
        let directory = tempfile::tempdir().expect("directory");
        fs::create_dir_all(directory.path().join("local")).expect("local directory");
        fs::write(directory.path().join("local/formula.rb"), b"").expect("formula");
        let provider = provider(Arc::new(SpecRegistry::default()));
        for buffer in ["brew install ./local/fo", "cat local/fo"] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|row| row.display.primary.ends_with("formula.rb")),
                "no local path for {buffer:?}"
            );
            assert!(output.diagnostics.is_empty());
        }
    }

    #[test]
    fn opencode_scoped_plugin_specs_do_not_trigger_filesystem_diagnostics() {
        let directory = tempfile::tempdir().expect("directory");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in [
            "opencode plugin @prevalentware/",
            "opencode plugin @prevalentware/opencode-goal-plugin",
            "opencode plugin -g @prevalentware/opencode-goal-plugin@latest",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output.candidates.is_empty(),
                "package specifier leaked filesystem candidates for {buffer:?}"
            );
            assert!(
                output.diagnostics.is_empty(),
                "package specifier triggered a filesystem diagnostic for {buffer:?}: {:?}",
                output.diagnostics
            );
        }
    }

    #[test]
    fn scoped_names_remain_paths_for_file_commands() {
        let directory = tempfile::tempdir().expect("directory");
        let scoped = directory.path().join("@prevalentware");
        fs::create_dir(&scoped).expect("scoped directory");
        fs::write(scoped.join("plain.txt"), b"").expect("scoped file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        let output = provider.complete(&context(directory.path(), "cat @prevalentware/pl", 2));
        assert!(
            output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "@prevalentware/plain.txt"),
            "file commands must retain scoped-looking local paths"
        );
        assert!(output.diagnostics.is_empty());
    }

    #[test]
    fn typed_prefix_on_a_spec_covered_command_offers_files() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("Dockerfile"), b"").expect("file");
        let provider = provider(Arc::new(SpecRegistry::load(None)));
        // `ls` recipes are flag-only: with a typed prefix the spec rows die
        // on match and file rows must appear.
        let output = provider.complete(&context(directory.path(), "ls Do", 1));
        assert!(
            output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "Dockerfile"),
            "`ls Do` must offer files"
        );
        // The empty slot still belongs to the spec.
        assert!(
            provider
                .complete(&context(directory.path(), "ls ", 2))
                .candidates
                .is_empty()
        );
    }

    #[test]
    fn package_manager_slots_offer_no_file_rows() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"").expect("file");
        let provider = provider(Arc::new(SpecRegistry::default()));
        for buffer in [
            "pnpm ",
            "pnpm bu",
            "npm ",
            "yarn ",
            "bun ",
            "npm run ",
            "pnpm run ",
            "pnpm run bu",
            "deno task ",
            "pnpm --filter ",
            "pnpm --filter @acme/api ",
            "yarn workspace @acme/we",
            "npm --workspace=@acme/we",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output.candidates.is_empty(),
                "{buffer:?} must not offer file rows"
            );
        }
    }

    #[test]
    fn git_double_dash_resumes_file_rows_and_dash_b_stays_quiet() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        let provider = provider(Arc::new(SpecRegistry::default()));
        // After `--` the slot is a pathspec again: file rows resume.
        for buffer in ["git checkout -- ", "git checkout -- pl"] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "{buffer:?} must offer files"
            );
        }
        // After `-b`/`-c` the slot is a new branch name: no file rows.
        for buffer in ["git checkout -b ", "git checkout -b ne", "git switch -c "] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output.candidates.is_empty(),
                "{buffer:?} must not offer file rows"
            );
        }

        for buffer in ["git checkout main ", "git reset main ", "git restore "] {
            let output = provider.complete(&context(directory.path(), buffer, 2));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "completed ref must open a path slot for {buffer:?}"
            );
        }
        for buffer in ["git branch new main ", "git branch -m old "] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 3))
                    .candidates
                    .is_empty(),
                "branch completion must stop for {buffer:?}"
            );
        }
    }

    #[test]
    fn wrapper_chdir_and_find_exec_use_the_correct_executable_or_path_root() {
        let directory = tempfile::tempdir().expect("directory");
        fs::create_dir_all(directory.path().join("app/sub")).expect("nested app");
        fs::write(directory.path().join("root.txt"), b"root").expect("root file");
        fs::write(directory.path().join("app/inside.txt"), b"inside").expect("inside file");
        fs::write(directory.path().join("app/sub/deep.txt"), b"deep").expect("deep file");
        let runner = directory.path().join("app/runner");
        fs::write(&runner, b"#!/bin/sh\n").expect("runner");
        fs::set_permissions(&runner, fs::Permissions::from_mode(0o700)).expect("runner mode");
        fs::write(directory.path().join("app/rubbish"), b"plain\n").expect("non-executable decoy");
        fs::write(directory.path().join("app/script.py"), b"print('ok')\n").expect("Python script");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in ["env -C app cat in", "sudo -D app cat in"] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "inside.txt"),
                "nested cwd file missing for {buffer:?}"
            );
            assert!(
                !output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "root.txt"),
                "outer cwd leaked for {buffer:?}"
            );
        }
        assert!(
            provider
                .complete(&context(directory.path(), "builtin ls ", 4))
                .candidates
                .is_empty()
        );

        let wrapper_value =
            provider.complete(&context(directory.path(), "sudo -D app env -C su", 2));
        assert!(
            wrapper_value
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "sub/")
        );

        for buffer in [
            "env -C app ./ru",
            "find . -exec ./app/ru",
            "find . -exec env -C app ./ru",
            "uv --directory app run ./ru",
            "poetry -C app run ./ru",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 3));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary.ends_with("runner")),
                "explicit executable missing for {buffer:?}"
            );
            assert!(
                output
                    .candidates
                    .iter()
                    .all(|candidate| !candidate.display.primary.ends_with("rubbish")),
                "non-executable leaked for {buffer:?}"
            );
        }

        let script = provider.complete(&context(
            directory.path(),
            "uv --directory app run --script scr",
            5,
        ));
        assert!(
            script
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "script.py")
        );
        assert!(
            provider
                .complete(&context(
                    directory.path(),
                    "uv --directory app run --module scr",
                    6,
                ))
                .candidates
                .is_empty(),
            "Python module names must not degrade into filesystem rows"
        );
    }

    #[test]
    fn package_manager_path_flags_and_literal_values_have_distinct_slots() {
        let directory = tempfile::tempdir().expect("directory");
        fs::create_dir(directory.path().join("cache-dir")).expect("cache directory");
        fs::create_dir_all(directory.path().join("@scope/dir")).expect("scoped directory");
        fs::write(directory.path().join("npmrc"), b"").expect("npmrc");
        fs::write(directory.path().join("1000"), b"").expect("numeric decoy");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in [
            "pnpm --store-dir ca",
            "npm --cache ca",
            "yarn --cache-folder ca",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "cache-dir/"),
                "directory flag missing for {buffer:?}"
            );
        }
        assert!(
            provider
                .complete(&context(directory.path(), "npm --userconfig np", 2))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "npmrc")
        );
        assert!(
            provider
                .complete(&context(directory.path(), "npm --prefix @scope/di", 3))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "@scope/dir/"),
            "directory-valued npm flags must retain scoped-looking local paths"
        );
        for buffer in ["deno run --location 1", "bun test --timeout 1"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 3))
                    .candidates
                    .is_empty(),
                "literal manager value leaked files for {buffer:?}"
            );
        }
    }

    #[test]
    fn find_start_paths_accept_files_and_repeat_until_the_expression_begins() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("start.txt"), b"").expect("start file");
        fs::create_dir(directory.path().join("nested")).expect("nested");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in ["find ", "find -L ", "find start.txt "] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "start.txt"),
                "find start file missing for {buffer:?}"
            );
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "nested/"),
                "find directory missing for {buffer:?}"
            );
        }
        for buffer in ["find . -name ", "find . -type f "] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 2))
                    .candidates
                    .is_empty(),
                "find expression leaked start paths for {buffer:?}"
            );
        }
    }

    #[test]
    fn make_and_ssh_family_flag_tables() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        fs::create_dir(directory.path().join("nested")).expect("nested directory");
        let provider = provider(Arc::new(SpecRegistry::default()));

        // `make -f` takes a file (Path), `-C` stays directory-only.
        let output = provider.complete(&context(directory.path(), "make -f ", 1));
        assert!(
            output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "plain.txt"),
            "`make -f ` must offer files"
        );
        let output = provider.complete(&context(directory.path(), "make -C ", 2));
        assert!(
            !output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "plain.txt"),
            "`make -C ` must stay directory-only"
        );

        // Attached jobs flag `make -j4`: no file rows at the next slot.
        let output = provider.complete(&context(directory.path(), "make -j4 ", 3));
        assert!(
            output.candidates.is_empty(),
            "`make -j4 ` must not offer file rows"
        );

        // ssh-family literal value flags stay quiet; path flags offer files.
        for buffer in ["mosh -p ", "sftp -P ", "ssh -Q ", "scp -J "] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output.candidates.is_empty(),
                "{buffer:?} must not offer file rows"
            );
        }
        for buffer in ["ssh -S ", "ssh -I ", "sftp -D ", "scp -D "] {
            let output = provider.complete(&context(directory.path(), buffer, 4));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "{buffer:?} must offer files"
            );
        }
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
