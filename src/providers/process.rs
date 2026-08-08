use std::{fs, os::unix::fs::MetadataExt, time::Duration};

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderDiagnostic, ProviderOutput,
        SlotKind, TextEdit,
    },
    terminal::RiskLevel,
};

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProcessInfo {
    pid: u32,
    ppid: u32,
    owner: String,
    command: String,
}

pub struct ProcessProvider;

impl CandidateProvider for ProcessProvider {
    fn id(&self) -> &'static str {
        "process"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        context.command() == Some("kill") && process_slot(context)
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let processes = match process_list() {
            Ok(processes) => processes,
            Err(error) => {
                return ProviderOutput {
                    candidates: Vec::new(),
                    diagnostics: vec![ProviderDiagnostic {
                        provider: self.id(),
                        code: "HK-PROC-001",
                        message: error,
                    }],
                };
            }
        };
        let current_pid = std::process::id();
        let prefix = context.parsed.current_prefix.as_str();
        let candidates = processes
            .into_iter()
            .filter(|process| process.pid != current_pid && process.ppid != current_pid)
            .filter(|process| process_matches(prefix, process))
            .take(500)
            .map(|process| {
                Candidate::new(
                    context.query_id,
                    format!("{} {}", process.pid, process.command),
                    format!("{} 拥有的进程", process.owner),
                    Some(TextEdit {
                        range: context.parsed.replacement.clone(),
                        replacement: process.pid.to_string(),
                        cursor_after: CursorPlacement::End,
                    }),
                    CandidateAction::InsertAndContinue {
                        next_slot: SlotKind::Process,
                    },
                    CandidateSource::Process,
                    CandidateKind::Process,
                    Completeness::NeedsInput {
                        slot: SlotKind::Process,
                    },
                    RiskLevel::Medium,
                    format!("process:{}", process.pid),
                )
            })
            .collect();
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }
}

fn process_matches(prefix: &str, process: &ProcessInfo) -> bool {
    if prefix.is_empty() {
        return true;
    }
    let pid = process.pid.to_string();
    let display = format!("{pid} {}", process.command);
    crate::completion::match_quality(prefix, &pid) > 0
        || crate::completion::match_quality(prefix, &display) > 0
}

fn process_list() -> Result<Vec<ProcessInfo>, String> {
    if cfg!(target_os = "linux") {
        linux_processes()
    } else {
        ps_processes()
    }
}

fn linux_processes() -> Result<Vec<ProcessInfo>, String> {
    let current_uid = nix::unistd::Uid::effective().as_raw();
    let entries = fs::read_dir("/proc").map_err(|error| error.to_string())?;
    let mut processes = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.uid() != current_uid {
            continue;
        }
        let command = fs::read_to_string(entry.path().join("comm"))
            .unwrap_or_default()
            .trim()
            .to_owned();
        if command.is_empty() {
            continue;
        }
        let ppid = fs::read_to_string(entry.path().join("status"))
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find_map(|line| line.strip_prefix("PPid:").map(str::trim))
                    .and_then(|value| value.parse::<u32>().ok())
            })
            .unwrap_or(0);
        processes.push(ProcessInfo {
            pid,
            ppid,
            owner: current_uid.to_string(),
            command,
        });
    }
    processes.sort_by_key(|process| process.pid);
    Ok(processes)
}

fn ps_processes() -> Result<Vec<ProcessInfo>, String> {
    let output = crate::platform::run_bounded(
        "ps",
        ["-axo", "pid=,ppid=,user=,comm="],
        Duration::from_millis(250),
        2 * 1024 * 1024,
    )?;
    if !output.status.success() {
        return Err(format!(
            "ps exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let current_user = std::env::var("USER").unwrap_or_default();
    let text = String::from_utf8_lossy(&output.stdout);
    let mut processes: Vec<_> = text.lines().filter_map(parse_ps_line).collect();
    if !current_user.is_empty() {
        processes.retain(|process| process.owner == current_user);
    }
    Ok(processes)
}

fn parse_ps_line(line: &str) -> Option<ProcessInfo> {
    let mut fields = line.split_whitespace();
    let pid = fields.next()?.parse().ok()?;
    let ppid = fields.next()?.parse().ok()?;
    let owner = fields.next()?.to_owned();
    let command = fields.collect::<Vec<_>>().join(" ");
    (!command.is_empty()).then_some(ProcessInfo {
        pid,
        ppid,
        owner,
        command,
    })
}

fn process_slot(context: &CompletionContext) -> bool {
    if context.parsed.current_prefix.starts_with('-') {
        return false;
    }
    let Some((words, position)) = crate::providers::argument_progress(context) else {
        return false;
    };
    let before = words.get(1..=position).unwrap_or_default();
    if before.iter().any(|word| matches!(*word, "-l" | "-L")) {
        return false;
    }
    !matches!(words.get(position).copied(), Some("-s" | "--signal" | "-n"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{
        completion::{BufferSnapshot, SyncQuality},
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };

    #[test]
    fn parses_ps_without_leaking_columns_into_pid() {
        assert_eq!(
            parse_ps_line("  42 7 alice /usr/bin/demo --flag"),
            Some(ProcessInfo {
                pid: 42,
                ppid: 7,
                owner: "alice".into(),
                command: "/usr/bin/demo --flag".into()
            })
        );
    }

    #[test]
    fn process_rows_only_appear_at_pid_slots() {
        let context = |text: &str| {
            CompletionContext::new(
                QueryId::new(1),
                ShellKind::Zsh,
                PathBuf::from("/tmp"),
                BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
                    .expect("buffer"),
            )
            .expect("context")
        };
        for text in ["kill ", "kill 12", "kill -9 ", "sudo kill -TERM "] {
            assert!(
                process_slot(&context(text)),
                "expected PID slot for {text:?}"
            );
        }
        for text in [
            "kill",
            "kill -",
            "kill -s ",
            "kill --signal TER",
            "kill -l ",
        ] {
            assert!(
                !process_slot(&context(text)),
                "unexpected PID slot for {text:?}"
            );
        }
    }

    #[test]
    fn process_cap_is_applied_after_matching_pid_or_command() {
        let mut processes: Vec<_> = (1..=550)
            .map(|pid| ProcessInfo {
                pid,
                ppid: 0,
                owner: "alice".into(),
                command: "ordinary".into(),
            })
            .collect();
        processes.push(ProcessInfo {
            pid: 9_999,
            ppid: 0,
            owner: "alice".into(),
            command: "release-worker".into(),
        });
        let matched: Vec<_> = processes
            .into_iter()
            .filter(|process| process_matches("release", process))
            .take(500)
            .collect();
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].pid, 9_999);
    }
}
