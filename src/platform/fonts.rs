//! Nerd Font availability probing and bootstrap installation.
//!
//! A terminal cannot be asked which font it renders with, so coverage is
//! best-effort: terminals that bundle the Nerd Font symbols set, then the
//! configured font when its configuration is readable, then the fonts
//! installed in the standard font directories.

use std::{
    collections::BTreeSet,
    env,
    ffi::OsStr,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use regex::Regex;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use super::run_bounded;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const COMMAND_MAX_OUTPUT: usize = 512 * 1024;
const MAX_FONT_BYTES: usize = 8 * 1024 * 1024;
const MIN_FONT_BYTES: usize = 64 * 1024;
const FONT_SCAN_DEPTH: usize = 4;
const FONT_SCAN_LIMIT: usize = 64;

const SYMBOLS_BASE_URL: &str = "https://raw.githubusercontent.com/ryanoasis/nerd-fonts/v3.5.1/patched-fonts/NerdFontsSymbolsOnly";

struct FontAsset<'a> {
    name: &'a str,
    sha256: &'a str,
}

/// The Nerd Fonts Symbols-Only package (v3.5.1): it covers the private-use
/// glyphs without replacing the terminal's configured font — terminals pick
/// it up through font fallback.
const SYMBOLS_FONTS: [FontAsset; 2] = [
    FontAsset {
        name: "SymbolsNerdFont-Regular.ttf",
        sha256: "2839f0a572d4559f3f17a6fb74b8772e183f0c0a47150998ab194932cad55829",
    },
    FontAsset {
        name: "SymbolsNerdFontMono-Regular.ttf",
        sha256: "fe471e538392f51910faab985fa8e192a39dd3426125edd15b71b3680df0e749",
    },
];

/// Whether the overlay's icon column can expect Nerd Font glyph coverage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NerdFontCoverage {
    /// The terminal provides Nerd Font glyphs itself or its configured font
    /// is a Nerd Font; the detail names the source.
    Covered(String),
    /// A Nerd Font is installed system-wide but is not confirmed as the
    /// terminal font; terminals normally pick it up via font fallback.
    Installed(Vec<String>),
    /// No Nerd Font was found anywhere the probe can look.
    Missing,
    /// Remote session: glyphs render with the client terminal's font, which
    /// cannot be inspected from this host.
    RemoteClient,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NerdFontProbe {
    pub coverage: NerdFontCoverage,
    /// Terminal-specific hint, e.g. kitty's `symbol_map`.
    pub note: Option<String>,
}

/// Outcome of [`ensure_nerd_font`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FontSetup {
    /// Coverage already exists; nothing was downloaded.
    AlreadyCovered(String),
    /// These font files were installed.
    Installed(Vec<PathBuf>),
    /// Nothing to do on this host (e.g. a remote session).
    Skipped(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Terminal {
    ITerm2,
    Apple,
    VsCode,
    Zed,
    WezTerm,
    Warp,
    Ghostty,
    Kitty,
    Alacritty,
    Unknown,
}

impl Terminal {
    /// WezTerm, Warp, and Ghostty ship a Nerd Font symbols fallback, so the
    /// icon column renders regardless of the configured font.
    fn bundles_nerd_font(self) -> bool {
        matches!(self, Self::WezTerm | Self::Warp | Self::Ghostty)
    }

    fn name(self) -> &'static str {
        match self {
            Self::ITerm2 => "iTerm2",
            Self::Apple => "Terminal.app",
            Self::VsCode => "VS Code",
            Self::Zed => "Zed",
            Self::WezTerm => "WezTerm",
            Self::Warp => "Warp",
            Self::Ghostty => "Ghostty",
            Self::Kitty => "kitty",
            Self::Alacritty => "Alacritty",
            Self::Unknown => "the terminal",
        }
    }

    /// Hint shown when the terminal font is not confirmed to be a Nerd Font.
    fn font_hint(self) -> Option<String> {
        match self {
            Self::Kitty => Some(
                "kitty picks fallback glyphs through fontconfig; if icons still render as boxes add `symbol_map U+E000-U+F8FF Symbols Nerd Font` to kitty.conf"
                    .to_owned(),
            ),
            Self::ITerm2 => Some(
                "iTerm2 may need a Nerd Font under profile > text > non-ASCII font".to_owned(),
            ),
            _ => None,
        }
    }
}

