//! Shell alias / function / abbreviation discovery from rc files. Pure text
//! parsing — nothing is ever executed — with a fingerprint cache so the
//! completion worker re-reads rc files only when they change.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::UNIX_EPOCH,
};

use super::ShellKind;

const RC_FILE_MAX_BYTES: u64 = 1024 * 1024;
const MAX_ENTRIES: usize = 512;
/// Multi-line function bodies are accumulated only up to this size.
const FUNCTION_BODY_MAX_BYTES: usize = 4 * 1024;
/// `source` includes are followed exactly one level deep.
const MAX_INCLUDE_DEPTH: usize = 1;

type CachedAliases = HashMap<ShellKind, (Vec<Fingerprint>, Arc<ShellAliases>)>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AliasKind {
    Alias,
    Function,
    Abbreviation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AliasEntry {
    pub kind: AliasKind,
    /// The alias body (`ll` → `ls -la`); functions and abbreviations carry
    /// `None` when no single-line expansion is known.
    pub expansion: Option<String>,
    /// The raw function body when the definition was readable (single-line
    /// or up to the closing brace / `end`); used to infer argument slots.
    pub body: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingFunction {
    name: String,
    body: String,
    /// POSIX function-group brace depth. Fish uses `end` and keeps this at 0.
    brace_depth: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ShellAliases {
    entries: BTreeMap<String, AliasEntry>,
}

impl ShellAliases {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&AliasEntry> {
        self.entries.get(name)
    }

    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    #[must_use]
    pub fn has_longer_prefix(&self, prefix: &str) -> bool {
        self.entries
            .keys()
            .any(|name| name.len() > prefix.len() && name.starts_with(prefix))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Debug, Default)]
pub struct AliasCache {
    cached: Mutex<CachedAliases>,
    /// Test-only fixed set that bypasses rc discovery entirely.
    #[cfg(test)]
    fixed: Option<Arc<ShellAliases>>,
}

impl AliasCache {
    #[cfg(test)]
    pub(crate) fn new_fixed(aliases: ShellAliases) -> Self {
        Self {
            cached: Mutex::new(HashMap::new()),
            fixed: Some(Arc::new(aliases)),
        }
    }

    /// Aliases for the given shell, re-read when any rc file changed. The
    /// result is shared and immutable.
    pub fn load(&self, shell: ShellKind) -> Arc<ShellAliases> {
        #[cfg(test)]
        if let Some(fixed) = &self.fixed {
            return Arc::clone(fixed);
        }
        let files = rc_files(shell);
        let fingerprints: Vec<Fingerprint> = files.iter().map(|p| fingerprint(p)).collect();
        if let Ok(cache) = self.cached.lock()
            && let Some((cached_fp, aliases)) = cache.get(&shell)
            && *cached_fp == fingerprints
        {
            return Arc::clone(aliases);
        }
        let aliases = Arc::new(load_aliases(shell, &files));
        if let Ok(mut cache) = self.cached.lock() {
            cache.insert(shell, (fingerprints, Arc::clone(&aliases)));
        }
        aliases
    }
}

fn load_aliases(shell: ShellKind, files: &[PathBuf]) -> ShellAliases {
    let mut aliases = ShellAliases::default();
    for file in files {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        parse_rc_text(shell, &text, &mut aliases);
    }
    aliases
}

/// The rc files read for a shell, with one level of `source` includes
/// resolved. Missing files are kept (their fingerprint is simply "absent").
fn rc_files(shell: ShellKind) -> Vec<PathBuf> {
    let Some(home) = std::env::home_dir() else {
        return Vec::new();
    };
    let roots: Vec<PathBuf> = match shell {
        ShellKind::Zsh => {
            let zdotdir = std::env::var_os("ZDOTDIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.clone());
            vec![
                zdotdir.join(".zshenv"),
                zdotdir.join(".zprofile"),
                zdotdir.join(".zshrc"),
            ]
        }
        ShellKind::Bash => vec![
            home.join(".bashrc"),
            home.join(".bash_aliases"),
            home.join(".bash_profile"),
        ],
        ShellKind::Fish => {
            let config = std::env::var_os("XDG_CONFIG_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"));
            let fish = config.join("fish");
            let mut files = vec![fish.join("config.fish")];
            for directory in [fish.join("conf.d"), fish.join("functions")] {
                if let Ok(entries) = fs::read_dir(&directory) {
                    let mut paths: Vec<PathBuf> = entries
                        .flatten()
                        .map(|entry| entry.path())
                        .filter(|path| path.extension().is_some_and(|ext| ext == "fish"))
                        .collect();
                    paths.sort();
                    files.append(&mut paths);
                }
            }
            files
        }
    };
    let mut all = roots.clone();
    let mut depth = 0;
    let mut frontier = roots;
    while depth < MAX_INCLUDE_DEPTH && !frontier.is_empty() {
        depth += 1;
        let mut next = Vec::new();
        for file in &frontier {
            let Ok(text) = fs::read_to_string(file) else {
                continue;
            };
            for include in parse_includes(&text, file.parent().unwrap_or(Path::new(""))) {
                if !all.contains(&include) {
                    all.push(include.clone());
                    next.push(include);
                }
            }
        }
        frontier = next;
    }
    all
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Fingerprint {
    length: u64,
    modified_ns: u128,
}

fn fingerprint(path: &Path) -> Fingerprint {
    let metadata = fs::symlink_metadata(path);
    Fingerprint {
        length: metadata.as_ref().map_or(0, fs::Metadata::len),
        modified_ns: metadata
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_nanos()),
    }
}

/// `source file` / `. file` lines, with `~`, `$HOME`, and paths relative to
/// the sourcing file resolved. Anything else dynamic is skipped.
fn parse_includes(text: &str, base: &Path) -> Vec<PathBuf> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line
                .strip_prefix("source ")
                .or_else(|| line.strip_prefix(". "))?;
            let raw = rest.split_whitespace().next()?;
            let raw = raw.trim_matches(|c| c == '\'' || c == '"');
            if raw.contains('$') && !raw.starts_with("$HOME/") && !raw.starts_with("${HOME}/") {
                return None;
            }
            let expanded = raw
                .strip_prefix("~/")
                .map(|rest| std::env::home_dir().map(|home| home.join(rest)))
                .or_else(|| {
                    raw.strip_prefix("$HOME/")
                        .map(|rest| std::env::home_dir().map(|home| home.join(rest)))
                })
                .or_else(|| {
                    raw.strip_prefix("${HOME}/")
                        .map(|rest| std::env::home_dir().map(|home| home.join(rest)))
                })
                .flatten()
                .unwrap_or_else(|| base.join(raw));
            Some(expanded)
        })
        .collect()
}

