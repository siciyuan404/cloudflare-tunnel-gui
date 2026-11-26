use crate::cloudflared::{self, CmdResult};
use crate::config::TunnelConfig;
use crate::tasks::{Done, Runner};
use crate::updater;
use chrono::Local;
use eframe::egui;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Live state of the silent updater, shown in the sidebar.
#[derive(Clone, Debug)]
enum UpdateState {
    Checking,
    Current,
    Available(updater::ReleaseInfo),
    Downloading,
    Restarting,
    Error(String),
}

struct LogLine {
    time: String,
    tag: String,
    text: String,
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Status,
    Tunnels,
    Config,
    Commands,
    Logs,
}

pub struct ManagerApp {
    runner: Runner,
    binary: Option<PathBuf>,

    cloudflared_version: String,
    service_status: String,
    proc_info: Option<cloudflared::CloudflaredProc>,
    elevated: bool,

    tunnels: Vec<crate::config::TunnelEntry>,
    selected_tunnel: Option<String>,

    config: TunnelConfig,
    config_error: Option<String>,

    logs: Vec<LogLine>,
    log_text: String,
    run_text: String,
    pending: HashMap<u64, String>,

    // A short-lived banner so every action gives visible feedback instead of
    // only writing to the Logs tab.
    toast_msg: Option<(String, std::time::Instant)>,

    // Streaming tunnel-run viewer.
    run_log: Arc<Mutex<Vec<String>>>,
    run_child: Option<Child>,
    run_in_progress: bool,

    // Form scratch.
    create_name: String,
    ingress_hostname: String,
    ingress_path: String,
    ingress_service: String,
    route_hostname: String,
    cmd_input: String,
    rule_url: String,
    token_input: String,
    raw_config: String,

    // Silenced self-update (GitHub Releases).
    update: Arc<Mutex<UpdateState>>,
    close_requested: Arc<AtomicBool>,

    // Set by the tray "退出" menu (or self-update) to allow a real quit;
    // a plain window close (X) hides to the tray instead.
    allow_exit: Arc<AtomicBool>,

    tab: Tab,
}

fn now() -> String {
    Local::now().format("%H:%M:%S").to_string()
}

fn fmt_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

impl Default for ManagerApp {
    fn default() -> Self {
        Self::new(Arc::new(AtomicBool::new(false)))
    }
}

impl ManagerApp {
    pub fn new(allow_exit: Arc<AtomicBool>) -> Self {
        let config = TunnelConfig::load().unwrap_or_default();
        let mut app = Self {
            runner: Runner::new(),
            binary: cloudflared::find_binary().ok(),
            cloudflared_version: String::new(),
            service_status: String::new(),
            proc_info: None,
            elevated: cloudflared::is_elevated(),
            tunnels: Vec::new(),
            selected_tunnel: None,
            config,
            config_error: None,
            logs: Vec::new(),
            log_text: String::new(),
            run_text: String::new(),
            pending: HashMap::new(),
            toast_msg: None,
            run_log: Arc::new(Mutex::new(Vec::new())),
            run_child: None,
            run_in_progress: false,
            create_name: String::new(),
            ingress_hostname: String::new(),
            ingress_path: String::new(),
            ingress_service: "http://localhost:8080".into(),
            route_hostname: String::new(),
            cmd_input: String::new(),
            rule_url: String::new(),
            token_input: String::new(),
            raw_config: String::new(),
            update: Arc::new(Mutex::new(UpdateState::Checking)),
            close_requested: Arc::new(AtomicBool::new(false)),
            allow_exit,
            tab: Tab::Status,
        };
        app.start_background_update_check();
        app.refresh_all();
        app
    }

    /// Kick off a background check against GitHub Releases for a newer version.
    fn start_background_update_check(&self) {
        let update = self.update.clone();
        let current = env!("CARGO_PKG_VERSION").to_string();
        std::thread::spawn(move || {
            *update.lock().unwrap() = match updater::fetch_latest() {
                Ok(info) => match updater::is_outdated(&current, &info.version) {
                    Ok(true) => UpdateState::Available(info),
                    Ok(false) => UpdateState::Current,
                    Err(e) => UpdateState::Error(e.to_string()),
                },
                Err(e) => UpdateState::Error(e.to_string()),
            };
        });
    }

