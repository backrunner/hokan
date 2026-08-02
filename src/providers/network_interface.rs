use std::{fs, time::Duration};

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderDiagnostic, ProviderOutput,
        TextEdit,
    },
    terminal::RiskLevel,
};

pub struct NetworkInterfaceProvider;

impl CandidateProvider for NetworkInterfaceProvider {
    fn id(&self) -> &'static str {
        "network_interface"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        matches!(context.command(), Some("ifconfig" | "ip")) && argument_position(context)
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let interfaces = match interface_names() {
            Ok(interfaces) => interfaces,
            Err(error) => {
                return ProviderOutput {
                    candidates: Vec::new(),
                    diagnostics: vec![ProviderDiagnostic {
                        provider: self.id(),
                        code: "HK-NET-001",
                        message: error,
                    }],
                };
            }
        };
        let candidates = interfaces
            .into_iter()
            .map(|(name, status)| {
                Candidate::new(
                    context.query_id,
                    &name,
                    status,
                    Some(TextEdit {
                        range: context.parsed.replacement.clone(),
                        replacement: name.clone(),
                        cursor_after: CursorPlacement::End,
                    }),
                    CandidateAction::Insert,
                    CandidateSource::NetworkInterface,
                    CandidateKind::Interface,
                    Completeness::Runnable,
                    RiskLevel::ReadOnly,
                    format!("interface:{name}"),
                )
            })
            .collect();
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }
}

fn interface_names() -> Result<Vec<(String, String)>, String> {
    if cfg!(target_os = "linux") {
        let entries = fs::read_dir("/sys/class/net").map_err(|error| error.to_string())?;
        let mut interfaces = Vec::new();
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let status = fs::read_to_string(entry.path().join("operstate"))
                .unwrap_or_else(|_| "unknown".into())
                .trim()
                .to_owned();
            interfaces.push((name, status));
        }
        interfaces.sort_by(|left, right| {
            (left.1.as_str() != "up", left.0.as_str())
                .cmp(&(right.1.as_str() != "up", right.0.as_str()))
        });
        return Ok(interfaces);
    }
    let output =
        crate::platform::run_bounded("ifconfig", ["-l"], Duration::from_millis(250), 256 * 1024)?;
    if !output.status.success() {
        return Err(format!("ifconfig -l exited with {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(|name| (name.to_owned(), "network interface".into()))
        .collect())
}

fn argument_position(context: &CompletionContext) -> bool {
    let word_count = context
        .parsed
        .tokens
        .iter()
        .filter(|token| {
            token.kind == crate::parser::TokenKind::Word
                && token.range.start >= context.parsed.active_segment.start
                && token.range.start <= context.buffer.cursor
        })
        .count();
    word_count >= 2
        || context.buffer.text[..context.buffer.cursor]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
}
