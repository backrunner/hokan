use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use nix::fcntl::OFlag;
use serde::{Deserialize, Serialize};

use crate::shell::ShellKind;

const CONFIG_MAX_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub core: CoreConfig,
    pub ui: UiConfig,
    pub keys: KeysConfig,
    pub history: HistoryConfig,
    pub completion: CompletionConfig,
    pub logging: LoggingConfig,
    pub ai: AiConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            core: CoreConfig::default(),
            ui: UiConfig::default(),
            keys: KeysConfig::default(),
            history: HistoryConfig::default(),
            completion: CompletionConfig::default(),
            logging: LoggingConfig::default(),
            ai: AiConfig::default(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> crate::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let mut options = fs::OpenOptions::new();
        options.read(true).custom_flags(OFlag::O_NONBLOCK.bits());
        let mut file = options.open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(crate::Error::Config(format!(
                "{} is not a regular configuration file",
                path.display()
            )));
        }
        if metadata.len() > CONFIG_MAX_BYTES {
            return Err(crate::Error::Config(format!(
                "{} exceeds the 1 MiB configuration limit",
                path.display()
            )));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(CONFIG_MAX_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > CONFIG_MAX_BYTES {
            return Err(crate::Error::Config(format!(
                "{} exceeds the 1 MiB configuration limit",
                path.display()
            )));
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| crate::Error::Config(format!("{} is not valid UTF-8", path.display())))?;
        let config: Self = toml::from_str(&text).map_err(|error| {
            let location = error
                .span()
                .and_then(|span| text.get(..span.start))
                .map(|prefix| {
                    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
                    let column = prefix
                        .rsplit('\n')
                        .next()
                        .map_or(1, |tail| tail.chars().count() + 1);
                    format!(" at line {line}, column {column}")
                })
                .unwrap_or_default();
            crate::Error::Config(format!(
                "{}: invalid TOML configuration{location}",
                path.display()
            ))
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> crate::Result<()> {
        if self.version != 1 {
            return Err(crate::Error::Config(format!(
                "unsupported config version {}",
                self.version
            )));
        }
        if !(3..=50).contains(&self.ui.max_rows) {
            return Err(crate::Error::Config(
                "ui.max_rows must be 3..=50 (bordered overlay minimum)".into(),
            ));
        }
        if !(40..=240).contains(&self.ui.max_width) {
            return Err(crate::Error::Config("ui.max_width must be 40..=240".into()));
        }
        if !matches!(self.ui.color.as_str(), "auto" | "always" | "never") {
            return Err(crate::Error::Config(
                "ui.color must be auto, always, or never".into(),
            ));
        }
        self.keys.validate()?;
        if !(100..=100_000).contains(&self.history.max_command_bytes) {
            return Err(crate::Error::Config(
                "history.max_command_bytes must be 100..=100000".into(),
            ));
        }
        if !(10..=5_000).contains(&self.completion.local_timeout_ms)
            || !(10..=10_000).contains(&self.completion.max_candidates)
        {
            return Err(crate::Error::Config(
                "completion limits are outside their supported range".into(),
            ));
        }
        if !(64 * 1024..=64 * 1024 * 1024).contains(&self.logging.max_bytes)
            || !(1..=10).contains(&self.logging.rotations)
        {
            return Err(crate::Error::Config(
                "logging.max_bytes must be 65536..=67108864 and logging.rotations must be 1..=10"
                    .into(),
            ));
        }
        if self.ai.enabled {
            let has_credential_source = self
                .ai
                .api_key_file
                .as_ref()
                .is_some_and(|path| !path.as_os_str().is_empty())
                || !self.ai.api_key_env.trim().is_empty();
            if self.ai.endpoint.trim().is_empty()
                || self.ai.model.trim().is_empty()
                || !has_credential_source
            {
                return Err(crate::Error::Config(
                    "enabled AI requires endpoint, model, and a credential source".into(),
                ));
            }
            if !(1_000..=120_000).contains(&self.ai.timeout_ms) {
                return Err(crate::Error::Config(
                    "ai.timeout_ms must be 1000..=120000".into(),
                ));
            }
            let endpoint = reqwest::Url::parse(self.ai.endpoint.trim()).map_err(|_| {
                crate::Error::Config("ai.endpoint must be an absolute HTTP(S) URL".into())
            })?;
            if !matches!(endpoint.scheme(), "http" | "https")
                || endpoint.host_str().is_none()
                || !endpoint.username().is_empty()
                || endpoint.password().is_some()
                || endpoint.query().is_some()
                || endpoint.fragment().is_some()
            {
                return Err(crate::Error::Config(
                    "ai.endpoint must be an HTTP(S) URL without credentials, query, or fragment"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    pub fn write_default(path: &Path) -> crate::Result<()> {
        let parent = path.parent().ok_or_else(|| {
            crate::Error::Config(format!("{} has no parent directory", path.display()))
        })?;
        fs::create_dir_all(parent)?;
        let text = toml::to_string_pretty(&Self::default())
            .map_err(|error| crate::Error::Config(error.to_string()))?;
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(crate::Error::Config(format!(
                    "refusing to overwrite {}",
                    path.display()
                )));
            }
            Err(error) => return Err(error.into()),
        };
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    }

    pub fn write_atomic(&self, path: &Path) -> crate::Result<()> {
        self.validate()?;
        let parent = path.parent().ok_or_else(|| {
            crate::Error::Config(format!("{} has no parent directory", path.display()))
        })?;
        fs::create_dir_all(parent)?;
        let text = toml::to_string_pretty(self)
            .map_err(|error| crate::Error::Config(error.to_string()))?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".config.toml.")
            .tempfile_in(parent)?;
        temporary.write_all(text.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CoreConfig {
    pub shell: Option<ShellKind>,
    pub login_shell: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// Total overlay box height including the two border rows (3..=50).
    pub max_rows: usize,
    pub max_width: usize,
    pub color: String,
    /// Render the Nerd Font icon column (requires a Nerd Font in the terminal).
    pub nerd_fonts: bool,
    /// Removed in favor of `nerd_fonts`; accepted so old configs still load.
    #[serde(skip_serializing)]
    pub ascii_icons: Option<bool>,
    pub show_hidden: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeyBinding {
    Disabled,
    Up,
    Down,
    PageUp,
    PageDown,
    Tab,
    BackTab,
    Enter,
    Escape,
    CtrlC,
    CtrlD,
    CtrlL,
    CtrlR,
}

impl KeyBinding {
    #[must_use]
    pub const fn matches(self, input: &crate::terminal::InputKind) -> bool {
        matches!(
            (self, input),
            (Self::Up, crate::terminal::InputKind::Up)
                | (Self::Down, crate::terminal::InputKind::Down)
                | (Self::PageUp, crate::terminal::InputKind::PageUp)
                | (Self::PageDown, crate::terminal::InputKind::PageDown)
                | (Self::Tab, crate::terminal::InputKind::Tab)
                | (Self::BackTab, crate::terminal::InputKind::BackTab)
                | (Self::Enter, crate::terminal::InputKind::Enter)
                | (Self::Escape, crate::terminal::InputKind::Escape)
                | (Self::CtrlC, crate::terminal::InputKind::CtrlC)
                | (Self::CtrlD, crate::terminal::InputKind::CtrlD)
                | (Self::CtrlL, crate::terminal::InputKind::CtrlL)
                | (Self::CtrlR, crate::terminal::InputKind::CtrlR)
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeysConfig {
    pub accept: KeyBinding,
    /// Enter is two-state: with no selection it executes the typed buffer
    /// as-is (pass-through); with an explicit selection it activates the
    /// selected candidate — executing runnable ones (after a confirmation
    /// step when dangerous) and degrading to the Tab fill-back otherwise.
    pub activate: KeyBinding,
    pub up: KeyBinding,
    pub down: KeyBinding,
    pub page_up: KeyBinding,
    pub page_down: KeyBinding,
    pub dismiss: KeyBinding,
    pub history: KeyBinding,
    pub toggle: KeyBinding,
}

impl Default for KeysConfig {
    fn default() -> Self {
        Self {
            accept: KeyBinding::Tab,
            activate: KeyBinding::Enter,
            up: KeyBinding::Up,
            down: KeyBinding::Down,
            page_up: KeyBinding::PageUp,
            page_down: KeyBinding::PageDown,
            dismiss: KeyBinding::Escape,
            history: KeyBinding::CtrlR,
            toggle: KeyBinding::BackTab,
        }
    }
}

impl KeysConfig {
    fn validate(&self) -> crate::Result<()> {
        let bindings = [
            ("accept", self.accept),
            ("activate", self.activate),
            ("up", self.up),
            ("down", self.down),
            ("page_up", self.page_up),
            ("page_down", self.page_down),
            ("dismiss", self.dismiss),
            ("history", self.history),
            ("toggle", self.toggle),
        ];
        for (index, (name, binding)) in bindings.iter().enumerate() {
            if *binding == KeyBinding::Disabled {
                continue;
            }
            if let Some((other, _)) = bindings[..index]
                .iter()
                .find(|(_, previous)| previous == binding)
            {
                return Err(crate::Error::Config(format!(
                    "keys.{name} conflicts with keys.{other}"
                )));
            }
        }
        Ok(())
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            max_rows: 8,
            max_width: 76,
            color: "auto".into(),
            nerd_fonts: true,
            ascii_icons: None,
            show_hidden: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HistoryConfig {
    pub enabled: bool,
    pub max_command_bytes: usize,
    pub exclude: Vec<String>,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_command_bytes: 16_384,
            exclude: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompletionConfig {
    pub local_timeout_ms: u64,
    pub max_candidates: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    pub enabled: bool,
    pub max_bytes: u64,
    pub rotations: usize,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_bytes: 1024 * 1024,
            rotations: 3,
        }
    }
}

impl Default for CompletionConfig {
    fn default() -> Self {
        Self {
            local_timeout_ms: 100,
            max_candidates: 1_000,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AiConfig {
    pub enabled: bool,
    pub endpoint: String,
    pub model: String,
    pub api_key_env: String,
    pub api_key_file: Option<PathBuf>,
    pub timeout_ms: u64,
    pub trigger_prefix: String,
    pub send_cwd_basename: bool,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: "https://api.openai.com/v1".into(),
            model: String::new(),
            api_key_env: "OPENAI_API_KEY".into(),
            api_key_file: None,
            timeout_ms: 8_000,
            trigger_prefix: "??".into(),
            send_cwd_basename: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_ascii_icons_field_is_accepted_and_ignored() {
        let config: Config = toml::from_str("version = 1\n[ui]\nascii_icons = true\n")
            .expect("legacy ascii_icons must still parse");
        assert_eq!(config.ui.ascii_icons, Some(true));
        assert!(config.ui.nerd_fonts, "nerd_fonts defaults to true");
        assert_eq!(config.ui.max_rows, 8);
        assert_eq!(config.ui.max_width, 76);
        let rendered = toml::to_string(&config).expect("serialize config");
        assert!(!rendered.contains("ascii_icons"));
    }

    #[test]
    fn max_rows_range_requires_room_for_the_bordered_box() {
        let mut config = Config::default();
        config.ui.max_rows = 2;
        assert!(config.validate().is_err());
        config.ui.max_rows = 3;
        assert!(config.validate().is_ok());
        config.ui.max_rows = 51;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_unknown_fields_and_incomplete_ai() {
        assert!(toml::from_str::<Config>("version = 1\nunknown = true").is_err());
        let mut config = Config::default();
        config.ai.enabled = true;
        assert!(config.validate().is_err());

        config.ai.model = "model".into();
        config.ai.endpoint = "https://user:secret@example.com/v1".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validates_key_conflicts_and_allows_disabled_bindings() {
        let mut config = Config::default();
        config.keys.activate = KeyBinding::Tab;
        assert!(config.validate().is_err());
        config.keys.activate = KeyBinding::Disabled;
        config.keys.history = KeyBinding::Disabled;
        assert!(config.validate().is_ok());

        let rendered = toml::to_string(&config).expect("serialize config");
        assert!(rendered.contains("activate = \"disabled\""));
        assert!(toml::from_str::<Config>(&rendered).is_ok());
    }

    #[test]
    fn validates_debug_log_bounds() {
        let mut config = Config::default();
        config.logging.enabled = true;
        config.logging.max_bytes = 1_024;
        assert!(config.validate().is_err());
        config.logging.max_bytes = 64 * 1024;
        config.logging.rotations = 0;
        assert!(config.validate().is_err());
        config.logging.rotations = 2;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn parse_errors_never_echo_source_values() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("config.toml");
        let secret = "sk-config-secret-value";
        fs::write(&path, format!("version = 1\napi_key = \"{secret}\"\n")).expect("invalid config");

        let error = Config::load(&path).expect_err("unknown field must fail");
        let detail = error.to_string();
        assert!(detail.contains("invalid TOML configuration"));
        assert!(detail.contains("line 2"));
        assert!(!detail.contains(secret));
    }

    #[test]
    fn rejects_non_regular_configuration_files_without_blocking() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("config.fifo");
        nix::unistd::mkfifo(
            &path,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .expect("config FIFO");
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn oversized_config_is_rejected_before_parsing() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("config.toml");
        fs::write(&path, vec![b'x'; CONFIG_MAX_BYTES as usize + 1]).expect("oversized config");
        let error = Config::load(&path).expect_err("oversized config should fail");
        assert!(error.to_string().contains("1 MiB configuration limit"));
    }

    #[test]
    fn default_config_never_follows_an_existing_symlink() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("config.toml");
        let target = directory.path().join("must-not-be-created");
        std::os::unix::fs::symlink(&target, &path).expect("dangling config symlink");

        let error = Config::write_default(&path).expect_err("existing path must be rejected");

        assert!(error.to_string().contains("refusing to overwrite"));
        assert!(!target.exists());
        assert!(
            fs::symlink_metadata(path)
                .expect("config symlink")
                .file_type()
                .is_symlink()
        );
    }
}