    /// Download the newer release, stage it beside the exe, then relaunch.
    fn apply_update(&self) {
        let info = match &*self.update.lock().unwrap() {
            UpdateState::Available(i) => i.clone(),
            _ => return,
        };
        let update = self.update.clone();
        let close = self.close_requested.clone();
        std::thread::spawn(move || {
            *update.lock().unwrap() = UpdateState::Downloading;
            let result = (|| -> anyhow::Result<()> {
                let staged = updater::staged_path()?;
                let exe = std::env::current_exe()?;
                updater::download(&info.asset_url, &staged)?;
                updater::install_and_restart(&exe, &staged)?;
                Ok(())
            })();
            match result {
                Ok(()) => {
                    *update.lock().unwrap() = UpdateState::Restarting;
                    close.store(true, Ordering::Relaxed);
                }
                Err(e) => *update.lock().unwrap() = UpdateState::Error(e.to_string()),
            }
        });
    }

    fn log(&mut self, tag: &str, text: &str) {
        self.logs.push(LogLine {
            time: now(),
            tag: tag.to_string(),
            text: text.to_string(),
        });
        // Mirror the first meaningful line into the on-screen banner.
        let first = text
            .lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .unwrap_or("");
        self.toast(format!("[{tag}] {first}"));
    }

    /// Show a transient banner for the next few seconds.
    fn toast(&mut self, msg: impl Into<String>) {
        self.toast_msg = Some((msg.into(), std::time::Instant::now()));
    }

    /// Queue a command to run in the background; captures the binary at queue
    /// time so the closure stays `'static`.
    fn spawn(&mut self, label: impl Into<String>, f: impl FnOnce(PathBuf) -> CmdResult + Send + 'static) {
        let label = label.into();
        let bin = match self.binary.clone() {
            Some(b) => b,
            None => {
                self.log("error", "cloudflared 二进制不可用");
                return;
            }
        };
        let lbl = label.clone();
        let id = self.runner.spawn(label.clone(), Box::new(move || f(bin)));
        self.pending.insert(id, lbl);
        self.toast(format!("已执行：{label}（后台运行，结果稍后显示）"));
    }

    fn refresh_all(&mut self) {
        self.refresh_status();
        self.list_tunnels();
    }

    fn refresh_status(&mut self) {
        let bin = match self.binary.clone() {
            Some(b) => b,
            None => {
                self.cloudflared_version = "(not found)".into();
                self.service_status = "cloudflared 未找到".into();
                return;
            }
        };
        let v = cloudflared::version(&bin);
        self.cloudflared_version = v.stdout.trim().to_string();
        self.service_status = cloudflared::service_query_status();
        self.proc_info = cloudflared::find_running_process();
    }

    fn list_tunnels(&mut self) {
        if !self.pending.values().any(|s| s == "tunnel list") {
            self.spawn("tunnel list", |b| cloudflared::tunnel_list(&b));
        }
    }

    /// Poll for finished background jobs and route results.
    fn handle_jobs(&mut self) {
        let done: Vec<Done> = self.runner.drain();
        for d in done {
            let tag = self.pending.remove(&d.id).unwrap_or_else(|| d.label.clone());
            // Structured update for tunnel list.
            if tag.starts_with("tunnel list") {
                self.tunnels = crate::config::parse_tunnel_list(&d.result.stdout);
                if self.selected_tunnel.is_none() {
                    self.selected_tunnel = self
                        .tunnels
                        .first()
                        .map(|t| t.name.clone())
                        .or_else(|| self.config.tunnel.clone());
                }
            }
            // Auto-select the newly created tunnel.
            if tag.starts_with("tunnel create") {
                self.selected_tunnel = None;
            }
            // Re-read process + service state after any state-changing command.
            if tag.starts_with("接管为服务") || tag.starts_with("service ") {
                self.refresh_status();
            }
            self.log(&tag, &d.result.output_text());
        }
    }

    /// Spawn `cloudflared <args>` in the background and stream its stdout/stderr
    /// into the run-log buffer. Used both for `tunnel run <name>` and for
    /// `tunnel run --token <token>`.
    fn open_run(&mut self, args: Vec<String>) {
        self.log("info", &format!("启动 `cloudflared {}`", args.join(" ")));
        self.run_log.lock().unwrap().clear();
        self.run_in_progress = true;
        let buf = self.run_log.clone();
        let bin = match self.binary.clone() {
            Some(b) => b,
            None => return,
        };
        let mut cmd = std::process::Command::new(bin);
        cmd.args(&args);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        match cmd.spawn() {
            Ok(mut child) => {
                if let Some(out) = child.stdout.take() {
                    let b = buf.clone();
                    std::thread::spawn(move || {
                        use std::io::{BufRead, BufReader};
                        for line in BufReader::new(out).lines().flatten() {
                            b.lock().unwrap().push(line);
                        }
                    });
                }
                if let Some(err) = child.stderr.take() {
                    let b = buf.clone();
                    std::thread::spawn(move || {
                        use std::io::{BufRead, BufReader};
                        for line in BufReader::new(err).lines().flatten() {
                            b.lock().unwrap().push(format!("[err] {line}"));
                        }
                    });
                }
                self.run_child = Some(child);
            }
            Err(e) => {
                self.log("error", &format!("无法启动: {e}"));
                self.run_in_progress = false;
            }
        }
    }

