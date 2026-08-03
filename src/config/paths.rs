use std::{env, path::PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigPaths {
    pub config_file: PathBuf,
    pub credentials_file: PathBuf,
    pub specs_directory: PathBuf,
    pub state_directory: PathBuf,
    pub cache_directory: PathBuf,
}

impl ConfigPaths {
    pub fn discover() -> crate::Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| crate::Error::Config("$HOME is not set".into()))?;
        let config_root = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("hokan");
        let state_directory = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/state"))
            .join("hokan");
        let cache_directory = env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"))
            .join("hokan");
        Ok(Self {
            config_file: config_root.join("config.toml"),
            credentials_file: config_root.join("credentials.toml"),
            specs_directory: config_root.join("specs"),
            state_directory,
            cache_directory,
        })
    }
}
