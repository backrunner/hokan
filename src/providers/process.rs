use std::{fs, os::unix::fs::MetadataExt, time::Duration};

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderDiagnostic, ProviderOutput,
        TextEdit,
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
        context.command() == Some("kill") && is_argument_position(context)
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
        let candidates = processes
            .into_iter()
            .filter(|process| process.pid != current_pid && process.ppid != current_pid)
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
                    CandidateAction::Insert,
                    CandidateSource::Process,
                    CandidateKind::Process,
                    Completeness::Runnable,
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

fn is_argument_position(context: &CompletionContext) -> bool {
    let Some(command) = context.parsed.tokens.iter().find(|token| {
        token.kind == crate::parser::TokenKind::Word
            && token.range.start >= context.parsed.active_segment.start
    }) else {
        return false;
    };
    context.buffer.cursor > command.range.end
        || context.buffer.text[..context.buffer.cursor]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
