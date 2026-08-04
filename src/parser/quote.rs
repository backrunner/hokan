use super::QuoteContext;
use crate::shell::ShellKind;

#[must_use]
pub fn escape_for_shell(value: &str, context: QuoteContext, shell: ShellKind) -> String {
    match context {
        QuoteContext::Single => escape_single_fragment(value, shell),
        QuoteContext::Double => escape_double(value, shell),
        QuoteContext::Unquoted => escape_unquoted(value, shell),
        QuoteContext::Opaque => String::new(),
    }
}

fn escape_unquoted(value: &str, shell: ShellKind) -> String {
    if value.is_empty() {
        return "''".into();
    }
    // zsh EQUALS expansion (on by default): a word starting with `=` expands
    // to the path of the command named after it, so `=vim` must be quoted.
    let zsh_equals = matches!(shell, ShellKind::Zsh) && value.starts_with('=');
    if !zsh_equals
        && value
            .chars()
            .all(|character| character.is_alphanumeric() || "_+-./:@%=".contains(character))
    {
        return value.to_owned();
    }
    let escaped = match shell {
        ShellKind::Zsh | ShellKind::Bash => value.replace('\'', "'\\''"),
        ShellKind::Fish => value.replace('\\', "\\\\").replace('\'', "\\'"),
    };
    format!("'{escaped}'")
}

fn escape_single_fragment(value: &str, shell: ShellKind) -> String {
    match shell {
        ShellKind::Zsh | ShellKind::Bash => value.replace('\'', "'\\''"),
        ShellKind::Fish => value.replace('\\', "\\\\").replace('\'', "\\'"),
    }
}

fn escape_double(value: &str, shell: ShellKind) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        let special = match shell {
            ShellKind::Zsh | ShellKind::Bash => "\\\"$`".contains(character),
            ShellKind::Fish => "\\\"$".contains(character),
        };
        if special {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    #[test]
    fn separates_display_value_from_shell_escaping() {
        assert_eq!(
            escape_for_shell("two words", QuoteContext::Unquoted, ShellKind::Zsh),
            "'two words'"
        );
        assert_eq!(
            escape_for_shell("a'b", QuoteContext::Single, ShellKind::Bash),
            "a'\\''b"
        );
        assert_eq!(
            escape_for_shell("$HOME", QuoteContext::Double, ShellKind::Fish),
            "\\$HOME"
        );
    }

    #[test]
    fn zsh_quotes_leading_equals_words() {
        assert_eq!(
            escape_for_shell("=vim", QuoteContext::Unquoted, ShellKind::Zsh),
            "'=vim'"
        );
        // bash and fish have no EQUALS expansion: bare is safe there.
        assert_eq!(
            escape_for_shell("=vim", QuoteContext::Unquoted, ShellKind::Bash),
            "=vim"
        );
        assert_eq!(
            escape_for_shell("=vim", QuoteContext::Unquoted, ShellKind::Fish),
            "=vim"
        );
    }

    #[test]
    fn unquoted_words_round_trip_through_real_shells() {
        let fixtures = [
            "",
            "plain",
            "two words",
            "a'b",
            "double\"quote",
            "back\\slash",
            "$HOME",
            "中文 文件",
            "emoji-😀",
            "-leading",
            "=leading",
        ];
        for (shell, executable) in [
            (ShellKind::Bash, "bash"),
            (ShellKind::Zsh, "zsh"),
            (ShellKind::Fish, "fish"),
        ] {
            if !command_exists(executable) {
                continue;
            }
            for fixture in fixtures {
                let escaped = escape_for_shell(fixture, QuoteContext::Unquoted, shell);
                let script = match shell {
                    ShellKind::Bash | ShellKind::Zsh => {
                        format!("set -- {escaped}; printf %s \"$1\"")
                    }
                    ShellKind::Fish => format!("set value {escaped}; printf %s \"$value\""),
                };
                let output = Command::new(executable)
                    .arg("-c")
                    .arg(script)
                    .output()
                    .expect("shell round trip should start");
                assert!(output.status.success(), "{shell} rejected {escaped:?}");
                assert_eq!(output.stdout, fixture.as_bytes(), "{shell} for {fixture:?}");
            }
        }
    }

    fn command_exists(name: &str) -> bool {
        std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|directory| directory.join(name).is_file())
        })
    }
}