/// Probes Nerd Font coverage for the current process environment.
pub fn probe() -> NerdFontProbe {
    let home = env::home_dir().unwrap_or_default();
    probe_with(
        &|key| env::var(key).ok(),
        &home,
        env::consts::OS,
        &probe_command,
    )
}

/// Ensures a Nerd Font is available to the terminal, installing the
/// Symbols-Only package into the user font directory when none is found.
/// Never fails the caller's workflow — the caller reports the outcome.
pub fn ensure_nerd_font() -> Result<FontSetup, String> {
    match probe().coverage {
        NerdFontCoverage::Covered(detail) => Ok(FontSetup::AlreadyCovered(detail)),
        NerdFontCoverage::Installed(names) => {
            let mut preview: Vec<&str> = names.iter().take(3).map(String::as_str).collect();
            if names.len() > 3 {
                preview.push("…");
            }
            Ok(FontSetup::AlreadyCovered(format!(
                "installed Nerd Font: {}",
                preview.join(", ")
            )))
        }
        NerdFontCoverage::RemoteClient => Ok(FontSetup::Skipped(
            "remote session — install a Nerd Font in the local terminal".to_owned(),
        )),
        NerdFontCoverage::Missing => {
            let home = env::home_dir().ok_or("cannot resolve the home directory")?;
            let dir = user_font_dir(&home, env::consts::OS)
                .ok_or_else(|| format!("no user font directory on {}", env::consts::OS))?;
            let files = install_symbols_only(&dir, fetch_symbols_font)?;
            refresh_font_cache(&dir);
            Ok(FontSetup::Installed(files))
        }
    }
}

/// Downloads and installs the pinned Symbols-Only Nerd Font files into
/// `fonts_dir`. Files that already match are left untouched.
fn install_symbols_only<F>(fonts_dir: &Path, fetch: F) -> Result<Vec<PathBuf>, String>
where
    F: FnMut(&str) -> Result<Vec<u8>, String>,
{
    install_fonts(fonts_dir, &SYMBOLS_FONTS, fetch)
}