    fn open_tunnel_run(&mut self, tunnel: &str) {
        self.open_run(vec!["tunnel".into(), "run".into(), tunnel.into()]);
    }

    fn stop_tunnel_run(&mut self) {
        if let Some(mut child) = self.run_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.run_in_progress = false;
        self.log("info", "隧道已停止");
    }

    /// One-click take-over: stop any plain cloudflared process, install the
    /// Windows service, start it, then refresh state. Requires admin rights.
    fn spawn_takeover(&mut self) {
        let pid = self.proc_info.as_ref().map(|p| p.pid);
        self.spawn("接管为服务", move |b| {
            let mut text = String::new();
            if let Some(pid) = pid {
                text.push_str(&format!("1/3 停止现有穿透进程 PID {pid}\n"));
                let r = cloudflared::kill_process(pid);
                text.push_str(&r.output_text());
                text.push('\n');
            }
            text.push_str("2/3 安装 Windows 服务（需管理员权限）\n");
            let r = cloudflared::service_install(&b);
            text.push_str(&r.output_text());
            text.push('\n');
            text.push_str("3/3 启动服务\n");
            let r = cloudflared::sc("start");
            text.push_str(&r.output_text());
            CmdResult {
                ok: true,
                stdout: text,
                stderr: String::new(),
                error: None,
            }
        });
    }

    // ---------- UI panels ----------

    fn ui_status(&mut self, ui: &mut egui::Ui) {
        ui.heading("cloudflared 状态");
        ui.add_space(6.0);
        egui::Grid::new("status_grid").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
            ui.label("二进制路径:");
            ui.monospace(
                self.binary
                    .as_ref()
                    .map(|b| b.display().to_string())
                    .unwrap_or_else(|| "未找到".into()),
            );
            ui.end_row();
            ui.label("版本:");
            ui.monospace(&self.cloudflared_version);
            ui.end_row();
            ui.label("服务状态:");
            let st = &self.service_status;
            let color = if st.contains("RUNNING") {
                egui::Color32::from_rgb(76, 175, 80)
            } else if st.contains("STOPPED") || st.contains("NOT") {
                egui::Color32::from_rgb(244, 67, 54)
            } else {
                egui::Color32::from_rgb(255, 183, 77)
            };
            ui.colored_label(color, st);
            ui.end_row();
            ui.label("穿透进程:");
            match &self.proc_info {
                Some(p) => {
                    ui.colored_label(
                        egui::Color32::from_rgb(76, 175, 80),
                        format!("运行中 · PID {} · {}", p.pid, p.start_time),
                    );
                    ui.end_row();
                    ui.label("");
                    ui.monospace(&p.command_line);
                }
                None => {
                    ui.colored_label(egui::Color32::from_rgb(158, 158, 158), "未运行");
                    ui.end_row();
                }
            }
            ui.label("隧道数量:");
            ui.monospace(self.tunnels.len().to_string());
            ui.end_row();
            ui.label("配置路径:");
            ui.monospace(crate::config::config_path().display().to_string());
            ui.end_row();
            ui.label("凭据文件:");
            let creds = self.config.find_credentials_files();
            ui.monospace(
                creds
                    .first()
                    .map(|c| c.display().to_string())
                    .unwrap_or_else(|| "未找到（先 login）".into()),
            );
            ui.end_row();
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.button("刷新状态").clicked() {
                self.refresh_status();
                self.toast("状态已刷新");
            }
            if ui.button("安装服务").on_hover_text("需要管理员权限").clicked() {
                if !self.elevated {
                    self.toast("安装服务需要管理员权限：请点击下方“以管理员身份重启”");
                } else {
                    self.toast("正在安装服务…（结果见日志）");
                    self.spawn("service install", |b| cloudflared::service_install(&b));
                }
            }
            if ui.button("卸载服务").clicked() {
                if !self.elevated {
                    self.toast("卸载服务需要管理员权限：请点击下方“以管理员身份重启”");
                } else {
                    self.toast("正在卸载服务…");
                    self.spawn("service uninstall", |b| cloudflared::service_uninstall(&b));
                }
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("启动服务").clicked() {
                if !self.elevated {
                    self.toast("启动/停止服务需要管理员权限：请点击下方“以管理员身份重启”");
                } else {
                    self.toast("正在启动服务…");
                    self.spawn("service start", |_b| cloudflared::sc("start"));
                }
            }
            if ui.button("停止服务").clicked() {
                if !self.elevated {
                    self.toast("启动/停止服务需要管理员权限：请点击下方“以管理员身份重启”");
                } else {
                    self.toast("正在停止服务…");
                    self.spawn("service stop", |_b| cloudflared::sc("stop"));
                }
            }
            if ui.button("重启服务").clicked() {
                if !self.elevated {
                    self.toast("重启服务需要管理员权限：请点击下方“以管理员身份重启”");
                } else {
                    self.toast("正在重启服务…");
                    self.spawn("service restart", |_b| {
                        let _ = cloudflared::sc("stop");
                        cloudflared::sc("start")
                    });
                }
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if let Some(p) = &self.proc_info {
                let pid = p.pid;
                if ui.button("停止穿透进程").on_hover_text("taskkill /F 结束普通进程方式的 cloudflared").clicked() {
                    self.spawn(format!("kill 进程 {pid}"), move |_| cloudflared::kill_process(pid));
                }
                let svc_installed = !self.service_status.contains("NOT") && !self.service_status.contains("1060");
                if ui
                    .add_enabled(!svc_installed, egui::Button::new("一键接管为服务"))
                    .on_hover_text("停掉当前普通进程 → 安装 Windows 服务 → 启动服务。之后由系统服务托管、开机自启，状态页显示 RUNNING")
                    .clicked()
                {
                    self.spawn_takeover();
                }
            } else {
                ui.weak("（无穿透进程在运行）");
            }
        });
        ui.horizontal(|ui| {
            if self.elevated {
                ui.weak("权限状态：管理员 —— 可安装/卸载/启停服务");
            } else {
                ui.colored_label(
                    egui::Color32::from_rgb(255, 183, 77),
                    "权限状态：普通用户 —— 安装/卸载/启停服务需要管理员权限",
                );
                if ui.button("以管理员身份重启").clicked() {
                    match cloudflared::relaunch_elevated() {
                        Ok(()) => {
                            // The elevated instance is starting; quit this one
                            // so it can replace the window.
                            self.close_requested.store(true, Ordering::Relaxed);
                            self.toast("已请求提权，正在关闭当前窗口…");
                        }
                        Err(e) => self.toast(format!("提权失败: {e}")),
                    }
                }
            }
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("校验配置 (ingress validate)").on_hover_text("验证 config.yml 的 ingress 规则").clicked() {
                self.spawn("ingress validate", |b| cloudflared::ingress_validate(&b));
            }
            let sel = self.selected_tunnel.clone();
            if ui.add_enabled(sel.is_some(), egui::Button::new("查看选中隧道连接 (info)")).clicked() {
                if let Some(n) = sel {
                    self.spawn(format!("info {n}"), move |b| cloudflared::tunnel_info(&b, &n));
                }
            }
        });
        ui.add_space(10.0);
        ui.separator();
        ui.weak("提示：安装/卸载服务需要管理员权限。请以管理员身份运行本程序后再操作。");
    }

