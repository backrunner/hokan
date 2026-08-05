use std::{fs, os::unix::fs::PermissionsExt, sync::Arc, time::Instant};

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderDiagnostic, ProviderOutput,
        SlotKind, TextEdit,
    },
    parser::escape_for_shell,
    providers::{CommandHelpCache, argument_progress},
    specs::SpecRegistry,
    terminal::RiskLevel,
};

const MAX_DIRECTORY_ENTRIES: usize = 5_000;
const DIRECTORY_BUDGET_MS: u128 = 80;

pub struct FilesystemProvider {
    show_hidden: bool,
    specs: Arc<SpecRegistry>,
    help: Arc<CommandHelpCache>,
}

impl FilesystemProvider {
    #[must_use]
    pub fn new(show_hidden: bool, specs: Arc<SpecRegistry>, help: Arc<CommandHelpCache>) -> Self {
        Self {
            show_hidden,
            specs,
            help,
        }
    }
}

impl CandidateProvider for FilesystemProvider {
    fn id(&self) -> &'static str {
        "filesystem"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        self.infer_slot(context).is_some()
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let Some(slot) = self.infer_slot(context) else {
            return ProviderOutput::default();
        };
        let prefix = context.parsed.current_prefix.as_str();
        let (directory_prefix, basename) = split_prefix(prefix);
        let scan_directory = scan_directory_for(&context.cwd, directory_prefix);
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
            // A leading dash makes the path look like a flag (`cd -foo/`);
            // prefix `./` for directories and files alike.
            if directory_prefix.is_empty() && logical.starts_with('-') {
                logical.insert_str(0, "./");
            }
            if directory {
                logical.push('/');
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
                    // Descending is the norm at path-like slots: keep
                    // directories level with files instead of sinking them
                    // below the siblings the user drills through.
                    (SlotKind::Path | SlotKind::Directory | SlotKind::NewFile, true, _, _) => 80,
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

impl FilesystemProvider {
    fn infer_slot(&self, context: &CompletionContext) -> Option<SlotKind> {
        let command = context.command()?;
        let (words, argument_position) = argument_progress(context)?;
        // Flags belong to the spec/help providers: a dashed active word never
        // completes to filesystem entries.
        if context.parsed.current_prefix.starts_with('-') {
            return None;
        }
        match command {
            "cd" => Some(SlotKind::Directory),
            "bash" | "zsh" | "sh" => Some(SlotKind::Executable),
            "df" => Some(SlotKind::Path),
            "tar" => tar_slot(&words, argument_position),
            "lsof" if words.contains(&"+D") => Some(SlotKind::Directory),
            "lsof" => None,
            "kill" | "ifconfig" | "ip" | "ps" => None,
            // Ref-taking git slots (`git checkout <…>`) belong to the git
            // provider's branch/remote/tag rows; `git add <path>` keeps
            // file completion.
            "git" if at_git_ref_slot(&words, argument_position) => None,
            // The ssh host slot belongs to the ssh-host provider.
            "ssh" | "sftp" | "mosh"
                if super::ssh::at_host_slot(
                    command,
                    &words,
                    argument_position,
                    &context.parsed.current_prefix,
                ) =>
            {
                None
            }
            // Build-tool first arguments are target names, not paths.
            "make" | "just" if argument_position == 0 => None,
            // Bare/script/keyword/filter positions of the Node package
            // managers belong to the project provider — cwd file rows are
            // noise there (`npm run `, `pnpm --filter `, `pnpm `, …).
            _ if super::manager_position(context).is_some()
                || super::filter_position(context).is_some() =>
            {
                None
            }
            _ => {
                // Spec-covered commands own the empty slot; with a typed
                // prefix the spec rows die on match anyway and file rows
                // must appear (`ls Do` completes `Documents/`).
                if self.specs.get(command).is_some() && context.parsed.current_prefix.is_empty() {
                    return None;
                }
                // A flag immediately before the active slot decides what the
                // slot wants: value flags (`git commit -m`, `ssh -p`) ask for
                // literal text, so raw file rows would be noise; file flags
                // (`curl -o`, `make -C`) want paths. `words[position]` is the
                // word before the active slot in both the trailing-space and
                // mid-typing cases (see `argument_progress`).
                if let Some(slot) = flag_value_slot(
                    command,
                    words.get(argument_position).copied().unwrap_or_default(),
                ) {
                    return Some(slot);
                }
                // At the first-argument position a command whose man page
                // documents subcommands (git-style) takes subcommand rows
                // instead of a raw directory scan. Past the first argument —
                // and whenever help has no subcommands (`cp`, `vim`) — file
                // completion still applies. Peek only: the help provider is
                // registered first and warms the shared cache from its
                // applies pass; never spawn `man` from here.
                if argument_position == 0
                    && self
                        .help
                        .peek(command)
                        .is_some_and(|help| help.has_subcommands())
                {
                    return None;
                }
                Some(SlotKind::Path)
            }
        }
    }
}

/// `git checkout <…>`-style slots take refs, not files — the git provider
/// owns them (`git add <path>` keeps file completion). After `--` the slot is
/// a pathspec and file rows resume; after `checkout -b` / `switch -c` the
/// slot is a new branch name and stays suppressed.
fn at_git_ref_slot(words: &[&str], argument_position: usize) -> bool {
    super::git::ref_slot_subcommand(words, argument_position).is_some()
        || super::git::new_branch_slot(words, argument_position)
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

/// Well-known flags whose value is literal text (`Value` — no filesystem
/// rows) versus a path (`Path`/`Directory`). Best-effort heuristics for the
/// common commands; unknown flags fall through to the caller's default.
fn flag_value_slot(command: &str, flag: &str) -> Option<SlotKind> {
    if !flag.starts_with('-') {
        return None;
    }
    match (command, flag) {
        ("git", "-C" | "--git-dir" | "--work-tree") => Some(SlotKind::Directory),
        ("git", "-F" | "--file") => Some(SlotKind::Path),
        (
            "git",
            "-m" | "--message" | "-c" | "--author" | "--date" | "--format" | "--pretty" | "--grep",
        ) => Some(SlotKind::Value),
        (
            "curl",
            "-o" | "--output" | "-K" | "--config" | "--cacert" | "--cert" | "--key" | "-T"
            | "--upload-file" | "--data-binary",
        ) => Some(SlotKind::Path),
        (
            "curl",
            "-d" | "--data" | "--data-raw" | "-H" | "--header" | "-X" | "--request" | "-u"
            | "--user" | "-A" | "--user-agent" | "-e" | "--referer" | "-x" | "--proxy"
            | "--connect-timeout" | "--max-time",
        ) => Some(SlotKind::Value),
        ("ssh", "-i" | "-F" | "-S") | ("scp", "-i" | "-F") => Some(SlotKind::Path),
        ("ssh", "-p" | "-l" | "-o" | "-L" | "-R" | "-D" | "-J" | "-W" | "-b" | "-c" | "-m")
        | ("scp", "-P" | "-o" | "-l" | "-c")
        | ("mosh", "-p")
        | ("sftp", "-P") => Some(SlotKind::Value),
        ("cargo", "--manifest-path") => Some(SlotKind::Path),
        (
            "cargo",
            "-p" | "--package" | "--features" | "-j" | "--jobs" | "--target" | "--profile",
        ) => Some(SlotKind::Value),
        ("make", "-f" | "--file") => Some(SlotKind::Path),
        ("make", "-C" | "--directory") => Some(SlotKind::Directory),
        ("make", "-j" | "--jobs") => Some(SlotKind::Value),
        // The attached form `make -j4` carries its own value: the slot after
        // it is a target, not a path.
        ("make", flag) if is_attached_jobs_flag(flag) => Some(SlotKind::Value),
        _ => None,
    }
}

/// `make -j<digits>`: the jobs flag with its value attached.
fn is_attached_jobs_flag(flag: &str) -> bool {
    flag.len() > 2
        && flag.starts_with("-j")
        && flag[2..]
            .chars()
            .all(|character| character.is_ascii_digit())
}

fn split_prefix(prefix: &str) -> (&str, &str) {
    prefix
        .rfind('/')
        .map_or(("", prefix), |index| prefix.split_at(index + 1))
}

/// The directory to scan for a typed path prefix. The shell expands `~/…`
/// before executing, so tilde prefixes scan under the home directory while
/// the literal `~/` spelling is kept for display and edits.
fn scan_directory_for(cwd: &std::path::Path, directory_prefix: &str) -> std::path::PathBuf {
    if directory_prefix.is_empty() {
        return cwd.to_owned();
    }
    if let Some(rest) = directory_prefix.strip_prefix("~/")
        && let Some(home) = std::env::home_dir()
    {
        return home.join(rest);
    }
    cwd.join(directory_prefix)
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
        providers::command_help::{CommandHelp, HelpEntry},
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
        let provider = provider(Arc::new(SpecRegistry::default()));

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
    fn help_subcommands_suppress_files_only_at_the_first_argument() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"").expect("file");
        let help = Arc::new(CommandHelpCache::default());
        help.seed(
            "git",
            CommandHelp {
                flags: Vec::new(),
                subcommands: vec![HelpEntry {
                    name: "add".into(),
                    description: String::new(),
                }],
            },
        );
        let provider =
            FilesystemProvider::new(false, Arc::new(SpecRegistry::load(None)), Arc::clone(&help));
        // Subcommand position with a positive help result: no file rows.
        assert!(
            provider
                .complete(&context(directory.path(), "git ", 1))
                .candidates
                .is_empty()
        );
        // Past the first argument file completion still works.
        assert!(
            provider
                .complete(&context(directory.path(), "git add ", 2))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "plain.txt")
        );
        // Help without subcommands (e.g. `cp`) never suppresses files.
        help.seed(
            "cp",
            CommandHelp {
                flags: vec![HelpEntry {
                    name: "-R".into(),
                    description: String::new(),
                }],
                subcommands: Vec::new(),
            },
        );
        assert!(
            provider
                .complete(&context(directory.path(), "cp ", 3))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "plain.txt")
        );
    }

    #[test]
    fn tilde_prefix_scans_the_home_directory() {
        let cwd = tempfile::tempdir().expect("cwd");
        let home = std::env::home_dir().expect("home directory");
        // `~/` resolves against $HOME, absolute and relative prefixes against cwd.
        assert_eq!(scan_directory_for(cwd.path(), "~/"), home.clone());
        assert_eq!(
            scan_directory_for(cwd.path(), "~/Documents/"),
            home.join("Documents")
        );
        assert_eq!(
            scan_directory_for(cwd.path(), "src/"),
            cwd.path().join("src")
        );
        assert_eq!(scan_directory_for(cwd.path(), ""), cwd.path().to_owned());
    }

    fn provider(specs: Arc<SpecRegistry>) -> FilesystemProvider {
        FilesystemProvider::new(false, specs, Arc::new(CommandHelpCache::default()))
    }

    #[test]
    fn value_flags_suppress_file_rows_and_path_flags_offer_them() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        fs::create_dir(directory.path().join("nested")).expect("nested directory");
        let provider = provider(Arc::new(SpecRegistry::default()));

        // Literal-text slots: no filesystem rows at all.
        for buffer in ["git commit -m ", "ssh -p ", "curl -H "] {
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
        for buffer in ["curl -o ", "curl -o pl"] {
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
        let dir_context = context(directory.path(), "make -C ", 1);
        let output = provider.complete(&dir_context);
        assert!(
            output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "nested/")
        );
        assert!(
            !output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "plain.txt")
        );

        // The flag word itself is still a flag position, not its value.
        let flag_context = context(directory.path(), "curl -", 1);
        let output = provider.complete(&flag_context);
        assert!(output.candidates.is_empty());
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

        // ssh-family value flags stay quiet; `ssh -S` wants a path.
        for buffer in ["mosh -p ", "sftp -P "] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output.candidates.is_empty(),
                "{buffer:?} must not offer file rows"
            );
        }
        let output = provider.complete(&context(directory.path(), "ssh -S ", 4));
        assert!(
            output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "plain.txt"),
            "`ssh -S ` must offer files"
        );
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
