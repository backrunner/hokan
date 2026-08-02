use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{safety::classify_command, shell::ShellKind, terminal::RiskLevel};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AiContext {
    pub request: String,
    pub os: &'static str,
    pub architecture: &'static str,
    pub shell: ShellKind,
    pub cwd_basename: Option<String>,
    pub project_kinds: Vec<&'static str>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AiCommand {
    pub command: String,
    pub explanation: String,
    #[serde(skip)]
    pub risk: Option<RiskLevel>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AiEnvelope {
    Object { commands: Vec<AiCommand> },
    Array(Vec<AiCommand>),
}

#[must_use]
pub fn build_context(
    request: &str,
    trigger_prefix: &str,
    shell: ShellKind,
    cwd: &Path,
    send_cwd_basename: bool,
) -> AiContext {
    let request = request
        .trim()
        .strip_prefix(trigger_prefix)
        .unwrap_or(request.trim())
        .trim()
        .to_owned();
    let project_kinds = [
        ("package.json", "node"),
        ("Cargo.toml", "rust"),
        ("pyproject.toml", "python"),
    ]
    .into_iter()
    .filter_map(|(file, kind)| cwd.join(file).exists().then_some(kind))
    .collect();
    AiContext {
        request,
        os: std::env::consts::OS,
        architecture: std::env::consts::ARCH,
        shell,
        cwd_basename: send_cwd_basename
            .then(|| cwd.file_name()?.to_str().map(str::to_owned))
            .flatten(),
        project_kinds,
    }
}

pub fn parse_ai_commands(content: &str) -> Result<Vec<AiCommand>, AiProtocolError> {
    if content.len() > 64 * 1024 {
        return Err(AiProtocolError::TooLarge);
    }
    let content = extract_json(content).ok_or(AiProtocolError::InvalidJson)?;
    let envelope: AiEnvelope =
        serde_json::from_str(content).map_err(|_| AiProtocolError::InvalidJson)?;
    let mut commands = match envelope {
        AiEnvelope::Object { commands } | AiEnvelope::Array(commands) => commands,
    };
    if commands.is_empty() || commands.len() > 5 {
        return Err(AiProtocolError::InvalidCount);
    }
    for item in &mut commands {
        if item.command.is_empty()
            || item.command.len() > 2_048
            || item.command.contains(['\0', '\n', '\r', '\u{1b}'])
            || item.command.chars().any(|character| {
                character.is_control() || ('\u{80}'..='\u{9f}').contains(&character)
            })
        {
            return Err(AiProtocolError::InvalidCommand);
        }
        if item.explanation.is_empty()
            || item.explanation.len() > 500
            || item.explanation.chars().any(char::is_control)
        {
            return Err(AiProtocolError::InvalidExplanation);
        }
        crate::parser::parse_line(&item.command, item.command.len())
            .map_err(|_| AiProtocolError::InvalidCommand)?;
        item.risk = Some(classify_command(&item.command).level);
    }
    Ok(commands)
}

fn extract_json(content: &str) -> Option<&str> {
    let trimmed = content.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return Some(trimmed);
    }
    let fenced = trimmed.strip_prefix("```json")?.strip_suffix("```")?;
    Some(fenced.trim())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AiProtocolError {
    TooLarge,
    InvalidJson,
    InvalidCount,
    InvalidCommand,
    InvalidExplanation,
}

impl std::fmt::Display for AiProtocolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::TooLarge => "AI response exceeded 64 KiB",
            Self::InvalidJson => "AI response was not the required JSON shape",
            Self::InvalidCount => "AI response must contain 1 to 5 commands",
            Self::InvalidCommand => "AI response contained an unsafe command value",
            Self::InvalidExplanation => "AI response contained an invalid explanation",
        };
        formatter.write_str(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_strict_json_and_rejects_terminal_controls() {
        let commands = parse_ai_commands(
            r#"{"commands":[{"command":"find . -name '*.rs'","explanation":"查找 Rust 文件"}]}"#,
        )
        .expect("valid response");
        assert_eq!(commands.len(), 1);
        assert!(commands[0].risk.is_some());
        assert!(
            parse_ai_commands(r#"[{"command":"echo \u001b[31m","explanation":"bad"}]"#).is_err()
        );
        assert!(
            parse_ai_commands(r#"[{"command":"echo one\necho two","explanation":"bad"}]"#).is_err()
        );
    }

    #[test]
    fn accepts_one_strict_json_fence_but_rejects_other_markdown() {
        let commands = parse_ai_commands(
            "```json\n[{\"command\":\"pwd\",\"explanation\":\"print directory\"}]\n```",
        )
        .expect("strict JSON fence");
        assert_eq!(commands.len(), 1);
        assert!(parse_ai_commands("Here is a command: `pwd`").is_err());
        assert!(parse_ai_commands("```sh\npwd\n```").is_err());
    }

    #[test]
    fn enforces_count_and_field_limits() {
        assert!(parse_ai_commands("{\"commands\":[]}").is_err());
        let six = serde_json::to_string(
            &(0..6)
                .map(|_| serde_json::json!({"command": "pwd", "explanation": "pwd"}))
                .collect::<Vec<_>>(),
        )
        .expect("JSON");
        assert!(parse_ai_commands(&six).is_err());
        let response = serde_json::json!([{
            "command": "x".repeat(2_049),
            "explanation": "too long"
        }]);
        assert!(parse_ai_commands(&response.to_string()).is_err());
    }
}