pub(crate) fn parse_rc_text(shell: ShellKind, text: &str, aliases: &mut ShellAliases) {
    let mut end = text.len().min(RC_FILE_MAX_BYTES as usize);
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let text = &text[..end];
    let mut pending: Option<PendingFunction> = None;
    for line in text.lines() {
        if aliases.entries.len() >= MAX_ENTRIES {
            return;
        }
        let line = line.trim();
        if let Some(open) = pending.as_mut() {
            // Accumulating a multi-line function body until the closing
            // brace (posix) or `end` (fish).
            let close_at = match shell {
                ShellKind::Zsh | ShellKind::Bash => {
                    let (close_at, depth) = posix_function_close(line, open.brace_depth);
                    open.brace_depth = depth;
                    close_at
                }
                ShellKind::Fish => (line == "end" || line.starts_with("end ")).then_some(0),
            };
            if let Some(close_at) = close_at {
                let prefix = &line[..close_at];
                if open.body.len() + prefix.len() <= FUNCTION_BODY_MAX_BYTES {
                    if !open.body.is_empty() {
                        open.body.push('\n');
                    }
                    open.body.push_str(prefix.trim());
                }
                let open = pending.take().expect("checked above");
                insert(
                    aliases,
                    &open.name.clone(),
                    AliasEntry {
                        kind: AliasKind::Function,
                        expansion: None,
                        body: Some(open.body),
                    },
                );
            } else if open.body.len() + line.len() <= FUNCTION_BODY_MAX_BYTES {
                if !open.body.is_empty() {
                    open.body.push('\n');
                }
                open.body.push_str(line);
            }
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match shell {
            ShellKind::Zsh | ShellKind::Bash => {
                pending = parse_posix_line(line, aliases);
            }
            ShellKind::Fish => {
                pending = parse_fish_line(line, aliases);
            }
        }
    }
    // A function left open at EOF is still registered with what we read.
    if let Some(open) = pending.take() {
        insert(
            aliases,
            &open.name.clone(),
            AliasEntry {
                kind: AliasKind::Function,
                expansion: None,
                body: Some(open.body),
            },
        );
    }
}

fn parse_posix_line(line: &str, aliases: &mut ShellAliases) -> Option<PendingFunction> {
    if let Some(rest) = line.strip_prefix("alias ") {
        // Flag-bearing aliases (`alias -g`, `alias -s`) expand in positions
        // command completion does not cover — skip them.
        if rest.starts_with('-') {
            return None;
        }
        if let Some((name, expansion)) = rest.split_once('=') {
            let name = name.trim();
            if valid_name(name) {
                insert(
                    aliases,
                    name,
                    AliasEntry {
                        kind: AliasKind::Alias,
                        expansion: Some(unquote(expansion.trim())),
                        body: None,
                    },
                );
            }
        }
        return None;
    }
    // `foo() {`, `foo () {`, `function foo {`, `function foo() {`
    let rest = line
        .strip_prefix("function ")
        .map(str::trim_start)
        .unwrap_or(line);
    let name_candidate = rest
        .split([' ', '(', '{'])
        .next()
        .unwrap_or_default()
        .trim();
    let looks_like_function = line.starts_with("function ")
        || rest[name_candidate.len()..].trim_start().starts_with("()");
    if !looks_like_function || !valid_name(name_candidate) {
        return None;
    }
    let open_brace = rest.find('{')?;
    let after = &rest[open_brace + 1..];
    let (close_at, brace_depth) = posix_function_close(after, 1);
    if let Some(close_at) = close_at {
        // Single-line body: `proj() { cd ~/projects/$1; }`.
        insert(
            aliases,
            name_candidate,
            AliasEntry {
                kind: AliasKind::Function,
                expansion: None,
                body: Some(after[..close_at].trim().to_owned()),
            },
        );
        return None;
    }
    Some(PendingFunction {
        name: name_candidate.to_owned(),
        body: after.trim().to_owned(),
        brace_depth,
    })
}

/// Locate the closing reserved-word brace for a POSIX-style function group.
/// Braces inside quotes, parameter expansions, command substitutions, and
/// ordinary words such as brace expansions do not affect the group depth.
fn posix_function_close(line: &str, mut depth: usize) -> (Option<usize>, usize) {
    let bytes = line.as_bytes();
    let mut index = 0;
    let mut quote = None;
    while index < bytes.len() {
        match (quote, bytes[index]) {
            (Some(b'\''), b'\'') => quote = None,
            (Some(b'\''), _) => {}
            (Some(b'"'), b'"') => quote = None,
            (Some(b'"'), b'\\') => {
                index = (index + 2).min(bytes.len());
                continue;
            }
            (Some(b'"'), _) => {}
            (None, b'\'' | b'"') => quote = Some(bytes[index]),
            (None, b'\\') => {
                index = (index + 2).min(bytes.len());
                continue;
            }
            (None, b'#') if brace_word_boundary(bytes.get(index.wrapping_sub(1)).copied()) => break,
            (None, b'$') if bytes.get(index + 1) == Some(&b'{') => {
                index = skip_balanced(bytes, index + 2, b'{', b'}');
                continue;
            }
            (None, b'$') if bytes.get(index + 1) == Some(&b'(') => {
                index = skip_balanced(bytes, index + 2, b'(', b')');
                continue;
            }
            (None, brace @ (b'{' | b'}')) if is_reserved_brace(bytes, index) => {
                if brace == b'{' {
                    depth = depth.saturating_add(1);
                } else {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return (Some(index), 0);
                    }
                }
            }
            (None, _) => {}
            (Some(_), _) => {}
        }
        index += 1;
    }
    (None, depth)
}

