use super::*;
use std::{io::ErrorKind, path::Path};

fn base<'a>(context: &'a CompletionContext, slot: &'a FilesystemSlot<'_>) -> &'a Path {
    slot.base_dir.as_deref().unwrap_or(context.cwd.as_ref())
}

fn new_target(context: &CompletionContext, slot: &FilesystemSlot<'_>) -> bool {
    slot.kind == SlotKind::NewFile
        || super::super::redirect_operator(context)
            .is_some_and(|operator| context.buffer.text[operator.range.clone()].contains('>'))
        || matches!(
            context.command().map(super::super::executable_basename),
            Some("touch" | "mkdir" | "vi" | "vim" | "nvim" | "nano" | "code")
        )
}

fn creates_parents(context: &CompletionContext) -> bool {
    let Some((words, position)) = argument_progress(context) else {
        return false;
    };
    if words
        .first()
        .map(|word| super::super::executable_basename(word))
        != Some("mkdir")
    {
        return false;
    }
    words
        .iter()
        .take(position + 1)
        .skip(1)
        .take_while(|word| **word != "--")
        .any(|word| {
            *word == "--parents"
                || (word.starts_with('-') && !word.starts_with("--") && word[1..].contains('p'))
        })
}

pub(super) fn repair_basename(
    context: &CompletionContext,
    slot: &FilesystemSlot<'_>,
    candidates: &mut [Candidate],
    started: Instant,
) {
    if new_target(context, slot)
        || candidates.iter().any(|candidate| {
            crate::completion::match_quality(slot.prefix, &candidate.display.primary) > 0
        })
    {
        return;
    }
    let (_, typed) = split_prefix(slot.prefix);
    for candidate in candidates {
        if started.elapsed().as_millis() > DIRECTORY_BUDGET_MS {
            break;
        }
        let (_, name) = split_prefix(candidate.display.primary.trim_end_matches('/'));
        if let Some(distance) = spelling_distance(typed, name) {
            candidate.action = CandidateAction::CorrectPath;
            candidate.display.description = if candidate.kind == CandidateKind::Directory {
                "修正目录名"
            } else {
                "修正文件名"
            }
            .into();
            candidate.score.spec_priority = 100 - i16::from(distance) * 20;
        }
    }
}

