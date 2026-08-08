use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::process::Command;

/// Result of running a single cloudflared command.
#[derive(Debug, Clone, Default)]
pub struct CmdResult {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
    pub error: Option<String>,
}

impl CmdResult {
    pub fn output_text(&self) -> String {
        let mut s = String::new();
        if let Some(e) = &self.error {
            s.push_str(&format!("[error] {e}\n"));
        }
        if !self.stdout.trim().is_empty() {
            s.push_str(&self.stdout);
        }
        if !self.stderr.trim().is_empty() {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(&format!("[stderr]\n{}", self.stderr));
        }
        s
    }
}

fn run(cmd: &mut Command) -> CmdResult {
    match cmd.output() {
        Ok(out) => CmdResult {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            error: None,
        },
        Err(e) => CmdResult {
            ok: false,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(e.to_string()),
        },
    }
}

/// Resolve symlinks / reparse points (e.g. WinGet Links shims) to the real
/// executable. Windows services cannot reliably start from a WinGet link shim
/// (DLL init failure 0xc0000142), so installing the service must use the real
/// binary path.
fn resolve_real(p: PathBuf) -> PathBuf {
    std::fs::canonicalize(&p).unwrap_or(p)
}

/// Locate the `cloudflared` executable.
pub fn find_binary() -> Result<PathBuf> {
    // 1. Env override
    if let Ok(p) = std::env::var("CLOUDFLARED_PATH") {
        let pb = resolve_real(PathBuf::from(&p));
        if pb.exists() {
            return Ok(pb);
        }
    }
    // 2. Shell resolution (PATH)
    if let Some(pb) = which() {
        return Ok(resolve_real(pb));
    }
    // 3. Common locations on Windows
    for cand in [
        dirs::home_dir().map(|d| d.join(".cloudflared\\cloudflared.exe")),
        Some(PathBuf::from("C:\\Program Files\\cloudflared\\cloudflared.exe")),
        Some(PathBuf::from("C:\\cloudflared\\cloudflared.exe")),
    ]
    .into_iter()
    .flatten()
    {
        if cand.exists() {
            return Ok(resolve_real(cand));
        }
    }
    Err(anyhow!(
        "cloudflared not found. Install it or set CLOUDFLARED_PATH."
    ))
}

/// On Windows, resolve an executable name via `where`.
fn which() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let out = Command::new("where.exe").arg("cloudflared").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().next().map(PathBuf::from)
}

pub fn version(binary: &PathBuf) -> CmdResult {
    run(Command::new(binary).arg("--version"))
}

pub fn tunnel_list(binary: &PathBuf) -> CmdResult {
    let mut c = Command::new(binary);
    c.args(["tunnel", "list"]);
    run(&mut c)
}

pub fn tunnel_info(binary: &PathBuf, name: &str) -> CmdResult {
    let mut c = Command::new(binary);
    c.args(["tunnel", "info", name]);
    run(&mut c)
}

pub fn login(binary: &PathBuf) -> CmdResult {
    // Interactive: opens browser. Run as a normal child; capture output.
    run(Command::new(binary).arg("tunnel").arg("login"))
}

pub fn tunnel_create(binary: &PathBuf, name: &str) -> CmdResult {
    let mut c = Command::new(binary);
    c.args(["tunnel", "create", name]);
    run(&mut c)
}

pub fn tunnel_delete(binary: &PathBuf, name: &str) -> CmdResult {
    let mut c = Command::new(binary);
    c.args(["tunnel", "delete", "-f", name]);
    run(&mut c)
}

pub fn tunnel_cleanup(binary: &PathBuf, name: &str) -> CmdResult {
    let mut c = Command::new(binary);
    c.args(["tunnel", "cleanup", name]);
    run(&mut c)
}

pub fn service_install(binary: &PathBuf) -> CmdResult {
    let mut c = Command::new(binary);
    c.arg("service").arg("install");
    run(&mut c)
}

pub fn service_uninstall(binary: &PathBuf) -> CmdResult {
    let mut c = Command::new(binary);
    c.arg("service").arg("uninstall");
    run(&mut c)
}

/// Logical names the service can be controlled with.
pub const SERVICE_NAME: &str = "cloudflared";

/// Control the Windows service via `sc.exe`.
pub fn sc(action: &str) -> CmdResult {
    let mut c = Command::new("sc.exe");
    c.args([action, SERVICE_NAME]);
    run(&mut c)
}

pub fn service_query_status() -> String {
    let mut c = Command::new("sc.exe");
    c.args(["query", SERVICE_NAME]);
    let r = run(&mut c);
    let text = r.output_text();
    // Parse "STATE              : 4  RUNNING"
    for line in text.lines() {
        if let Some(stripped) = line.trim().strip_prefix("STATE") {
            return stripped.trim().to_string();
        }
    }
    if !r.ok {
        return if text.contains("1060") || text.to_lowercase().contains("does not exist") {
            "NOT INSTALLED".to_string()
        } else {
            text.trim().to_string()
        };
    }
    text.trim().to_string()
}