fn skip_balanced(bytes: &[u8], mut index: usize, open: u8, close: u8) -> usize {
    let mut depth = 1_usize;
    let mut quote = None;
    while index < bytes.len() && depth > 0 {
        match (quote, bytes[index]) {
            (Some(b'\''), b'\'') => quote = None,
            (Some(b'\''), _) => {}
            (Some(b'"'), b'"') => quote = None,
            (Some(b'"'), b'\\') | (None, b'\\') => {
                index = (index + 2).min(bytes.len());
                continue;
            }
            (Some(b'"'), _) => {}
            (None, b'\'' | b'"') => quote = Some(bytes[index]),
            (None, byte) if byte == open => depth = depth.saturating_add(1),
            (None, byte) if byte == close => depth = depth.saturating_sub(1),
            (None, _) => {}
            (Some(_), _) => {}
        }
        index += 1;
    }
    index
}

fn is_reserved_brace(bytes: &[u8], index: usize) -> bool {
    brace_word_boundary(
        index
            .checked_sub(1)
            .and_then(|before| bytes.get(before).copied()),
    ) && brace_word_boundary(bytes.get(index + 1).copied())
}

fn brace_word_boundary(byte: Option<u8>) -> bool {
    byte.is_none_or(|byte| byte.is_ascii_whitespace() || b";|&()".contains(&byte))
}

