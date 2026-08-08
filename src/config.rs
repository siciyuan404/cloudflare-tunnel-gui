use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Default Cloudflare Tunnel config directory.
pub fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cloudflared")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.yml")
}

/// A single ingress rule.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Ingress {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub service: String,
}

/// Parsed representation of config.yml.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct TunnelConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "credentials-file")]
    pub credentials_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "protocol")]
    pub protocol: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ingress: Vec<Ingress>,
}

impl TunnelConfig {
    pub fn load() -> Result<TunnelConfig> {
        let path = config_path();
        if !path.exists() {
            return Ok(TunnelConfig::default());
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| anyhow!("cannot read {}: {e}", path.display()))?;
        if raw.trim().is_empty() {
            return Ok(TunnelConfig::default());
        }
        serde_yaml::from_str(&raw).map_err(|e| anyhow!("invalid config.yml: {e}"))
    }

    pub fn save(&self) -> Result<()> {
        let dir = config_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow!("cannot create {}: {e}", dir.display()))?;
        let yaml = serde_yaml::to_string(self)?;
        let path = config_path();
        std::fs::write(&path, yaml).map_err(|e| anyhow!("cannot write {}: {e}", path.display()))?;
        Ok(())
    }

    pub fn find_credentials_files(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(cf) = &self.credentials_file {
            let p = PathBuf::from(cf);
            if p.exists() {
                out.push(p);
            }
        }
        // Fallback: any *.json in the config dir (cloudflared naming)
        if let Ok(rd) = std::fs::read_dir(config_dir()) {
            for e in rd.flatten() {
                if let Some(name) = e.file_name().to_str() {
                    if name.ends_with(".json") && !name.contains("cert.pem") {
                        out.push(e.path());
                    }
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }
}

/// Simple cache of what cloudflared reports for `tunnel list` so the GUI can
/// populate the tunnel picker without re-parsing repeatedly.
#[derive(Debug, Clone, Default)]
pub struct TunnelEntry {
    pub id: String,
    pub name: String,
    pub created: String,
}

pub fn parse_tunnel_list(output: &str) -> Vec<TunnelEntry> {
    let mut entries = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("ID") || line.starts_with("-") {
            continue;
        }
        let mut parts = line.split_whitespace();
        let id = parts.next().unwrap_or("").to_string();
        let name = parts.next().unwrap_or("").to_string();
        let created = parts.collect::<Vec<_>>().join(" ");
        if !id.is_empty() {
            entries.push(TunnelEntry { id, name, created });
        }
    }
    entries
}