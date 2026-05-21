//! Configuration loader for Synergy.
//!
//! Resolution order (highest → lowest priority):
//! 1. Explicit path passed to [`SynergyConfig::load_from`].
//! 2. `%APPDATA%/Synergy/config.toml` (`~/.config/synergy/config.toml` on
//!    Unix).
//! 3. Built-in defaults.
//!
//! The loader is permissive on first run: a missing file simply returns
//! defaults, which lets the app boot even before a user has provided a
//! config.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use synergy_proxy::ProxyConfig;

/// Top-level config persisted at `%APPDATA%/Synergy/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SynergyConfig {
    pub general: GeneralConfig,
    pub leader: LeaderConfig,
    pub workers: WorkersConfig,
    pub proxy: ProxySection,
    pub task: TaskPolicy,
}

impl Default for SynergyConfig {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            leader: LeaderConfig::default(),
            workers: WorkersConfig::default(),
            proxy: ProxySection::default(),
            task: TaskPolicy::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub language: String,
    pub theme: String,
    pub project_dir: Option<String>,
    pub recent_projects: Vec<String>,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            language: "id".to_owned(),
            theme: "dark".to_owned(),
            project_dir: None,
            recent_projects: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LeaderConfig {
    /// Adapter id. `"opencode"`, `"cli-generic"`, or anything registered.
    pub adapter: String,
    pub bin_path: Option<String>,
    pub api: Option<LeaderApi>,
}

impl Default for LeaderConfig {
    fn default() -> Self {
        Self {
            adapter: "opencode".to_owned(),
            bin_path: None,
            api: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaderApi {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkersConfig {
    pub count: u32,
    pub adapter: String,
    pub bin_path: String,
    /// AI model for workers (e.g., "anthropic/claude-sonnet-4-20250514").
    /// Sent as `/model <name>` to OpenCode workers after spawn.
    pub model: String,
}

impl Default for WorkersConfig {
    fn default() -> Self {
        Self {
            count: 6,
            adapter: "opencode".to_owned(),
            bin_path: "opencode".to_owned(),
            model: String::new(), // Empty = will query from OpenCode on first use
        }
    }
}

/// Proxy mode mirrors the spec (§7.2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    None,
    List,
    Rotating,
}

impl Default for ProxyMode {
    fn default() -> Self {
        ProxyMode::None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxySection {
    pub mode: ProxyMode,
    pub address: Option<String>,
    #[serde(rename = "list")]
    pub entries: Vec<ProxyEntry>,
}

impl Default for ProxySection {
    fn default() -> Self {
        Self {
            mode: ProxyMode::default(),
            address: None,
            entries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyEntry {
    pub address: String,
    pub label: Option<String>,
}

impl ProxySection {
    /// Materialize the configured entries into [`ProxyConfig`] structs that
    /// [`synergy_proxy::ProxyManager`] understands.
    pub fn to_proxy_configs(&self, worker_count: u32) -> Vec<ProxyConfig> {
        match self.mode {
            ProxyMode::None => Vec::new(),
            ProxyMode::List => self
                .entries
                .iter()
                .map(|e| ProxyConfig {
                    address: e.address.clone(),
                    label: e.label.clone(),
                })
                .collect(),
            ProxyMode::Rotating => {
                let Some(addr) = self.address.as_ref() else {
                    return Vec::new();
                };
                (0..worker_count)
                    .map(|i| ProxyConfig {
                        address: addr.clone(),
                        label: Some(format!("rotating-{i}")),
                    })
                    .collect()
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskPolicy {
    pub max_retries: u32,
    pub timeout_minutes: u32,
    pub escalate_to_leader: bool,
}

impl Default for TaskPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            timeout_minutes: 10,
            escalate_to_leader: true,
        }
    }
}

impl SynergyConfig {
    /// Resolve the canonical config path (creates parent dir if needed but
    /// does *not* create the file itself).
    pub fn default_path() -> Result<PathBuf> {
        let base = dirs::config_dir()
            .context("could not resolve user config dir")?
            .join("Synergy");
        if !base.exists() {
            std::fs::create_dir_all(&base).with_context(|| {
                format!("creating config dir {}", base.display())
            })?;
        }
        Ok(base.join("config.toml"))
    }

    /// Load from the canonical user config path. Missing file → defaults.
    pub fn load_default() -> Result<Self> {
        let path = Self::default_path()?;
        Self::load_from(&path)
    }

    /// Load from an arbitrary path. Missing file → defaults.
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: SynergyConfig = toml::from_str(&raw)
            .with_context(|| format!("parsing TOML at {}", path.display()))?;
        Ok(cfg)
    }

    /// Persist the configuration as TOML to the given path.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let body = toml::to_string_pretty(self).context("serializing config")?;
        std::fs::write(path, body)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_file_returns_defaults() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.toml");
        let cfg = SynergyConfig::load_from(&path).unwrap();
        assert_eq!(cfg.workers.count, 6);
        assert_eq!(cfg.leader.adapter, "opencode");
    }

    #[test]
    fn round_trip_full_config() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("c.toml");

        let mut cfg = SynergyConfig::default();
        cfg.proxy.mode = ProxyMode::List;
        cfg.proxy.entries.push(ProxyEntry {
            address: "socks5://1.2.3.4:1080".to_owned(),
            label: Some("A".to_owned()),
        });
        cfg.workers.count = 3;
        cfg.save_to(&path).unwrap();

        let loaded = SynergyConfig::load_from(&path).unwrap();
        assert_eq!(loaded.workers.count, 3);
        assert_eq!(loaded.proxy.mode, ProxyMode::List);
        assert_eq!(loaded.proxy.entries.len(), 1);
    }

    #[test]
    fn rotating_proxy_expands_per_worker() {
        let mut section = ProxySection::default();
        section.mode = ProxyMode::Rotating;
        section.address = Some("socks5://rot.example:1080".to_owned());
        let configs = section.to_proxy_configs(3);
        assert_eq!(configs.len(), 3);
        assert!(configs[0].label.as_ref().unwrap().contains("rotating-0"));
    }
}
