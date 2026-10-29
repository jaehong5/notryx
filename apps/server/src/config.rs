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
    #[serde(default)]
    pub sync: JournalRetentionConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JournalRetentionConfig {
    pub journal_retention_days: u64,
    pub journal_budget_bytes: u64,
}

impl Default for JournalRetentionConfig {
    fn default() -> Self {
        Self {
            journal_retention_days: 30,
            journal_budget_bytes: 512 * 1024 * 1024,
        }
    }
}

impl JournalRetentionConfig {
    pub fn retention_seconds(&self) -> Result<u64, ConfigError> {
        let seconds = self
            .journal_retention_days
            .checked_mul(24 * 60 * 60)
            .ok_or(ConfigError::Invalid)?;
        i64::try_from(seconds).map_err(|_| ConfigError::Invalid)?;
        Ok(seconds)
    }
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
    #[serde(default)]
    pub uploads: UploadLimits,
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
        config.workspace.uploads.validate()?;
        config.sync.retention_seconds()?;
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UploadLimits {
    pub max_device_bytes: u64,
    pub max_device_uploads: u16,
    pub max_workspace_bytes: u64,
    pub max_workspace_uploads: u16,
    pub max_receiving: u16,
}

impl Default for UploadLimits {
    fn default() -> Self {
        Self {
            max_device_bytes: 128 * 1024 * 1024,
            max_device_uploads: 64,
            max_workspace_bytes: 512 * 1024 * 1024,
            max_workspace_uploads: 256,
            max_receiving: 2,
        }
    }
}

impl UploadLimits {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_device_bytes == 0
            || self.max_device_bytes > self.max_workspace_bytes
            || self.max_workspace_bytes > 64 * 1024 * 1024 * 1024
            || self.max_device_uploads == 0
            || self.max_device_uploads > self.max_workspace_uploads
            || self.max_workspace_uploads > 4096
            || self.max_receiving == 0
            || self.max_receiving > 16
            || self.max_receiving > self.max_workspace_uploads
        {
            return Err(ConfigError::Invalid);
        }
        Ok(())
    }
}
