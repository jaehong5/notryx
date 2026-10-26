use serde::Deserialize;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

const MAX_CONFIG_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub workspace: WorkspaceConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    #[serde(default = "default_admin_socket")]
    pub admin_socket: PathBuf,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            admin_socket: default_admin_socket(),
        }
    }
}

fn default_listen() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080)
}

fn default_admin_socket() -> PathBuf {
    PathBuf::from("/run/mdw/admin.sock")
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub notes_root: PathBuf,
    pub state_root: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    Read,
    Invalid,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Read => "config_unreadable",
            Self::Invalid => "invalid_config",
        })
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        if text.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::Invalid);
        }
        let config: Self = toml::from_str(text).map_err(|_| ConfigError::Invalid)?;
        if !config.workspace.notes_root.is_absolute()
            || !config.workspace.state_root.is_absolute()
            || config.workspace.notes_root == config.workspace.state_root
            || !config.server.admin_socket.is_absolute()
            || config.server.admin_socket.file_name().is_none()
        {
            return Err(ConfigError::Invalid);
        }
        Ok(config)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let mut text = String::new();
        std::fs::File::open(path)
            .map_err(|_| ConfigError::Read)?
            .take(MAX_CONFIG_BYTES as u64 + 1)
            .read_to_string(&mut text)
            .map_err(|_| ConfigError::Read)?;
        Self::from_toml(&text)
    }
}
