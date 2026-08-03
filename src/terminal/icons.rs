//! Nerd Font icons for overlay rows, ported from a subset of iris
//! (<https://github.com/versenilvis/iris>, `integration/icons.go`).
//!
//! Glyphs come from the Nerd Font private-use ranges; terminals without a
//! Nerd Font show placeholder boxes, so `ui.nerd_fonts = false` disables the
//! icon column entirely.

/// Glyph for candidates whose command word starts with a digit (iris reuses
/// its history clock glyph, U+F1DA).
const DIGIT_GLYPH: &str = "\u{f1da}";

/// Glyph used when no icon matches (U+276F, heavy right-pointing angle
/// quotation mark ornament — present in regular fonts).
pub const FALLBACK_GLYPH: &str = "\u{276f}";

/// Look up the Nerd Font icon for a command word (the first word of a
/// candidate's primary text). Matching is case-insensitive and ignores any
/// directory prefix, mirroring iris's `lookupIcon`.
#[must_use]
pub fn lookup_icon(word: &str) -> &'static str {
    let lowered = word.trim().to_ascii_lowercase();
    let key = lowered.rsplit('/').next().unwrap_or(lowered.as_str());
    if key.is_empty() {
        return FALLBACK_GLYPH;
    }
    match key {
        "git" => "󰊢",
        "docker" => "",
        "docker-compose" => "",
        "podman" => "󰡨",
        "python" => "",
        "python3" => "",
        "pip" => "",
        "pip3" => "",
        "pipx" => "",
        "node" => "",
        "npm" => "",
        "npx" => "",
        "pnpm" => "",
        "bun" => "",
        "yarn" => "",
        "rust" => "",
        "cargo" => "",
        "rustup" => "",
        "rustc" => "",
        "go" => "",
        "java" => "",
        "mvn" => "",
        "gradle" => "",
        "ruby" => "",
        "gem" => "",
        "php" => "",
        "lua" => "",
        "vim" => "",
        "nvim" => "",
        "vi" => "",
        "emacs" => "",
        "nano" => "󰔷",
        "code" => "󰨞",
        "cd" => "",
        "ls" => "",
        "eza" => "",
        "tree" => "",
        "pwd" => "",
        "cat" => "",
        "bat" => "",
        "less" => "",
        "head" => "",
        "tail" => "",
        "grep" => "",
        "rg" => "",
        "find" => "",
        "fd" => "",
        "ssh" => "󰣀",
        "scp" => "󰣀",
        "rsync" => "󰣀",
        "kubectl" => "󱃾",
        "helm" => "󱃾",
        "k9s" => "󱃾",
        "terraform" => "󱁢",
        "aws" => "󰸏",
        "gcloud" => "󱇶",
        "tmux" => "󰓓",
        "curl" => "󰌗",
        "wget" => "󰌗",
        "tar" => "",
        "zip" => "",
        "unzip" => "",
        "make" => "󰷈",
        "cmake" => "󰷈",
        "just" => "󰷈",
        "ffmpeg" => "󰕼",
        "ollama" => "󰫢",
        "ai" => "󰫢",
        "systemctl" => "󰒓",
        "htop" => "󰍛",
        "btop" => "󰍛",
        "top" => "󰍛",
        "mysql" => "",
        "psql" => "",
        "redis-cli" => "󰆧",
        "sqlite3" => "",
        "nginx" => "󰟀",
        "history" => "",
        _ => {
            if key.bytes().next().is_some_and(|byte| byte.is_ascii_digit()) {
                DIGIT_GLYPH
            } else {
                FALLBACK_GLYPH
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_commands_have_icons() {
        assert_eq!(lookup_icon("git"), "\u{f02a2}");
        assert_ne!(lookup_icon("Cargo"), FALLBACK_GLYPH);
        assert_ne!(lookup_icon("/usr/bin/kubectl"), FALLBACK_GLYPH);
    }

    #[test]
    fn unknown_and_digit_words_fall_back() {
        assert_eq!(lookup_icon("definitely-not-a-command"), FALLBACK_GLYPH);
        assert_eq!(lookup_icon(""), FALLBACK_GLYPH);
        assert_eq!(lookup_icon("7z"), DIGIT_GLYPH);
    }
}