fn install_fonts<F>(
    fonts_dir: &Path,
    assets: &[FontAsset<'_>],
    mut fetch: F,
) -> Result<Vec<PathBuf>, String>
where
    F: FnMut(&str) -> Result<Vec<u8>, String>,
{
    fs::create_dir_all(fonts_dir)
        .map_err(|error| format!("cannot create {}: {error}", fonts_dir.display()))?;
    let mut installed = Vec::new();
    for asset in assets {
        let target = fonts_dir.join(asset.name);
        let bytes = fetch(asset.name)?;
        verify_font(asset, &bytes)?;
        if fs::read(&target).is_ok_and(|existing| existing == bytes) {
            installed.push(target);
            continue;
        }
        let mut temporary = NamedTempFile::new_in(fonts_dir)
            .map_err(|error| format!("cannot write {}: {error}", target.display()))?;
        temporary
            .write_all(&bytes)
            .map_err(|error| format!("cannot write {}: {error}", target.display()))?;
        temporary
            .persist(&target)
            .map_err(|error| format!("cannot install {}: {}", target.display(), error.error))?;
        installed.push(target);
    }
    Ok(installed)
}

fn verify_font(asset: &FontAsset<'_>, bytes: &[u8]) -> Result<(), String> {
    if !(MIN_FONT_BYTES..=MAX_FONT_BYTES).contains(&bytes.len()) {
        return Err(format!("{} has an unexpected size", asset.name));
    }
    if format!("{:x}", Sha256::digest(bytes)) != asset.sha256 {
        return Err(format!("{} failed its SHA-256 check", asset.name));
    }
    Ok(())
}

fn fetch_symbols_font(name: &str) -> Result<Vec<u8>, String> {
    let client = crate::update::download_client().map_err(|error| error.to_string())?;
    let url = format!("{SYMBOLS_BASE_URL}/{name}");
    crate::update::block_on(crate::update::download(&client, &url, MAX_FONT_BYTES))
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

/// Refreshes fontconfig where it exists (Linux); a no-op elsewhere.
fn refresh_font_cache(dir: &Path) {
    let _ = run_bounded(
        "fc-cache",
        [OsStr::new("-f"), dir.as_os_str()],
        Duration::from_secs(15),
        1024,
    );
}

fn probe_command(program: &str, args: &[&str]) -> Option<String> {
    run_bounded(
        program,
        args.iter().map(OsStr::new),
        COMMAND_TIMEOUT,
        COMMAND_MAX_OUTPUT,
    )
    .ok()
    .filter(|output| output.status.success())
    .and_then(|output| String::from_utf8(output.stdout).ok())
}

fn probe_with(
    get_env: &dyn Fn(&str) -> Option<String>,
    home: &Path,
    os: &'static str,
    run_command: &dyn Fn(&str, &[&str]) -> Option<String>,
) -> NerdFontProbe {
    if get_env("SSH_CONNECTION").is_some() || get_env("SSH_TTY").is_some() {
        return NerdFontProbe {
            coverage: NerdFontCoverage::RemoteClient,
            note: None,
        };
    }
    let terminal = terminal_kind(get_env);
    if terminal.bundles_nerd_font() {
        return NerdFontProbe {
            coverage: NerdFontCoverage::Covered(format!(
                "{} bundles Nerd Font symbols",
                terminal.name()
            )),
            note: None,
        };
    }
    let configured = configured_fonts(terminal, home, os, get_env, run_command).unwrap_or_default();
    if let Some(font) = configured.iter().find(|name| is_nerd_font(name)) {
        return NerdFontProbe {
            coverage: NerdFontCoverage::Covered(format!("terminal font '{font}'")),
            note: None,
        };
    }
    let installed = installed_nerd_fonts(home, os);
    if !installed.is_empty() {
        return NerdFontProbe {
            coverage: NerdFontCoverage::Installed(installed),
            note: terminal.font_hint(),
        };
    }
    NerdFontProbe {
        coverage: NerdFontCoverage::Missing,
        note: terminal.font_hint(),
    }
}

fn terminal_kind(get_env: &dyn Fn(&str) -> Option<String>) -> Terminal {
    if let Some(program) = get_env("TERM_PROGRAM") {
        match program.as_str() {
            "iTerm.app" => return Terminal::ITerm2,
            "Apple_Terminal" => return Terminal::Apple,
            "vscode" => return Terminal::VsCode,
            "WezTerm" => return Terminal::WezTerm,
            "WarpTerminal" => return Terminal::Warp,
            "ghostty" => return Terminal::Ghostty,
            "zed" => return Terminal::Zed,
            _ => {}
        }
    }
    if get_env("KITTY_WINDOW_ID").is_some()
        || get_env("TERM").is_some_and(|term| term.contains("kitty"))
    {
        return Terminal::Kitty;
    }
    if get_env("WEZTERM_EXECUTABLE").is_some() {
        return Terminal::WezTerm;
    }
    if get_env("ALACRITTY_SOCKET").is_some()
        || get_env("ALACRITTY_WINDOW_ID").is_some()
        || get_env("TERM").is_some_and(|term| term.starts_with("alacritty"))
    {
        return Terminal::Alacritty;
    }
    Terminal::Unknown
}

fn configured_fonts(
    terminal: Terminal,
    home: &Path,
    os: &str,
    get_env: &dyn Fn(&str) -> Option<String>,
    run_command: &dyn Fn(&str, &[&str]) -> Option<String>,
) -> Option<Vec<String>> {
    match terminal {
        Terminal::ITerm2 => iterm_fonts(run_command),
        Terminal::Kitty => kitty_fonts(&config_root(home, get_env)),
        Terminal::Alacritty => alacritty_fonts(&config_root(home, get_env)),
        Terminal::VsCode => vscode_fonts(home, os),
        Terminal::Zed => zed_fonts(&config_root(home, get_env)),
        _ => None,
    }
}

fn config_root(home: &Path, get_env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    get_env("XDG_CONFIG_HOME")
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
}

/// iTerm2 profile fonts live in the `New Bookmarks` defaults domain as
/// `"Normal Font" = "Name <size>";` plus an optional non-ASCII font.
fn iterm_fonts(run_command: &dyn Fn(&str, &[&str]) -> Option<String>) -> Option<Vec<String>> {
    let output = run_command(
        "defaults",
        &["read", "com.googlecode.iterm2", "New Bookmarks"],
    )?;
    let pattern = Regex::new(r#""(?:Normal Font|Non Ascii Font)" = "([^"]+)""#).ok()?;
    let fonts: Vec<String> = pattern
        .captures_iter(&output)
        .map(|captures| captures[1].to_owned())
        .collect();
    (!fonts.is_empty()).then_some(fonts)
}

/// kitty.conf `font_family`/`symbol_map` entries.
fn kitty_fonts(config_root: &Path) -> Option<Vec<String>> {
    let contents = fs::read_to_string(config_root.join("kitty/kitty.conf")).ok()?;
    let mut fonts = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let value = value.trim();
        match key {
            "font_family" | "bold_font" | "italic_font" | "bold_italic_font" => {
                fonts.push(value.to_owned());
            }
            // `symbol_map <codepoint-ranges> <font name>`
            "symbol_map" => {
                if let Some((_, font)) = value.split_once(char::is_whitespace) {
                    fonts.push(font.trim().to_owned());
                }
            }
            _ => {}
        }
    }
    (!fonts.is_empty()).then_some(fonts)
}

/// alacritty.toml (and legacy .yml) `font.*.family` values.
fn alacritty_fonts(config_root: &Path) -> Option<Vec<String>> {
    let base = config_root.join("alacritty");
    let mut fonts = Vec::new();
    if let Ok(contents) = fs::read_to_string(base.join("alacritty.toml")) {
        let pattern = Regex::new(r#"family\s*=\s*"([^"]+)""#).ok()?;
        fonts.extend(
            pattern
                .captures_iter(&contents)
                .map(|captures| captures[1].to_owned()),
        );
    }
    for legacy in ["alacritty.yml", "alacritty.yaml"] {
        if let Ok(contents) = fs::read_to_string(base.join(legacy)) {
            for line in contents.lines() {
                let line = line.trim();
                if let Some((key, value)) = line.split_once(':')
                    && key.trim() == "family"
                {
                    fonts.push(value.trim().trim_matches('"').to_owned());
                }
            }
        }
    }
    (!fonts.is_empty()).then_some(fonts)
}

/// VS Code uses `terminal.integrated.fontFamily`, falling back to
/// `editor.fontFamily`. settings.json allows comments, so scan the text
/// instead of parsing JSON.
fn vscode_fonts(home: &Path, os: &str) -> Option<Vec<String>> {
    let settings = match os {
        "macos" => home.join("Library/Application Support/Code/User/settings.json"),
        "windows" => home.join("AppData/Roaming/Code/User/settings.json"),
        _ => home.join(".config/Code/User/settings.json"),
    };
    let contents = fs::read_to_string(settings).ok()?;
    let value = |key: &str| {
        Regex::new(&format!(r#""{key}"\s*:\s*"([^"]+)""#))
            .ok()?
            .captures(&contents)
            .map(|captures| captures[1].to_owned())
    };
    let fonts: Vec<String> = value("terminal.integrated.fontFamily")
        .into_iter()
        .chain(value("editor.fontFamily"))
        .collect();
    (!fonts.is_empty()).then_some(fonts)
}

/// Zed's `terminal.font_family` (a plain `font_family` key only appears
/// under the terminal section).
fn zed_fonts(config_root: &Path) -> Option<Vec<String>> {
    let contents = fs::read_to_string(config_root.join("zed/settings.json")).ok()?;
    let pattern = Regex::new(r#""font_family"\s*:\s*"([^"]+)""#).ok()?;
    let fonts: Vec<String> = pattern
        .captures_iter(&contents)
        .map(|captures| captures[1].to_owned())
        .collect();
    (!fonts.is_empty()).then_some(fonts)
}

/// Nerd Font names carry a `nerd` component or a standalone `nf` token
/// (`MesloLGS-NF`, `SymbolsNerdFontMono-Regular`, `Hack Nerd Font Mono`).
pub(crate) fn is_nerd_font(name: &str) -> bool {
    name.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|token| token.contains("nerd") || token == "nf")
}

fn installed_nerd_fonts(home: &Path, os: &str) -> Vec<String> {
    let mut fonts = BTreeSet::new();
    for dir in font_dirs(home, os) {
        scan_font_dir(&dir, FONT_SCAN_DEPTH, &mut fonts);
        if fonts.len() >= FONT_SCAN_LIMIT {
            break;
        }
    }
    fonts.into_iter().collect()
}

fn font_dirs(home: &Path, os: &str) -> Vec<PathBuf> {
    match os {
        "macos" => vec![
            home.join("Library/Fonts"),
            PathBuf::from("/Library/Fonts"),
            PathBuf::from("/System/Library/Fonts"),
        ],
        "windows" => vec![home.join("AppData/Local/Microsoft/Windows/Fonts")],
        _ => vec![
            home.join(".local/share/fonts"),
            home.join(".fonts"),
            PathBuf::from("/usr/local/share/fonts"),
            PathBuf::from("/usr/share/fonts"),
        ],
    }
}

fn user_font_dir(home: &Path, os: &str) -> Option<PathBuf> {
    match os {
        "macos" => Some(home.join("Library/Fonts")),
        "linux" | "freebsd" | "openbsd" | "netbsd" => Some(home.join(".local/share/fonts")),
        _ => None,
    }
}

fn scan_font_dir(dir: &Path, depth: usize, fonts: &mut BTreeSet<String>) {
    if depth == 0 || fonts.len() >= FONT_SCAN_LIMIT {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if !entry.file_name().to_string_lossy().starts_with('.') {
                scan_font_dir(&path, depth - 1, fonts);
            }
        } else if is_font_file(&path)
            && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
            && is_nerd_font(stem)
        {
            fonts.insert(stem.to_owned());
        }
        if fonts.len() >= FONT_SCAN_LIMIT {
            return;
        }
    }
}

fn is_font_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "ttf" | "otf" | "ttc"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_fn<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    fn no_command(_: &str, _: &[&str]) -> Option<String> {
        None
    }

    #[test]
    fn nerd_font_names_are_recognized() {
        for name in [
            "SymbolsNerdFont-Regular",
            "SymbolsNerdFontMono-Regular",
            "MesloLGS-NF-Regular",
            "Hack Nerd Font Mono",
            "JetBrainsMono Nerd Font",
            "0xProto Nerd Font Mono",
        ] {
            assert!(is_nerd_font(name), "{name}");
        }
        for name in [
            "SF Mono",
            "Monaco",
            "DejaVuSansMono",
            "FantasqueSansMono",
            "Courier New",
        ] {
            assert!(!is_nerd_font(name), "{name}");
        }
    }

    #[test]
    fn ssh_sessions_are_remote() {
        let env = env_fn(&[("SSH_TTY", "/dev/ttys001")]);
        let probe = probe_with(&env, Path::new("/nonexistent"), "macos", &no_command);
        assert_eq!(probe.coverage, NerdFontCoverage::RemoteClient);
    }

    #[test]
    fn bundled_terminals_are_covered() {
        for (key, value) in [
            ("TERM_PROGRAM", "WezTerm"),
            ("TERM_PROGRAM", "WarpTerminal"),
            ("TERM_PROGRAM", "ghostty"),
            ("WEZTERM_EXECUTABLE", "/opt/wezterm"),
        ] {
            let vars = [(key, value)];
            let env = env_fn(&vars);
            let probe = probe_with(&env, Path::new("/nonexistent"), "macos", &no_command);
            assert!(
                matches!(probe.coverage, NerdFontCoverage::Covered(_)),
                "{key}={value}: {:?}",
                probe.coverage
            );
        }
    }

    #[test]
    fn installed_font_files_count_as_coverage() {
        let home = tempfile::tempdir().expect("home");
        let fonts = home.path().join("Library/Fonts");
        fs::create_dir_all(&fonts).expect("fonts dir");
        fs::write(fonts.join("SymbolsNerdFont-Regular.ttf"), b"font").expect("font file");
        fs::write(fonts.join("Monaco.ttf"), b"font").expect("font file");

        let env = env_fn(&[("TERM_PROGRAM", "Apple_Terminal")]);
        let probe = probe_with(&env, home.path(), "macos", &no_command);
        assert_eq!(
            probe.coverage,
            NerdFontCoverage::Installed(vec!["SymbolsNerdFont-Regular".to_owned()])
        );
    }

    #[test]
    fn nothing_found_is_missing() {
        let home = tempfile::tempdir().expect("home");
        let env = env_fn(&[("TERM_PROGRAM", "Apple_Terminal")]);
        let probe = probe_with(&env, home.path(), "macos", &no_command);
        assert_eq!(probe.coverage, NerdFontCoverage::Missing);
    }

    #[test]
    fn configured_nerd_font_is_covered() {
        let home = tempfile::tempdir().expect("home");
        let kitty = home.path().join(".config/kitty");
        fs::create_dir_all(&kitty).expect("kitty dir");
        fs::write(
            kitty.join("kitty.conf"),
            "font_family MesloLGS Nerd Font\nfont_size 14\n",
        )
        .expect("kitty.conf");

        let env = env_fn(&[("TERM", "xterm-kitty")]);
        let probe = probe_with(&env, home.path(), "macos", &no_command);
        assert_eq!(
            probe.coverage,
            NerdFontCoverage::Covered("terminal font 'MesloLGS Nerd Font'".to_owned())
        );
    }

    #[test]
    fn kitty_symbol_map_covers_nerd_fonts() {
        let home = tempfile::tempdir().expect("home");
        let kitty = home.path().join(".config/kitty");
        fs::create_dir_all(&kitty).expect("kitty dir");
        fs::write(
            kitty.join("kitty.conf"),
            "font_family SF Mono\nsymbol_map U+E000-U+F8FF Symbols Nerd Font\n",
        )
        .expect("kitty.conf");

        let env = env_fn(&[("KITTY_WINDOW_ID", "1")]);
        let probe = probe_with(&env, home.path(), "linux", &no_command);
        assert!(matches!(probe.coverage, NerdFontCoverage::Covered(_)));
    }

    #[test]
    fn iterm_profile_fonts_are_read_from_defaults() {
        let home = tempfile::tempdir().expect("home");
        let env = env_fn(&[("TERM_PROGRAM", "iTerm.app")]);
        let plist = r#"{
            "New Bookmarks" = (
                { "Normal Font" = "Monaco 12"; "Use Non-ASCII Font" = 0; },
                { "Normal Font" = "Hack Nerd Font Mono 14"; }
            );
        }"#;
        let run = move |_: &str, _: &[&str]| Some(plist.to_owned());
        let probe = probe_with(&env, home.path(), "macos", &run);
        assert_eq!(
            probe.coverage,
            NerdFontCoverage::Covered("terminal font 'Hack Nerd Font Mono 14'".to_owned())
        );
    }

    #[test]
    fn vscode_terminal_font_is_read() {
        let home = tempfile::tempdir().expect("home");
        let settings = home.path().join("Library/Application Support/Code/User");
        fs::create_dir_all(&settings).expect("settings dir");
        fs::write(
            settings.join("settings.json"),
            r#"{ "terminal.integrated.fontFamily": "JetBrainsMono Nerd Font" }"#,
        )
        .expect("settings.json");

        let env = env_fn(&[("TERM_PROGRAM", "vscode")]);
        let probe = probe_with(&env, home.path(), "macos", &no_command);
        assert!(matches!(probe.coverage, NerdFontCoverage::Covered(_)));
    }

    #[test]
    fn install_writes_verified_fonts_once() {
        let dir = tempfile::tempdir().expect("fonts dir");
        let payload = vec![7u8; 100 * 1024];
        let sha256 = format!("{:x}", Sha256::digest(&payload));
        let assets = [FontAsset {
            name: "TestNerdFont-Regular.ttf",
            sha256: &sha256,
        }];
        let installed =
            install_fonts(dir.path(), &assets, |_| Ok(payload.clone())).expect("install");
        assert_eq!(installed, [dir.path().join("TestNerdFont-Regular.ttf")]);
        assert_eq!(
            fs::read(dir.path().join("TestNerdFont-Regular.ttf")).expect("font"),
            payload
        );
        // Second run keeps the identical file and is still reported.
        let again = install_fonts(dir.path(), &assets, |_| Ok(payload.clone())).expect("reinstall");
        assert_eq!(again, installed);
    }

    /// Real-network check of the pinned download URLs and SHA-256 digests:
    /// `cargo test -- --ignored platform::fonts`.
    #[test]
    #[ignore = "downloads from githubusercontent.com"]
    fn symbols_font_download_matches_the_pinned_sha256() {
        let dir = tempfile::tempdir().expect("fonts dir");
        let installed = install_symbols_only(dir.path(), fetch_symbols_font).expect("download");
        assert_eq!(installed.len(), SYMBOLS_FONTS.len());
        for path in installed {
            assert_eq!(path.extension().and_then(OsStr::to_str), Some("ttf"));
        }
    }

    #[test]
    fn install_rejects_a_checksum_mismatch() {
        let dir = tempfile::tempdir().expect("fonts dir");
        let assets = [FontAsset {
            name: "TestNerdFont-Regular.ttf",
            sha256: "0000000000000000000000000000000000000000000000000000000000000000",
        }];
        let error = install_fonts(dir.path(), &assets, |_| Ok(vec![0u8; 100 * 1024]))
            .expect_err("checksum mismatch must fail");
        assert!(error.contains("SHA-256"));
        assert!(!dir.path().join("TestNerdFont-Regular.ttf").exists());
    }
}