pub(super) fn read_error(
    provider: &FilesystemProvider,
    context: &CompletionContext,
    slot: &FilesystemSlot<'_>,
    error: &std::io::Error,
    started: Instant,
    literal_root: bool,
) -> ProviderOutput {
    let (directory, basename) = split_prefix(slot.prefix);
    if error.kind() == ErrorKind::NotFound && creates_parents(context) {
        return ProviderOutput::default();
    }
    if error.kind() == ErrorKind::NotFound
        && basename.is_empty()
        && context.command().map(super::super::executable_basename) == Some("mkdir")
        && fs::read_dir(scan_directory_with_root(
            base(context, slot),
            split_prefix(directory.trim_end_matches('/')).0,
            literal_root,
        ))
        .is_ok()
    {
        return ProviderOutput::default();
    }
    let mut output = ProviderOutput {
        candidates: Vec::new(),
        diagnostics: vec![ProviderDiagnostic {
            provider: "filesystem",
            code: "HK-FS-001",
            level: crate::completion::DiagnosticLevel::Warning,
            message: directory_read_error(
                Path::new(if directory.is_empty() { "." } else { directory }),
                error,
            ),
        }],
    };
    if !matches!(
        error.kind(),
        ErrorKind::NotFound | ErrorKind::NotADirectory | ErrorKind::PermissionDenied
    ) {
        return output;
    }
    let cwd = base(context, slot);
    // Keep expansions intact and never guess a value for an unknown variable.
    let root_len = ["${HOME}/", "${PWD}/", "$HOME/", "$PWD/", "~/", "/"]
        .into_iter()
        .find(|marker| directory.starts_with(marker))
        .map_or(0, str::len);
    if directory[root_len..].contains(['$', '`', '~']) {
        return output;
    }

    // A trailing slash on a real file is repairable only in a file-accepting
    // slot. For cd, the parent remains the useful alternative.
    let without_slash = directory.trim_end_matches('/');
    let resolved = scan_directory_with_root(cwd, without_slash, literal_root);
    if error.kind() == ErrorKind::NotADirectory
        && basename.is_empty()
        && resolved.is_file()
        && matches!(
            slot.kind,
            SlotKind::File | SlotKind::Path | SlotKind::Executable
        )
        && slot_accepts(slot.kind, &resolved, false)
    {
        output.candidates.push(repair(
            context,
            slot,
            without_slash,
            false,
            "移除文件末尾的 /",
            100,
            literal_root,
        ));
    }

    let mut parent = directory;
    while parent.len() > root_len && started.elapsed().as_millis() <= DIRECTORY_BUDGET_MS {
        parent = split_prefix(parent.trim_end_matches('/')).0;
        if parent.len() < root_len {
            break;
        }
        let parent_path = scan_directory_with_root(cwd, parent, literal_root);
        let Ok(entries) = fs::read_dir(&parent_path) else {
            continue;
        };
        let remainder = &directory[parent.len()..];
        let (missing, suffix) = remainder.split_once('/').unwrap_or((remainder, ""));
        if error.kind() == ErrorKind::NotFound {
            for entry in entries.take(MAX_DIRECTORY_ENTRIES) {
                if started.elapsed().as_millis() > DIRECTORY_BUDGET_MS {
                    break;
                }
                let Ok(entry) = entry else {
                    continue;
                };
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if name.chars().any(char::is_control)
                    || (!provider.show_hidden && name.starts_with('.') && !missing.starts_with('.'))
                {
                    continue;
                }
                let Some(distance) = spelling_distance(missing, &name) else {
                    continue;
                };
                if !entry.path().is_dir() {
                    continue;
                }
                let repaired_directory = format!("{parent}{name}/{suffix}");
                // Preserve the entire remaining directory path only when it
                // exists; never invent a correction for an inaccessible tail.
                if fs::read_dir(scan_directory_with_root(
                    cwd,
                    &repaired_directory,
                    literal_root,
                ))
                .is_err()
                {
                    continue;
                }
                let logical = format!("{repaired_directory}{basename}");
                output.candidates.push(repair(
                    context,
                    slot,
                    &logical,
                    true,
                    "修正目录路径",
                    100 - i16::from(distance) * 20,
                    literal_root,
                ));
            }
        }
        let logical = if parent.is_empty() { "./" } else { parent };
        output.candidates.push(repair(
            context,
            slot,
            logical,
            true,
            "返回可用目录继续选择",
            0,
            literal_root,
        ));
        break;
    }
    output
}

fn repair(
    context: &CompletionContext,
    slot: &FilesystemSlot<'_>,
    logical: &str,
    directory: bool,
    description: &str,
    priority: i16,
    literal_root: bool,
) -> Candidate {
    let logical = if logical.starts_with('-') {
        format!("./{logical}")
    } else {
        logical.to_owned()
    };
    let mut candidate = Candidate::new(
        context.query_id,
        &logical,
        description,
        Some(TextEdit {
            range: context.parsed.replacement.clone(),
            replacement: format!(
                "{}{}",
                slot.edit_prefix,
                escape_path_with_root(&logical, context.shell, literal_root)
            ),
            cursor_after: CursorPlacement::End,
        }),
        CandidateAction::CorrectPath,
        CandidateSource::Filesystem,
        if directory {
            CandidateKind::Directory
        } else {
            CandidateKind::File
        },
        Completeness::NeedsInput { slot: slot.kind },
        RiskLevel::Low,
        format!("fs-repair:{logical}"),
    );
    candidate.score.spec_priority = priority;
    candidate
}

