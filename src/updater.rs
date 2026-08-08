use anyhow::{anyhow, Context, Result};
use std::io;
use std::path::{Path, PathBuf};

pub const ASSET_NAME: &str = "cloudflare-tunnel-manager.exe";

/// GitHub API endpoint for the latest release.
const LATEST_URL: &str = concat!(
    "https://api.github.com/repos/",
    "siciyuan404/cloudflare-tunnel-gui/releases/latest"
);

/// Info about the latest release asset we care about.
#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    pub version: String, // e.g. "v0.1.2" or "0.1.2"
    pub asset_url: String,
    pub asset_size: u64,
}

/// Strip a leading "v".
fn strip_v(s: &str) -> &str {
    s.trim().trim_start_matches('v')
}

/// Compare two dotted versions (a leading "v" is tolerated).
/// Returns Ok(true) when `current` (this app) is older than `latest`.
pub fn is_outdated(current: &str, latest: &str) -> Result<bool> {
    let cur = parse_parts(strip_v(current))?;
    let new = parse_parts(strip_v(latest))?;
    Ok(cur < new)
}

fn parse_parts(s: &str) -> Result<Vec<u64>> {
    s.trim()
        .split('.')
        .map(|p| {
            p.trim()
                .parse::<u64>()
                .map_err(|_| anyhow!("非法版本段: {p:?}"))
        })
        .collect()
}

/// Query the GitHub API for the latest release and the matching asset.
pub fn fetch_latest() -> Result<ReleaseInfo> {
    let resp = ureq::get(LATEST_URL)
        .set("User-Agent", "cloudflare-tunnel-manager-updater")
        .set("Accept", "application/vnd.github+json")
        .call()
        .context("无法访问 GitHub API")?;
    let status = resp.status();
    let body = resp.into_string().context("读取 GitHub 响应失败")?;
    if status != 200 {
        let msg = body.lines().next().unwrap_or("").to_string();
        return Err(anyhow!("GitHub API 返回 {status}: {msg}"));
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).context("解析 Release JSON 失败")?;
    let version = v
        .get("tag_name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let asset = v
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|arr| arr.iter().find(|a| a["name"].as_str() == Some(ASSET_NAME)));
    let (asset_url, asset_size) = match asset {
        Some(a) => (
            a["browser_download_url"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            a["size"].as_u64().unwrap_or(0),
        ),
        None => return Err(anyhow!("最新 Release 中未找到 {ASSET_NAME}")),
    };
    if version.is_empty() || asset_url.is_empty() {
        return Err(anyhow!("Release 信息不完整"));
    }
    Ok(ReleaseInfo {
        version,
        asset_url,
        asset_size,
    })
}

/// Stream-download the asset to `dest`.
pub fn download(url: &str, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).context("创建下载目录失败")?;
    }
    let resp = ureq::get(url)
        .set("User-Agent", "cloudflare-tunnel-manager-updater")
        .call()
        .context("下载请求失败")?;
    let mut reader = resp.into_reader();
    let mut f = std::fs::File::create(dest).context("创建临时文件失败")?;
    io::copy(&mut reader, &mut f).context("写入文件失败")?;
    Ok(())
}

/// A temp download location in the same directory as the running exe (keeps a
/// cross-volume `move` from failing).
pub fn staged_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("获取当前进程路径失败")?;
    let dir = exe.parent().context("无法判断安装目录")?;
    Ok(dir.join("cloudflare-tunnel-manager.new.exe"))
}

/// Replace the running `exe` with `staged` and relaunch the new binary.
///
/// Windows cannot overwrite a running exe, so this spawns a detached `cmd`
/// that waits ~2s (during which this process is told to exit), then
/// `move`-replaces the exe and starts the freshly updated copy.
pub fn install_and_restart(exe: &Path, staged: &Path) -> Result<()> {
    let exe = exe.to_string_lossy().into_owned();
    let staged = staged.to_string_lossy().into_owned();
    let script = format!(
        "timeout /t 2 /nobreak >nul & move /Y \"{staged}\" \"{exe}\" >nul & start \"\" \"{exe}\" >nul"
    );
    let mut cmd = std::process::Command::new("cmd.exe");
    cmd.arg("/c").arg(&script);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    }
    cmd.spawn().context("启动更新进程失败")?;
    Ok(())
}