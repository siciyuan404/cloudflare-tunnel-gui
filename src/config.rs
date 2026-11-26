use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Machine-global Cloudflare Tunnel config directory.
///
/// On Windows this is `%ProgramData%\cloudflared` so the config, credentials
/// and the SYSTEM service all use the same machine-wide location. Requires
/// administrator rights to write.
pub fn config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(p) = std::env::var("ProgramData") {
            return PathBuf::from(p).join("cloudflared");
        }
        return PathBuf::from("C:\\ProgramData\\cloudflared");
    }
    #[cfg(not(windows))]
    {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".cloudflared")
    }
}

/// Legacy per-user config directory (`%USERPROFILE%\.cloudflared`). Read-only
/// fallback so existing setups keep working after switching to global mode.
pub fn legacy_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cloudflared")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.yml")
}

pub fn legacy_config_path() -> PathBuf {
    legacy_dir().join("config.yml")
}

/// True when the machine-global config directory can be written (i.e. the app
/// is running elevated or the directory already grants write access).
pub fn global_dir_writable() -> bool {
    let dir = config_dir();
    if dir.exists() {
        let probe = dir.join(".write_probe");
        match std::fs::write(&probe, b"ok") {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                true
            }
            Err(_) => false,
        }
    } else {
        // Try to create it (only works when elevated).
        std::fs::create_dir_all(&dir).is_ok()
    }
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
        // Prefer the machine-global config; fall back to the legacy per-user one.
        for path in [config_path(), legacy_config_path()] {
            if !path.exists() {
                continue;
            }
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| anyhow!("cannot read {}: {e}", path.display()))?;
            if raw.trim().is_empty() {
                continue;
            }
            return serde_yaml::from_str(&raw).map_err(|e| anyhow!("invalid config.yml: {e}"));
        }
        Ok(TunnelConfig::default())
    }

    /// True when the loaded config actually lives in the legacy user dir, so the
    /// GUI can offer to migrate it to the global location.
    pub fn is_legacy() -> bool {
        !config_path().exists() && legacy_config_path().exists()
    }

    /// Plain save of the known fields only. Prefer `save_merged` from the GUI:
    /// this variant drops unknown top-level keys.
    #[allow(dead_code)]
    pub fn save(&self) -> Result<()> {
        let dir = config_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow!("cannot create {}: {e}", dir.display()))?;
        let yaml = serde_yaml::to_string(self)?;
        let path = config_path();
        std::fs::write(&path, yaml).map_err(|e| anyhow!("cannot write {}: {e}", path.display()))?;
        Ok(())
    }

    /// Save the GUI-edited fields **on top of** the existing file, preserving
    /// any unknown top-level keys (e.g. `originRequest`, `warp-routing`) that
    /// the GUI does not edit. This is what lets the GUI "take over" the whole
    /// config without silently dropping hand-written sections.
    pub fn save_merged(&self) -> Result<()> {
        self.save_merged_to(&config_path())
    }

    fn save_merged_to(&self, path: &std::path::Path) -> Result<()> {
        let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(dir)
            .map_err(|e| anyhow!("无法创建目录 {}: {e}（写全局配置需要管理员权限）", dir.display()))?;

        let mut root: serde_yaml::Value = if path.exists() {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| anyhow!("cannot read {}: {e}", path.display()))?;
            if raw.trim().is_empty() {
                serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
            } else {
                serde_yaml::from_str(&raw)
                    .map_err(|e| anyhow!("原 config.yml 解析失败（未覆盖文件）: {e}"))?
            }
        } else {
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
        };

        let map = match &mut root {
            serde_yaml::Value::Mapping(m) => m,
            _ => return Err(anyhow!("config.yml 根节点必须是映射")),
        };
        let key = |s: &str| serde_yaml::Value::String(s.to_string());

        match &self.tunnel {
            Some(t) => {
                map.insert(key("tunnel"), serde_yaml::Value::String(t.clone()));
            }
            None => {
                map.remove(&key("tunnel"));
            }
        }
        match &self.credentials_file {
            Some(c) => {
                map.insert(key("credentials-file"), serde_yaml::Value::String(c.clone()));
            }
            None => {
                map.remove(&key("credentials-file"));
            }
        }
        match &self.protocol {
            Some(p) => {
                map.insert(key("protocol"), serde_yaml::Value::String(p.clone()));
            }
            None => {
                map.remove(&key("protocol"));
            }
        }
        if self.ingress.is_empty() {
            map.remove(&key("ingress"));
        } else {
            // cloudflared requires the catch-all (fallback) rule to be the
            // last entry: it matches every hostname, so any rule after it
            // would never fire and newer versions refuse to start with that
            // layout. Pin the fallback (hostname-less) rule to the end,
            // preserving the relative order of all other rules.
            let mut sorted = self.ingress.clone();
            sorted.sort_by_key(|r| !r.hostname.is_some());
            map.insert(key("ingress"), serde_yaml::to_value(&sorted)?);
        }

        let out = serde_yaml::to_string(&root).map_err(|e| anyhow!("序列化失败: {e}"))?;
        std::fs::write(&path, out).map_err(|e| anyhow!("cannot write {}: {e}", path.display()))?;
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
        // Fallback: any *.json in the global and legacy config dirs.
        for dir in [config_dir(), legacy_dir()] {
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    if let Some(name) = e.file_name().to_str() {
                        if name.ends_with(".json") && !name.contains("cert.pem") {
                            out.push(e.path());
                        }
                    }
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Ensure the credentials file referenced by the config exists in the
    /// machine-global dir so the SYSTEM service can read it. Copies from the
    /// legacy user dir when needed and returns the (possibly rewritten) path.
    pub fn normalize_credentials_to_global(&mut self) -> Result<()> {
        let Some(cf) = self.credentials_file.clone() else {
            return Ok(());
        };
        let src = PathBuf::from(&cf);
        if !src.exists() {
            return Ok(());
        }
        let global_dir = config_dir();
        let dst = global_dir.join(src.file_name().unwrap_or_default());
        // Already inside the global dir → nothing to do.
        if src.parent() == Some(global_dir.as_path()) {
            return Ok(());
        }
        std::fs::create_dir_all(&global_dir)
            .map_err(|e| anyhow!("无法创建 {}: {e}（需要管理员权限）", global_dir.display()))?;
        std::fs::copy(&src, &dst)
            .map_err(|e| anyhow!("复制凭据到全局目录失败: {e}"))?;
        self.credentials_file = Some(dst.display().to_string());
        Ok(())
    }
}