/// Bounded optimal-string-alignment distance includes adjacent transpositions
/// (projcets -> projects). Short names require one edit, longer names at most
/// two. No recursive filesystem search or unbounded edit-distance matrix.
fn spelling_distance(typed: &str, name: &str) -> Option<u8> {
    let typed: Vec<_> = typed.to_lowercase().chars().take(65).collect();
    let name: Vec<_> = name.to_lowercase().chars().take(65).collect();
    if typed.len() < 3 || typed.len() > 64 || name.len() > 64 {
        return None;
    }
    let limit = if typed.len() >= 8 { 2 } else { 1 };
    if typed.len().abs_diff(name.len()) > limit {
        return None;
    }
    let mut previous: Vec<usize> = (0..=name.len()).collect();
    let mut before = previous.clone();
    for (i, left) in typed.iter().enumerate() {
        let mut row = vec![i + 1; name.len() + 1];
        for (j, right) in name.iter().enumerate() {
            row[j + 1] = (row[j] + 1)
                .min(previous[j + 1] + 1)
                .min(previous[j] + usize::from(left != right));
            if i > 0 && j > 0 && *left == name[j - 1] && typed[i - 1] == *right {
                row[j + 1] = row[j + 1].min(before[j - 1] + 1);
            }
        }
        before = previous;
        previous = row;
    }
    let distance = previous[name.len()];
    (distance > 0 && distance <= limit).then_some(distance as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        completion::{
            Activation, BufferSnapshot, CompletionEngine, SyncQuality, activate_candidate,
        },
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };

    fn complete(root: &Path, text: &str, cursor: usize) -> (CompletionContext, ProviderOutput) {
        let context = CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            root.to_owned(),
            BufferSnapshot::new(text, cursor, BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer"),
        )
        .expect("context");
        let mut engine = CompletionEngine::new(0, 6);
        engine.register(FilesystemProvider::new(
            false,
            Arc::new(SpecRegistry::default()),
            Arc::new(CommandHelpCache::default()),
            Arc::new(AliasCache::default()),
        ));
        let output = engine.complete(&context);
        (context, output)
    }

    #[test]
    fn misspelled_basename_and_parent_survive_ranking() {
        let root = tempfile::tempdir().expect("root");
        fs::create_dir_all(root.path().join("projects/src")).expect("tree");
        for (text, expected) in [
            ("cd projcets", "projects/"),
            ("cd projcets/", "projects/"),
            ("cd projcets/src/", "projects/src/"),
            ("cd projcets/src/pa", "projects/src/pa"),
        ] {
            let (_, output) = complete(root.path(), text, text.len());
            let first = output.candidates.first().expect(text);
            assert_eq!(first.display.primary, expected, "{text}");
            assert_eq!(first.action, CandidateAction::CorrectPath);
        }
        let text = "cd projcets/missing/";
        let (_, output) = complete(root.path(), text, text.len());
        assert_eq!(output.candidates.len(), 1);
        assert_eq!(output.candidates[0].display.primary, "./");
    }

    #[test]
    fn repairs_preserve_other_arguments_and_quote_paths() {
        let root = tempfile::tempdir().expect("root");
        fs::create_dir_all(root.path().join("my projects/src")).expect("tree");
        let text = "ls 'my projcets/src/' --color=always";
        let cursor = text.find(" --color").expect("cursor");
        let (context, output) = complete(root.path(), text, cursor);
        let repair = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "my projects/src/")
            .expect("repair");
        let activation = activate_candidate(repair, &context, &context.buffer).expect("activation");
        assert!(
            matches!(activation, Activation::ReplaceBuffer { text, .. } if text == "ls 'my projects/src/' --color=always")
        );
    }

    #[test]
    fn valid_prefixes_and_new_targets_do_not_become_repairs() {
        let root = tempfile::tempdir().expect("root");
        fs::create_dir(root.path().join("projects")).expect("projects");
        for text in [
            "cd pro",
            "mkdir projcets",
            "mkdir new/",
            "mkdir -p new/nested/",
            "mkdir --parents new/nested/",
            "echo > projcets",
        ] {
            let (_, output) = complete(root.path(), text, text.len());
            assert!(
                output.diagnostics.is_empty(),
                "{text}: {:?}",
                output.diagnostics
            );
            assert!(
                !output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.action == CandidateAction::CorrectPath),
                "{text}"
            );
        }
        for text in ["mkdir new/nested/", "mkdir -- -p/new/", "cat missing/file"] {
            let (_, output) = complete(root.path(), text, text.len());
            assert!(!output.diagnostics.is_empty(), "{text}");
        }
    }

    #[test]
    fn trailing_file_slash_is_repaired_only_for_file_slots() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("file.txt"), "fixture").expect("file");
        for (text, expected) in [("cat file.txt/", "file.txt"), ("cd file.txt/", "./")] {
            let (_, output) = complete(root.path(), text, text.len());
            assert_eq!(output.candidates[0].display.primary, expected);
            assert_eq!(output.candidates[0].action, CandidateAction::CorrectPath);
        }
    }

    #[test]
    fn distance_is_conservative_and_unicode_aware() {
        assert_eq!(spelling_distance("projcets", "projects"), Some(1));
        assert_eq!(spelling_distance("文档目录", "文件目錄"), None);
        assert_eq!(spelling_distance("文档目绿", "文档目录"), Some(1));
        assert_eq!(spelling_distance("ab", "ba"), None);
        assert_eq!(spelling_distance("project", "project"), None);
        assert_eq!(spelling_distance("completely", "different"), None);
        assert_eq!(spelling_distance(&"a".repeat(100), &"b".repeat(100)), None);
    }
}
