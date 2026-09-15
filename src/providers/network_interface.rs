use std::{fs, sync::Arc, time::Duration};

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderDiagnostic, ProviderOutput,
        TextEdit,
    },
    platform::CommandPathCache,
    terminal::RiskLevel,
};

/// Interface membership changes rarely, but the `ifconfig`/`/sys` lookup
/// (250 ms bound) must not run per keystroke: one probe per TTL while an
/// interface slot is being typed. Failures are cached too.
const INTERFACE_CACHE_TTL: Duration = Duration::from_secs(2);

/// `(name, status)` rows for the interface list.
type InterfaceRows = Vec<(String, String)>;

pub struct NetworkInterfaceProvider {
    commands: Arc<CommandPathCache>,
    interfaces: super::TtlSlot<Result<InterfaceRows, String>>,
    list: fn() -> Result<InterfaceRows, String>,
}

impl NetworkInterfaceProvider {
    #[must_use]
    pub fn new(commands: Arc<CommandPathCache>) -> Self {
        Self {
            commands,
            interfaces: super::TtlSlot::new(INTERFACE_CACHE_TTL),
            list: interface_names,
        }
    }

    #[cfg(test)]
    fn with_loader(
        commands: Arc<CommandPathCache>,
        list: fn() -> Result<InterfaceRows, String>,
    ) -> Self {
        Self {
            list,
            ..Self::new(commands)
        }
    }
}

impl CandidateProvider for NetworkInterfaceProvider {
    fn id(&self) -> &'static str {
        "network_interface"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        context
            .command()
            .is_some_and(|command| self.commands.contains(command))
            && crate::providers::effective_command_accepts_external(context)
            && interface_slot(context)
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let interfaces = self.interfaces.get_or(|| (self.list)());
        let interfaces = match interfaces.as_ref() {
            Ok(interfaces) => interfaces,
            Err(error) => {
                return ProviderOutput {
                    candidates: Vec::new(),
                    diagnostics: vec![ProviderDiagnostic {
                        provider: self.id(),
                        code: "HK-NET-001",
                        level: crate::completion::DiagnosticLevel::Warning,
                        message: error.clone(),
                    }],
                };
            }
        };
        let candidates = interfaces
            .iter()
            .map(|(name, status)| {
                Candidate::new(
                    context.query_id,
                    name,
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

fn interface_names() -> Result<InterfaceRows, String> {
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

fn interface_slot(context: &CompletionContext) -> bool {
    if context.parsed.current_prefix.starts_with('-')
        || !crate::providers::effective_command_accepts_external(context)
    {
        return false;
    }
    let Some((words, position)) = crate::providers::argument_progress(context) else {
        return false;
    };
    match context.command() {
        Some("ifconfig") => {
            !ifconfig_flag_takes_value(words.get(position).copied().unwrap_or_default())
                && !has_ifconfig_interface_before(&words, position)
        }
        Some("ip") => {
            words.get(position).copied() == Some("dev")
                || ip_object_index(&words).is_some_and(|object| {
                    words.get(object).copied() == Some("link")
                        && words.get(object + 1).copied() == Some("show")
                        && position == object + 1
                })
        }
        _ => false,
    }
}

fn has_ifconfig_interface_before(words: &[&str], position: usize) -> bool {
    let mut index = 1;
    while index <= position {
        let word = words.get(index).copied().unwrap_or_default();
        if ifconfig_flag_takes_value(word) {
            index += 2;
        } else if word.starts_with('-') {
            index += 1;
        } else {
            return true;
        }
    }
    false
}

fn ifconfig_flag_takes_value(flag: &str) -> bool {
    matches!(flag, "-f" | "-g" | "-G")
}

fn ip_object_index(words: &[&str]) -> Option<usize> {
    let mut index = 1;
    while let Some(word) = words.get(index).copied() {
        if matches!(
            word,
            "-f" | "-family" | "-n" | "-netns" | "-b" | "-batch" | "-rcvbuf"
        ) {
            index += 2;
            continue;
        }
        if word.starts_with('-') {
            index += 1;
            continue;
        }
        return Some(index);
    }
    None
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

    fn context(text: &str) -> CompletionContext {
        CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from("/tmp"),
            BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer"),
        )
        .expect("context")
    }

    #[test]
    fn interfaces_only_appear_at_interface_slots() {
        for text in [
            "ifconfig ",
            "ifconfig en",
            "ifconfig -v en",
            "ifconfig -m en",
            "ip link show ",
            "ip -br link show ",
            "ip -family inet link show ",
            "ip -n testns link show ",
            "ip addr show dev ",
            "ip route add default dev ",
        ] {
            assert!(
                interface_slot(&context(text)),
                "expected interface slot for {text:?}"
            );
        }
        for text in [
            "ifconfig",
            "ifconfig en0 ",
            "ifconfig -f ",
            "ifconfig -g gr",
            "ip ",
            "ip addr ",
            "ip route show ",
            "builtin ifconfig ",
        ] {
            assert!(
                !interface_slot(&context(text)),
                "unexpected interface slot for {text:?}"
            );
        }
    }

    #[test]
    fn interface_list_is_cached_for_a_keystroke_burst() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static CALLS: AtomicUsize = AtomicUsize::new(0);
        fn counting_loader() -> Result<InterfaceRows, String> {
            CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(vec![("en0".to_owned(), "network interface".to_owned())])
        }

        let provider = NetworkInterfaceProvider::with_loader(
            Arc::new(CommandPathCache::default()),
            counting_loader,
        );
        for text in ["ifconfig ", "ifconfig e", "ifconfig en"] {
            let output = provider.complete(&context(text));
            assert_eq!(
                output.candidates.len(),
                1,
                "expected the cached interface row for {text:?}"
            );
        }
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            1,
            "a burst of interface-slot queries must share one probe"
        );
    }
}