/// Simple cache of what cloudflared reports for `tunnel list` so the GUI can
/// populate the tunnel picker without re-parsing repeatedly.
#[derive(Debug, Clone, Default)]
pub struct TunnelEntry {
    pub id: String,
    pub name: String,
    pub created: String,
    /// Non-empty when the tunnel has live edge connections, e.g. "1xlax09, 1xsjc10".
    pub connections: String,
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
        let created = parts.next().unwrap_or("").to_string();
        // CONNECTIONS column arrives as "1xlax09, 1xsjc10"; strip trailing
        // commas from each token before re-joining.
        let connections = parts
            .map(|s| s.trim_end_matches(','))
            .collect::<Vec<_>>()
            .join(", ");
        if !id.is_empty() {
            entries.push(TunnelEntry {
                id,
                name,
                created,
                connections,
            });
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_config_yaml(content: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "cft_manager_test_{}_{n}",
            std::process::id()
        ));
        let path = dir.join("config.yml");
        std::fs::create_dir_all(&dir).unwrap();
        if content.is_empty() {
            let _ = std::fs::remove_file(&path);
        } else {
            std::fs::write(&path, content).unwrap();
        }
        (dir, path)
    }

    #[test]
    fn save_merged_preserves_unknown_top_level_keys() {
        let (dir, path) = temp_config_yaml(
            "tunnel: old-tunnel\nprotocol: quic\ncustom-key: keep-me\nexperimental: true\n",
        );
        let cfg = TunnelConfig {
            tunnel: Some("new-tunnel".into()),
            protocol: None, // 显式清空 protocol，验证删除逻辑
            ..Default::default()
        };
        cfg.save_merged_to(&path).unwrap();

        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains("new-tunnel"), "tunnel 应被更新: {out}");
        assert!(!out.contains("old-tunnel"), "旧 tunnel 应被替换: {out}");
        assert!(!out.contains("protocol"), "清空的 protocol 应被移除: {out}");
        assert!(out.contains("custom-key: keep-me"), "未知字段应保留: {out}");
        assert!(out.contains("experimental: true"), "未知字段应保留: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_merged_writes_ingress_array() {
        let (dir, path) = temp_config_yaml("");
        let cfg = TunnelConfig {
            tunnel: Some("t1".into()),
            ingress: vec![Ingress {
                hostname: Some("app.example.com".into()),
                path: Some("/api".into()),
                service: "http://localhost:8080".into(),
            }],
            ..Default::default()
        };
        cfg.save_merged_to(&path).unwrap();

        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains("app.example.com"), "hostname 应写入: {out}");
        assert!(out.contains("/api"), "path 应写入: {out}");
        assert!(out.contains("http://localhost:8080"), "service 应写入: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_tunnel_list_skips_header() {
        let out = parse_tunnel_list(
            "ID                                    NAME    CREATED              CONNECTIONS\n\
             abcdef1234567890abcdef1234567890      my-tun  2026-08-01T10:00:00Z  1xabc, 1xdef\n",
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "my-tun");
        assert_eq!(out[0].id, "abcdef1234567890abcdef1234567890");
        assert_eq!(out[0].created, "2026-08-01T10:00:00Z");
        assert_eq!(out[0].connections, "1xabc, 1xdef");
    }

    #[test]
    fn save_merged_pins_fallback_last() {
        let (dir, path) = temp_config_yaml("");
        let cfg = TunnelConfig {
            tunnel: Some("t1".into()),
            ingress: vec![
                Ingress {
                    hostname: None,
                    path: None,
                    service: "http_status:404".into(),
                },
                Ingress {
                    hostname: Some("app.example.com".into()),
                    path: None,
                    service: "http://localhost:8080".into(),
                },
                Ingress {
                    hostname: Some("dev.example.com".into()),
                    path: None,
                    service: "http://localhost:9090".into(),
                },
            ],
            ..Default::default()
        };
        cfg.save_merged_to(&path).unwrap();

        let out = std::fs::read_to_string(&path).unwrap();
        let pos_app = out.find("app.example.com").unwrap();
        let pos_dev = out.find("dev.example.com").unwrap();
        let pos_fallback = out.find("http_status:404").unwrap();
        assert!(
            pos_fallback > pos_app && pos_fallback > pos_dev,
            "fallback 必须排最后: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}