    fn ui_tunnels(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("隧道管理");
            if ui.button("刷新列表").clicked() {
                self.list_tunnels();
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("登录 / 认证:");
            if ui.button("login").on_hover_text("打开浏览器完成 Cloudflare 登录").clicked() {
                self.spawn("login", |b| cloudflared::login(&b));
            }
            ui.separator();
            ui.label("新建隧道:");
            ui.text_edit_singleline(&mut self.create_name);
            if ui.add_enabled(!self.create_name.trim().is_empty(), egui::Button::new("创建")).clicked() {
                let n = self.create_name.trim().to_string();
                if !n.is_empty() {
                    self.spawn(format!("tunnel create {n}"), move |b| cloudflared::tunnel_create(&b, &n));
                    self.create_name.clear();
                }
            }
        });
        ui.add_space(6.0);
        let mut sel = self.selected_tunnel.clone();
        let mut info_request: Option<String> = None;
        let tunnels = self.tunnels.clone();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if tunnels.is_empty() {
                ui.weak("没有隧道。先 login，或点击“刷新列表”。");
            }
            for t in tunnels.iter() {
                ui.horizontal(|ui| {
                    if ui.radio(sel.as_deref() == Some(t.name.as_str()), t.name.clone()).clicked() {
                        sel = Some(t.name.clone());
                    }
                    if !t.connections.is_empty() {
                        ui.colored_label(
                            egui::Color32::from_rgb(76, 175, 80),
                            format!("连接中 [{}]", t.connections),
                        );
                    } else {
                        ui.weak("未运行");
                    }
                    ui.weak(&t.id);
                    ui.weak(&t.created);
                    if ui.button("info").clicked() {
                        info_request = Some(t.name.clone());
                    }
                });
            }
        });
        self.selected_tunnel = sel;
        if let Some(n) = info_request {
            self.spawn(format!("info {n}"), move |b| cloudflared::tunnel_info(&b, &n));
        }
        ui.add_space(6.0);
        let has_sel = self.selected_tunnel.as_deref().is_some_and(|s| !s.is_empty());
        ui.horizontal(|ui| {
            let on = self.selected_tunnel.clone().unwrap_or_default();
            if ui.add_enabled(has_sel && !self.run_in_progress, egui::Button::new("运行隧道")).clicked() {
                self.open_tunnel_run(&on);
            }
            if ui.add_enabled(has_sel, egui::Button::new("删除")).clicked() {
                let n = on.clone();
                self.spawn(format!("tunnel delete {n}"), move |b| cloudflared::tunnel_delete(&b, &n));
            }
            if ui.add_enabled(has_sel, egui::Button::new("cleanup")).clicked() {
                let n = on.clone();
                self.spawn(format!("tunnel cleanup {n}"), move |b| cloudflared::tunnel_cleanup(&b, &n));
            }
            if ui.add_enabled(has_sel, egui::Button::new("生成 token")).on_hover_text("输出可用于 tunnel run --token 的令牌").clicked() {
                let n = on.clone();
                self.spawn(format!("token {n}"), move |b| cloudflared::tunnel_token(&b, &n));
            }
            if ui.add_enabled(has_sel, egui::Button::new("写入 config tunnel")).clicked() {
                self.config.tunnel = Some(on.clone());
                match self.config.save_merged() {
                    Ok(_) => self.log("config", &format!("tunnel 设为 {on}（保留未知字段）")),
                    Err(e) => self.log("config error", &e.to_string()),
                }
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("route dns 域名:");
            ui.add(
                egui::TextEdit::singleline(&mut self.route_hostname)
                    .hint_text("如 app.example.com —— 绑定域名到选中隧道"),
            );
            if ui.add_enabled(has_sel && !self.route_hostname.trim().is_empty(), egui::Button::new("绑定 (route dns)"))
                .on_hover_text("cloudflared tunnel route dns：创建域名 → 隧道的 CNAME，穿透生效必需")
                .clicked()
            {
                let n = self.selected_tunnel.clone().unwrap_or_default();
                let h = self.route_hostname.trim().to_string();
                self.spawn(format!("route dns {n} {h}"), move |b| cloudflared::route_dns(&b, &n, &h));
                self.route_hostname.clear();
            }
        });
        if self.run_in_progress {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("隧道已在后台运行，请查看“隧道运行日志”。");
                if ui.button("停止隧道").clicked() {
                    self.stop_tunnel_run();
                }
            });
        }
    }

    fn ui_config(&mut self, ui: &mut egui::Ui) {
        ui.heading("配置 config.yml（整机全局）");
        ui.add_space(4.0);
        ui.weak(format!("全局路径: {}", crate::config::config_path().display()));
        if crate::config::TunnelConfig::is_legacy() {
            ui.colored_label(
                egui::Color32::from_rgb(255, 183, 77),
                "当前读取的是旧用户目录配置；保存后将写入上面的全局路径（需要管理员权限）。",
            );
        }
        if !crate::config::global_dir_writable() {
            ui.colored_label(
                egui::Color32::from_rgb(244, 67, 54),
                "全局配置目录不可写 —— 请以管理员身份重启后再保存。",
            );
        }
        ui.separator();

        ui.horizontal(|ui| {
            ui.label("tunnel:");
            let mut t = self.config.tunnel.clone().unwrap_or_default();
            if ui.text_edit_singleline(&mut t).changed() {
                self.config.tunnel = if t.trim().is_empty() { None } else { Some(t) };
            }
        });
        ui.horizontal(|ui| {
            ui.label("协议:");
            let mut proto = self.config.protocol.clone().unwrap_or_default();
            if ui.text_edit_singleline(&mut proto).changed() {
                self.config.protocol = if proto.trim().is_empty() { None } else { Some(proto) };
            }
        });
        ui.horizontal(|ui| {
            ui.label("credentials-file:");
            ui.monospace(self.config.credentials_file.clone().unwrap_or_default());
        });
        ui.weak("凭据文件会自动探测全局与用户目录；保存时会复制到全局目录，确保服务（SYSTEM）可读。");

        ui.add_space(8.0);
        ui.strong("Ingress 规则");
        ui.horizontal(|ui| {
            ui.label("主机名:");
            ui.add(
                egui::TextEdit::singleline(&mut self.ingress_hostname)
                    .hint_text("如 app.example.com（留空 = fallback）"),
            );
            ui.label("路径:");
            ui.add(
                egui::TextEdit::singleline(&mut self.ingress_path)
                    .hint_text("如 /api（可选）"),
            );
            ui.label("服务:");
            ui.text_edit_singleline(&mut self.ingress_service);
            let valid = !self.ingress_service.trim().is_empty();
            if ui.add_enabled(valid, egui::Button::new("添加")).clicked() {
                let svc = self.ingress_service.trim().to_string();
                let mut rule = crate::config::Ingress { hostname: None, path: None, service: svc };
                if !self.ingress_hostname.trim().is_empty() {
                    rule.hostname = Some(self.ingress_hostname.trim().to_string());
                }
                if !self.ingress_path.trim().is_empty() {
                    rule.path = Some(self.ingress_path.trim().to_string());
                }
                self.config.ingress.push(rule);
                self.ingress_service = "http://localhost:8080".into();
                self.ingress_hostname.clear();
                self.ingress_path.clear();
            }
        });
        let mut remove: Option<usize> = None;
        let mut move_up: Option<usize> = None;
        let mut move_down: Option<usize> = None;
        let mut ingress = self.config.ingress.clone();
        for (i, r) in ingress.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("{}", i + 1));
                let shown = match (&r.hostname, &r.path) {
                    (Some(h), Some(p)) => format!("{h}{p}\u{2192}{}", r.service),
                    (Some(h), None) => format!("{h}\u{2192}{}", r.service),
                    (None, _) => format!("(fallback) {}{}", r.service, r.path.clone().unwrap_or_default()),
                };
                ui.monospace(shown);
                if ui.add_enabled(i > 0, egui::Button::new("↑")).on_hover_text("上移（规则按顺序匹配，靠前优先）").clicked() {
                    move_up = Some(i);
                }
                if ui.add_enabled(i + 1 < ingress.len(), egui::Button::new("↓")).clicked() {
                    move_down = Some(i);
                }
                if ui.button("删除").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = move_up {
            ingress.swap(i, i - 1);
        }
        if let Some(i) = move_down {
            ingress.swap(i, i + 1);
        }
        if let Some(i) = remove {
            ingress.remove(i);
        }
        self.config.ingress = ingress;

        ui.add_space(10.0);
        if let Some(err) = &self.config_error {
            ui.colored_label(egui::Color32::RED, err);
        }
        ui.horizontal(|ui| {
            if ui.button("保存配置").clicked() {
                if self.config.credentials_file.is_none() {
                    if let Some(f) = self.config.find_credentials_files().first() {
                        self.config.credentials_file = Some(f.display().to_string());
                    }
                }
                // Move credentials into the global dir so the SYSTEM service can
                // read them, then persist the config there.
                let norm = self.config.normalize_credentials_to_global();
                match norm.and_then(|_| self.config.save_merged()) {
                    Ok(_) => {
                        self.config_error = None;
                        self.log("config", "config.yml 已保存到全局目录（保留未知字段）");
                    }
                    Err(e) => self.config_error = Some(e.to_string()),
                }
            }
            if ui.button("重新加载").clicked() {
                match TunnelConfig::load() {
                    Ok(c) => {
                        self.config = c;
                        self.config_error = None;
                    }
                    Err(e) => self.config_error = Some(e.to_string()),
                }
            }
            if ui.button("打开配置目录").clicked() {
                let dir = crate::config::config_dir();
                let _ = std::process::Command::new("explorer.exe").arg(dir).spawn();
            }
        });

        ui.separator();
        ui.strong("原始 config.yml 编辑（完整接管：可写 GUI 不识别的字段）");
        ui.horizontal(|ui| {
            if ui.button("加载原始内容").on_hover_text("把当前 config.yml 全文载入下方文本框").clicked() {
                match std::fs::read_to_string(crate::config::config_path()) {
                    Ok(t) => {
                        self.raw_config = t;
                        self.config_error = None;
                    }
                    Err(e) => self.config_error = Some(format!("读取失败: {e}")),
                }
            }
            if ui.button("校验并保存原始配置").on_hover_text("YAML 语法校验通过后整体写回文件").clicked() {
                self.save_raw_config();
            }
        });
        ui.add(
            egui::TextEdit::multiline(&mut self.raw_config)
                .desired_rows(12)
                .desired_width(f32::INFINITY)
                .font(egui::TextStyle::Monospace),
        );
    }

    /// Parse + write the raw YAML editor content back to config.yml.
    /// Requires valid YAML syntax; structure fields the GUI doesn't know are
    /// preserved as-is (the file is written verbatim).
    fn save_raw_config(&mut self) {
        let trimmed = self.raw_config.trim().to_string();
        if trimmed.is_empty() {
            self.config_error = Some("原始配置为空，未保存".into());
            return;
        }
        match serde_yaml::from_str::<serde_yaml::Value>(&trimmed) {
            Ok(_) => {
                let mut warn: Option<String> = None;
                if let Err(e) = serde_yaml::from_str::<TunnelConfig>(&trimmed) {
                    warn = Some(format!("警告：图形化视图无法完整解析（{e}），但已按原样保存"));
                }
                match std::fs::write(crate::config::config_path(), &trimmed) {
                    Ok(_) => {
                        self.config_error = warn;
                        self.log("config", "原始 config.yml 已保存");
                        self.config = TunnelConfig::load().unwrap_or_default();
                    }
                    Err(e) => self.config_error = Some(format!("写入失败: {e}")),
                }
            }
            Err(e) => self.config_error = Some(format!("YAML 语法错误，未保存: {e}")),
        }
    }

    fn ui_logs(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("命令日志");
            if ui.button("复制全部").clicked() {
                ui.ctx().copy_text(self.log_text.clone());
            }
            if ui.button("清空命令日志").clicked() {
                self.logs.clear();
                self.log_text.clear();
            }
        });
        // Rebuild the selectable buffer so the whole block can be drag-selected
        // and copied (egui labels are not selectable by default).
        self.log_text = self
            .logs
            .iter()
            .take(400)
            .map(|l| format!("{} [{}] {}", l.time, l.tag, l.text))
            .collect::<Vec<_>>()
            .join("\n");
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .max_height(240.0)
            .show(ui, |ui| {
                if self.log_text.is_empty() {
                    ui.weak("暂无命令日志。");
                } else {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.log_text)
                            .desired_width(f32::INFINITY)
                            .desired_rows(10)
                            .font(egui::TextStyle::Monospace)
                            .interactive(true),
                    );
                }
            });
        ui.add_space(10.0);
        ui.separator();
        ui.horizontal(|ui| {
            ui.heading("隧道运行日志");
            if ui.button("复制全部").clicked() {
                ui.ctx().copy_text(self.run_text.clone());
            }
            if self.run_in_progress {
                ui.spinner();
                if ui.button("停止隧道").clicked() {
                    self.stop_tunnel_run();
                }
            }
        });
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                let lines = self.run_log.lock().unwrap().clone();
                self.run_text = lines.join("\n");
                if self.run_text.is_empty() {
                    ui.weak("（隧道未运行或无输出）");
                } else {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.run_text)
                            .desired_width(f32::INFINITY)
                            .desired_rows(10)
                            .font(egui::TextStyle::Monospace)
                            .interactive(true),
                    );
                }
            });
    }

    /// Generic command shell: run any `cloudflared <args>` from user input.
    fn run_generic(&mut self, args: Vec<String>) {
        if args.is_empty() {
            return;
        }
        let label = format!("$ cloudflared {}", args.join(" "));
        self.spawn(label, move |b| cloudflared::generic(&b, &args));
    }

    fn ui_commands(&mut self, ui: &mut egui::Ui) {
        ui.heading("通用命令执行");
        ui.add_space(4.0);
        ui.weak("输入 cloudflared 的参数（不用写 cloudflared 本身），例如：tunnel route dns my-tunnel app.example.com");
        ui.horizontal(|ui| {
            ui.monospace("$ cloudflared");
            let edit = ui.add(
                egui::TextEdit::singleline(&mut self.cmd_input)
                    .desired_width(320.0)
                    .hint_text("如 tunnel ingress validate"),
            );
            let run = ui.add_enabled(!self.cmd_input.trim().is_empty(), egui::Button::new("执行"));
            let submit = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if run.clicked() || submit {
                let args: Vec<String> = self.cmd_input.split_whitespace().map(str::to_string).collect();
                if !args.is_empty() {
                    self.run_generic(args);
                    self.cmd_input.clear();
                }
            }
        });
        ui.add_space(8.0);
        ui.strong("快捷命令");
        ui.horizontal_wrapped(|ui| {
            if ui.button("ingress validate").on_hover_text("校验 config.yml 的 ingress 规则").clicked() {
                self.spawn("ingress validate", |b| cloudflared::ingress_validate(&b));
            }
            if ui.button("version").clicked() {
                self.run_generic(vec!["version".into()]);
            }
            if ui.button("tunnel list").clicked() {
                self.run_generic(vec!["tunnel".into(), "list".into()]);
            }
            if ui.button("help").on_hover_text("列出 cloudflared 全部顶层命令").clicked() {
                self.run_generic(vec!["help".into()]);
            }
            if ui.button("update（升级 cloudflared 二进制）")
                .on_hover_text("cloudflared update：更新的是 cloudflared 本身，不是本管理器")
                .clicked()
            {
                self.spawn("update", |b| cloudflared::update(&b));
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("ingress rule 测试:");
            ui.add(
                egui::TextEdit::singleline(&mut self.rule_url)
                    .desired_width(300.0)
                    .hint_text("https://app.example.com/api"),
            );
            if ui.add_enabled(!self.rule_url.trim().is_empty(), egui::Button::new("看哪条规则命中")).clicked() {
                let url = self.rule_url.trim().to_string();
                self.spawn(format!("ingress rule {url}"), move |b| cloudflared::ingress_rule(&b, &url));
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("token 运行:");
            ui.add(
                egui::TextEdit::singleline(&mut self.token_input)
                    .desired_width(300.0)
                    .hint_text("粘贴 tunnel token 生成的令牌"),
            );
            if ui.add_enabled(!self.token_input.trim().is_empty() && !self.run_in_progress, egui::Button::new("tunnel run --token")).clicked() {
                let t = self.token_input.trim().to_string();
                self.open_run(vec!["tunnel".into(), "run".into(), "--token".into(), t]);
                self.token_input.clear();
            }
        });
        if self.run_in_progress {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("命令已在后台运行，实时输出见“日志”页");
                if ui.button("停止").clicked() {
                    self.stop_tunnel_run();
                }
            });
        }
        ui.add_space(8.0);
        ui.weak("提示：login（请用“隧道”页按钮）、tail、proxy-dns 等交互式或常驻命令不适合在本页执行；带引号/含空格的参数暂不支持（本页按空格分词）。");
    }

    /// Sidebar updater panel: silent, background-driven.
    fn ui_update_panel(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        let state = self.update.lock().unwrap().clone();
        match state {
            UpdateState::Checking => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak("检查更新…");
                });
            }
            UpdateState::Current => {
                ui.weak("已是最新版本");
            }
            UpdateState::Available(info) => {
                ui.colored_label(
                    egui::Color32::from_rgb(33, 150, 243),
                    format!("有新版 {} 可用", info.version),
                );
                if ui.button(format!("安装并重启（{}）", fmt_size(info.asset_size))).clicked() {
                    self.apply_update();
                }
                ui.weak(format!("当前 v{}", env!("CARGO_PKG_VERSION")));
            }
            UpdateState::Downloading => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak("正在后台下载新版本…");
                });
            }
            UpdateState::Restarting => {
                ui.weak("更新完成，正在重启应用…");
            }
            UpdateState::Error(msg) => {
                ui.weak(format!("更新检查失败: {msg}"));
                if ui.small_button("重试").clicked() {
                    self.start_background_update_check();
                }
            }
        }
    }
}

