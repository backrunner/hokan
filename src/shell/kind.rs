use std::{env, fmt, path::PathBuf, str::FromStr};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ShellKind {
    Zsh,
    Bash,
    Fish,
}

impl ShellKind {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Zsh => "zsh",
            Self::Bash => "bash",
            Self::Fish => "fish",
        }
    }

    #[must_use]
    pub const fn exact_buffer_sync(self) -> bool {
        matches!(self, Self::Zsh)
    }

    pub fn detect() -> crate::Result<Self> {
        let shell = env::var_os("SHELL").ok_or_else(|| {
            crate::Error::Shell("$SHELL is not set; pass --shell zsh, bash, or fish".into())
        })?;
        let shell_path = PathBuf::from(shell);
        let name = shell_path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| crate::Error::Shell("$SHELL is not valid UTF-8".into()))?
            .trim_start_matches('-');
        name.parse()
    }
}

impl fmt::Display for ShellKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

impl FromStr for ShellKind {
    type Err = crate::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value
            .rsplit('/')
            .next()
            .unwrap_or(value)
            .trim_start_matches('-')
        {
            "zsh" => Ok(Self::Zsh),
            "bash" => Ok(Self::Bash),
            "fish" => Ok(Self::Fish),
            other => Err(crate::Error::Shell(format!(
                "unsupported shell {other:?}; expected zsh, bash, or fish"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_and_paths() {
        assert_eq!("zsh".parse::<ShellKind>().expect("zsh"), ShellKind::Zsh);
        assert_eq!(
            "/bin/bash".parse::<ShellKind>().expect("bash path"),
            ShellKind::Bash
        );
        assert!("nu".parse::<ShellKind>().is_err());
    }
}