fn parse_fish_line(line: &str, aliases: &mut ShellAliases) -> Option<PendingFunction> {
    if let Some(rest) = line.strip_prefix("alias ") {
        // fish: `alias ll 'ls -la'` or `alias ll='ls -la'`.
        if let Some((name, expansion)) = rest.split_once('=') {
            let name = name.trim();
            if valid_name(name) {
                insert(
                    aliases,
                    name,
                    AliasEntry {
                        kind: AliasKind::Alias,
                        expansion: Some(unquote(expansion.trim())),
                        body: None,
                    },
                );
            }
        } else {
            let mut words = rest.splitn(2, char::is_whitespace);
            let name = words.next().unwrap_or_default();
            let expansion = words.next().unwrap_or_default().trim();
            if valid_name(name) && !expansion.is_empty() {
                insert(
                    aliases,
                    name,
                    AliasEntry {
                        kind: AliasKind::Alias,
                        expansion: Some(unquote(expansion)),
                        body: None,
                    },
                );
            }
        }
        return None;
    }
    if let Some(rest) = line.strip_prefix("function ") {
        let name = rest
            .split([' ', ';', '\t'])
            .next()
            .unwrap_or_default()
            .trim();
        if valid_name(name) {
            let after = rest[name.len()..]
                .trim_start()
                .trim_start_matches(';')
                .trim();
            if after.ends_with("end") && after.len() > 3 {
                // Single-line: `function foo; echo hi; end`.
                let body = after[..after.len() - 3].trim_end_matches(';').trim();
                insert(
                    aliases,
                    name,
                    AliasEntry {
                        kind: AliasKind::Function,
                        expansion: None,
                        body: Some(body.to_owned()),
                    },
                );
                return None;
            }
            return Some(PendingFunction {
                name: name.to_owned(),
                body: String::new(),
                brace_depth: 0,
            });
        }
        return None;
    }
    if let Some(rest) = line.strip_prefix("abbr ") {
        // `abbr -a ll ls -la` / `abbr --add ll ls -la` / `abbr ll ls -la`.
        let mut words = rest.split_whitespace().peekable();
        while words.peek().is_some_and(|word| word.starts_with('-')) {
            words.next();
        }
        let name = words.next().unwrap_or_default();
        let expansion = words.collect::<Vec<_>>().join(" ");
        if valid_name(name) && !expansion.is_empty() {
            insert(
                aliases,
                name,
                AliasEntry {
                    kind: AliasKind::Abbreviation,
                    expansion: Some(unquote(&expansion)),
                    body: None,
                },
            );
        }
    }
    None
}

