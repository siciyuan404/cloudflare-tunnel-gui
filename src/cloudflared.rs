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
    // Console children (sc.exe, taskkill.exe, cloudflared.exe, ...) would
    // otherwise flash a console window on every invocation.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    match cmd.output() {
        Ok(out) => CmdResult {
            ok: out.status.success(),
            stdout: decode_console(&out.stdout),
            stderr: decode_console(&out.stderr),
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
/// binary path. `canonicalize` returns a `\\?\C:\...` prefixed path on
/// Windows; strip the prefix because cloudflared's service mode crashes
/// (exit code 1067) when handed the prefixed form.
fn resolve_real(p: PathBuf) -> PathBuf {
    let c = std::fs::canonicalize(&p).unwrap_or(p);
    let mut s = c.to_string_lossy().into_owned();
    if let Some(stripped) = s.strip_prefix("\\\\?\\") {
        s = stripped.to_string();
    }
    PathBuf::from(s)
}

/// Windows console children (sc.exe, taskkill, ...) emit the ANSI/OEM code
/// page (GBK on zh-CN). Prefer plain UTF-8 when the bytes are already valid
/// UTF-8; otherwise decode as GBK; last resort is lossy UTF-8.
fn decode_console(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    #[cfg(windows)]
    {
        use encoding_rs::GBK;
        let (cow, _, _) = GBK.decode(bytes);
        if !cow.contains('\u{FFFD}') {
            return cow.into_owned();
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
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
    // cloudflared's own `service install` can silently skip creation when a
    // stale EventLog registry key exists ("registry key already exists"), so
    // we create the Windows service directly with sc.exe instead.
    let q = sc("query");
    if q.ok && !q.stdout.contains("1060") {
        return CmdResult {
            ok: true,
            stdout: "服务已存在，无需重复安装".to_string(),
            stderr: String::new(),
            error: None,
        };
    }
    let bin_path = format!(
        "\"{}\" tunnel --config \"{}\" run",
        binary.display(),
        crate::config::config_path().display()
    );
    let mut c = Command::new("sc.exe");
    c.args([
        "create",
        SERVICE_NAME,
        "binPath=",
        &bin_path,
        "start=",
        "auto",
        "DisplayName=",
        "cloudflared Tunnel",
    ]);
    run(&mut c)
}

pub fn service_uninstall(_binary: &PathBuf) -> CmdResult {
    let mut c = Command::new("sc.exe");
    c.args(["delete", SERVICE_NAME]);
    run(&mut c)
}

/// Logical name the service can be controlled with. cloudflared registers the
/// service as "Cloudflared" (SCM lookups are case-insensitive, but keep the
/// canonical capital-C name for clarity).
pub const SERVICE_NAME: &str = "Cloudflared";

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

/// `cloudflared tunnel route dns <tunnel> <hostname>`
/// Creates the DNS CNAME record that points a hostname at the tunnel. Without
/// this the ingress rules in config.yml never receive traffic.
pub fn route_dns(binary: &PathBuf, tunnel: &str, hostname: &str) -> CmdResult {
    let mut c = Command::new(binary);
    c.args(["tunnel", "route", "dns", tunnel, hostname]);
    run(&mut c)
}

/// `cloudflared tunnel ingress validate`
/// Validates the ingress rules in config.yml against the remote edge.
pub fn ingress_validate(binary: &PathBuf) -> CmdResult {
    let mut c = Command::new(binary);
    c.args(["tunnel", "ingress", "validate"]);
    run(&mut c)
}

/// `cloudflared tunnel ingress rule <url>`
/// Shows which ingress rule a given URL would match.
pub fn ingress_rule(binary: &PathBuf, url: &str) -> CmdResult {
    let mut c = Command::new(binary);
    c.args(["tunnel", "ingress", "rule", url]);
    run(&mut c)
}

/// `cloudflared tunnel token <name>`
/// Prints a token that can be used with `tunnel run --token`.
pub fn tunnel_token(binary: &PathBuf, name: &str) -> CmdResult {
    let mut c = Command::new(binary);
    c.args(["tunnel", "token", name]);
    run(&mut c)
}

/// `cloudflared update` — updates the cloudflared binary itself.
pub fn update(binary: &PathBuf) -> CmdResult {
    run(Command::new(binary).arg("update"))
}

/// Run any cloudflared command from pre-split args (generic shell on the
/// Commands tab). Args are the part after `cloudflared`, e.g.
/// `["tunnel", "list"]` or `["version"]`.
pub fn generic(binary: &PathBuf, args: &[String]) -> CmdResult {
    let mut c = Command::new(binary);
    c.args(args);
    run(&mut c)
}

/// A running `cloudflared.exe` process discovered on the machine. This covers
/// tunnels started as plain processes (e.g. `cloudflared tunnel --config
/// config.yml run`), which the Windows-service query alone would miss.
#[derive(Debug, Clone, Default)]
pub struct CloudflaredProc {
    pub pid: u32,
    pub command_line: String,
    pub start_time: String,
}

#[derive(serde::Deserialize)]
struct ProcJson {
    #[serde(rename = "ProcessId")]
    pid: u32,
    #[serde(rename = "CommandLine")]
    command_line: Option<String>,
    #[serde(rename = "StartTime")]
    start_time: Option<String>,
}

impl From<ProcJson> for CloudflaredProc {
    fn from(p: ProcJson) -> Self {
        Self {
            pid: p.pid,
            command_line: p.command_line.unwrap_or_default(),
            start_time: p.start_time.unwrap_or_default(),
        }
    }
}

/// Discover any running `cloudflared.exe` (any working mode: service child,
/// plain `tunnel run`, token mode, etc.) via WMI/CIM.
pub fn find_running_process() -> Option<CloudflaredProc> {
    let script = "Get-CimInstance Win32_Process -Filter \"Name='cloudflared.exe'\" | \
        Select-Object ProcessId,CommandLine,@{n='StartTime';e={$_.CreationDate.ToLocalTime().ToString('yyyy-MM-dd HH:mm:ss')}} | \
        ConvertTo-Json -Compress";
    let mut cmd = std::process::Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim();
    if text.is_empty() || text == "null" {
        return None;
    }
    let parsed: Vec<ProcJson> = if text.starts_with('[') {
        serde_json::from_str(text).ok()?
    } else {
        serde_json::from_str::<ProcJson>(text).ok().map(|p| vec![p])?
    };
    parsed.into_iter().next().map(Into::into)
}

/// Kill a cloudflared process by PID (`taskkill /F`).
pub fn kill_process(pid: u32) -> CmdResult {
    let mut c = Command::new("taskkill.exe");
    c.args(["/PID", &pid.to_string(), "/F", "/T"]);
    run(&mut c)
}

/// Relaunch this app elevated via a UAC prompt. On success the caller should
/// quit so the elevated instance takes over.
pub fn relaunch_elevated() -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let exe = std::env::current_exe()?;
        let verb: Vec<u16> = "runas\0".encode_utf16().collect();
        let file: Vec<u16> = exe
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        // ShellExecuteW returns a value <= 32 on failure.
        if (result as isize) <= 32 {
            return Err(anyhow!(
                "提权失败 (ShellExecuteW={})，可能取消了 UAC 授权",
                result as isize
            ));
        }
        return Ok(());
    }
    #[cfg(not(windows))]
    Err(anyhow!("当前平台不支持自动提权"))
}

/// Is the current process running elevated (administrator)? `whoami /groups`
/// lists S-1-16-12288 (High Mandatory Level) only when elevated.
pub fn is_elevated() -> bool {
    let mut cmd = Command::new("whoami.exe");
    cmd.arg("/groups");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output().ok();
    match out {
        Some(o) => String::from_utf8_lossy(&o.stdout).contains("S-1-16-12288"),
        None => false,
    }
}