impl eframe::App for ManagerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_jobs();

        if self.close_requested.load(Ordering::Relaxed) {
            // Self-update finished staging the new exe: quit for real so the
            // updater's relaunch takes over.
            self.allow_exit.store(true, Ordering::Relaxed);
            let _ = ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        // The X button (or any other close request) hides the window into the
        // tray instead of quitting — unless "退出" was picked from the tray
        // menu (handled on a background thread), in which case allow_exit lets
        // the close go through.
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.allow_exit.load(Ordering::Relaxed) {
                // Real quit: do not cancel the close.
            } else {
                let _ = ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                let _ = ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                self.log("info", "窗口已隐藏到系统托盘，点击右下角托盘图标可恢复");
                return;
            }
        }

        egui::SidePanel::left("nav").resizable(false).exact_width(170.0).show(ctx, |ui| {
            ui.add_space(8.0);
            ui.heading("CF 隧道管理器");
            ui.add_space(10.0);
            for (tab, label) in [
                (Tab::Status, "状态"),
                (Tab::Tunnels, "隧道"),
                (Tab::Config, "配置"),
                (Tab::Commands, "命令"),
                (Tab::Logs, "日志"),
            ] {
                let sel = self.tab == tab;
                ui.add_sized([150.0, 28.0], egui::Button::new(label).selected(sel)).clicked().then(|| self.tab = tab);
            }
            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                ui.add_space(8.0);
                self.ui_update_panel(ui);
                ui.add_space(6.0);
                ui.weak(concat!("v", env!("CARGO_PKG_VERSION")));
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            // Transient action feedback, visible on every page.
            if let Some((msg, at)) = &self.toast_msg {
                if at.elapsed() < std::time::Duration::from_secs(6) {
                    ui.horizontal(|ui| {
                        ui.colored_label(
                            egui::Color32::from_rgb(33, 150, 243),
                            format!("● {msg}"),
                        );
                    });
                    ui.separator();
                }
            }
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.add_space(6.0);
                match self.tab {
                    Tab::Status => self.ui_status(ui),
                    Tab::Tunnels => self.ui_tunnels(ui),
                    Tab::Config => self.ui_config(ui),
                    Tab::Commands => self.ui_commands(ui),
                    Tab::Logs => self.ui_logs(ui),
                }
            });
        });

        // Keep animating so queued jobs / log streaming repaint.
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }
}