fn insert(aliases: &mut ShellAliases, name: &str, entry: AliasEntry) {
    // First definition wins, matching shell sourcing order.
    aliases.entries.entry(name.to_owned()).or_insert(entry);
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'')
            || (bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"'))
    {
        value[1..value.len() - 1].to_owned()
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(shell: ShellKind, text: &str) -> ShellAliases {
        let mut aliases = ShellAliases::default();
        parse_rc_text(shell, text, &mut aliases);
        aliases
    }

    #[test]
    fn parses_zsh_and_bash_alias_and_function_forms() {
        let aliases = parse(
            ShellKind::Zsh,
            "\
# a comment
alias ll='ls -lah'
alias gs=\"git status\"
alias g=git
alias -g L='| less'
mkcd() { mkdir -p \"$1\" && cd \"$1\"; }
function gco { git checkout \"$1\"; }
function bare() { true; }
export EDITOR=vim
",
        );
        assert_eq!(
            aliases.get("ll").expect("ll").expansion.as_deref(),
            Some("ls -lah")
        );
        assert_eq!(
            aliases.get("gs").expect("gs").expansion.as_deref(),
            Some("git status")
        );
        assert_eq!(
            aliases.get("g").expect("g").expansion.as_deref(),
            Some("git")
        );
        assert!(!aliases.contains("L"), "global aliases are skipped");
        assert_eq!(aliases.get("mkcd").expect("mkcd").kind, AliasKind::Function);
        assert_eq!(aliases.get("gco").expect("gco").kind, AliasKind::Function);
        assert_eq!(aliases.get("bare").expect("bare").kind, AliasKind::Function);
        assert!(!aliases.contains("EDITOR"));
    }

    #[test]
    fn multiline_parameter_expansions_do_not_end_posix_functions() {
        let aliases = parse(
            ShellKind::Zsh,
            r#"
proj() {
  if [ -n "$1" ]; then
    cd "${HOME}/projects/${1}"
  else
    cd "${HOME}/projects"
  fi
}
alias after='still parsed'
"#,
        );
        let body = aliases
            .get("proj")
            .and_then(|entry| entry.body.as_deref())
            .expect("complete proj body");
        assert!(
            body.contains("else"),
            "body ended at a parameter brace: {body}"
        );
        assert!(aliases.contains("after"));

        let slot = crate::shell::infer_function_slot(ShellKind::Zsh, body).expect("proj slot");
        assert_eq!(slot.kind, crate::completion::SlotKind::Directory);
        assert_eq!(
            slot.base,
            std::env::home_dir().map(|home| home.join("projects"))
        );
    }

    #[test]
    fn nested_command_groups_do_not_end_the_outer_function() {
        let aliases = parse(
            ShellKind::Bash,
            r#"
enter() {
  {
    echo preparing
  }
  cd "$HOME/work/$1"
}
"#,
        );
        let body = aliases
            .get("enter")
            .and_then(|entry| entry.body.as_deref())
            .expect("complete function body");
        assert!(body.contains("cd \"$HOME/work/$1\""));
        let slot = crate::shell::infer_function_slot(ShellKind::Bash, body).expect("enter slot");
        assert_eq!(slot.kind, crate::completion::SlotKind::Directory);
    }

    #[test]
    fn parses_fish_alias_function_and_abbr() {
        let aliases = parse(
            ShellKind::Fish,
            "\
alias ll 'ls -lah'
alias gs='git status'
function mkcd --description 'make and enter'
    mkdir -p $argv[1]; and cd $argv[1]
end
abbr -a gco git checkout
abbr --add gcm git commit -m
",
        );
        assert_eq!(
            aliases.get("ll").expect("ll").expansion.as_deref(),
            Some("ls -lah")
        );
        assert_eq!(
            aliases.get("gs").expect("gs").expansion.as_deref(),
            Some("git status")
        );
        assert_eq!(aliases.get("mkcd").expect("mkcd").kind, AliasKind::Function);
        assert_eq!(
            aliases.get("gco").expect("gco").expansion.as_deref(),
            Some("git checkout")
        );
        assert_eq!(
            aliases.get("gcm").expect("gcm").kind,
            AliasKind::Abbreviation
        );
    }

    #[test]
    fn rejects_malformed_names_and_skips_comments() {
        let aliases = parse(
            ShellKind::Zsh,
            "\
alias -x='nope'
alias 1bad='nope'
# alias commented='nope'
alias good='ok'
",
        );
        assert!(!aliases.contains("-x"));
        assert!(!aliases.contains("1bad"));
        assert!(!aliases.contains("commented"));
        assert!(aliases.contains("good"));
    }

    #[test]
    fn resolves_source_includes_one_level() {
        let home = std::env::home_dir().expect("home");
        let includes = parse_includes(
            "\
source ~/.aliases.zsh
. ./extra.zsh
source $HOME/shared.zsh
source ${HOME}/braced.zsh
source $RANDOM/dynamic.zsh
",
            Path::new("/configs"),
        );
        assert!(includes.contains(&home.join(".aliases.zsh")));
        assert!(includes.contains(&PathBuf::from("/configs/extra.zsh")));
        assert!(includes.contains(&home.join("shared.zsh")));
        assert!(includes.contains(&home.join("braced.zsh")));
        assert!(
            !includes
                .iter()
                .any(|path| path.display().to_string().contains("dynamic"))
        );
    }

    #[test]
    fn first_definition_wins_with_the_current_file_loading_policy() {
        let aliases = parse(ShellKind::Zsh, "alias ll='first'\nalias ll='second'\n");
        assert_eq!(
            aliases.get("ll").expect("ll").expansion.as_deref(),
            Some("first")
        );
    }

    #[test]
    fn rc_size_limit_never_slices_through_utf8() {
        let mut text = "x".repeat(RC_FILE_MAX_BYTES as usize - 1);
        text.push('界');
        text.push_str("\nalias after='ignored past the limit'\n");
        let aliases = parse(ShellKind::Zsh, &text);
        assert!(!aliases.contains("after"));
    }
}
