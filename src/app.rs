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
    Logs,
}

pub struct ManagerApp {
    runner: Runner,
    binary: Option<PathBuf>,

    cloudflared_version: String,
    service_status: String,

    tunnels: Vec<crate::config::TunnelEntry>,
    selected_tunnel: Option<String>,

    config: TunnelConfig,
    config_error: Option<String>,

    logs: Vec<LogLine>,
    pending: HashMap<u64, String>,

    // Streaming tunnel-run viewer.
    run_log: Arc<Mutex<Vec<String>>>,
    run_child: Option<Child>,
    run_in_progress: bool,

    // Form scratch.
    create_name: String,
    ingress_hostname: String,
    ingress_service: String,

    // Silenced self-update (GitHub Releases).
    update: Arc<Mutex<UpdateState>>,
    close_requested: Arc<AtomicBool>,

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
        Self::new()
    }
}

impl ManagerApp {
    pub fn new() -> Self {
        let config = TunnelConfig::load().unwrap_or_default();
        let mut app = Self {
            runner: Runner::new(),
            binary: cloudflared::find_binary().ok(),
            cloudflared_version: String::new(),
            service_status: String::new(),
            tunnels: Vec::new(),
            selected_tunnel: None,
            config,
            config_error: None,
            logs: Vec::new(),
            pending: HashMap::new(),
            run_log: Arc::new(Mutex::new(Vec::new())),
            run_child: None,
            run_in_progress: false,
            create_name: String::new(),
            ingress_hostname: String::new(),
            ingress_service: "http://localhost:8080".into(),
            update: Arc::new(Mutex::new(UpdateState::Checking)),
            close_requested: Arc::new(AtomicBool::new(false)),
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
        let id = self.runner.spawn(label, Box::new(move || f(bin)));
        self.pending.insert(id, lbl);
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
            self.log(&tag, &d.result.output_text());
        }
    }

    fn open_tunnel_run(&mut self, tunnel: &str) {
        self.log("info", &format!("启动 `cloudflared tunnel run {tunnel}`"));
        self.run_log.lock().unwrap().clear();
        self.run_in_progress = true;
        let buf = self.run_log.clone();
        let bin = match self.binary.clone() {
            Some(b) => b,
            None => return,
        };
        let mut cmd = std::process::Command::new(bin);
        cmd.arg("tunnel").arg("run").arg(tunnel);
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
                self.log("error", &format!("无法启动隧道: {e}"));
                self.run_in_progress = false;
            }
        }
    }

    fn stop_tunnel_run(&mut self) {
        if let Some(mut child) = self.run_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.run_in_progress = false;
        self.log("info", "隧道已停止");
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
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.button("刷新状态").clicked() {
                self.refresh_status();
            }
            if ui.button("安装服务").on_hover_text("需要管理员权限").clicked() {
                self.spawn("service install", |b| cloudflared::service_install(&b));
            }
            if ui.button("卸载服务").clicked() {
                self.spawn("service uninstall", |b| cloudflared::service_uninstall(&b));
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("启动服务").clicked() {
                let r = cloudflared::sc("start");
                self.log("service start", &r.output_text());
                self.refresh_status();
            }
            if ui.button("停止服务").clicked() {
                let r = cloudflared::sc("stop");
                self.log("service stop", &r.output_text());
                self.refresh_status();
            }
            if ui.button("重启服务").clicked() {
                let _ = cloudflared::sc("stop");
                let r = cloudflared::sc("start");
                self.log("service restart", &r.output_text());
                self.refresh_status();
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
            if ui.add_enabled(has_sel, egui::Button::new("写入 config tunnel")).clicked() {
                self.config.tunnel = Some(on.clone());
                match self.config.save() {
                    Ok(_) => self.log("config", &format!("tunnel 设为 {on}")),
                    Err(e) => self.log("config error", &e.to_string()),
                }
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
        ui.heading("配置 config.yml");
        ui.add_space(4.0);
        ui.weak(format!("路径: {}", crate::config::config_path().display()));
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
        ui.weak("凭据文件自动探测于 ~/.cloudflared 目录，保存配置时若未指定将自动填充。");

        ui.add_space(8.0);
        ui.strong("Ingress 规则");
        ui.horizontal(|ui| {
            ui.label("主机名:");
            ui.text_edit_singleline(&mut self.ingress_hostname);
            ui.label("服务:");
            ui.text_edit_singleline(&mut self.ingress_service);
            let valid = !self.ingress_service.trim().is_empty();
            if ui.add_enabled(valid, egui::Button::new("添加")).clicked() {
                let svc = self.ingress_service.trim().to_string();
                let mut rule = crate::config::Ingress { hostname: None, path: None, service: svc };
                if !self.ingress_hostname.trim().is_empty() {
                    rule.hostname = Some(self.ingress_hostname.trim().to_string());
                }
                self.config.ingress.push(rule);
                self.ingress_service = "http://localhost:8080".into();
                self.ingress_hostname.clear();
            }
        });
        let mut remove: Option<usize> = None;
        let mut ingress = self.config.ingress.clone();
        for (i, r) in ingress.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("{}", i + 1));
                ui.monospace(
                    r.hostname
                        .clone()
                        .map(|h| format!("{h}\u{2192}{}", r.service))
                        .unwrap_or_else(|| format!("(fallback) {}{}", r.service, r.path.clone().unwrap_or_default())),
                );
                if ui.button("删除").clicked() {
                    remove = Some(i);
                }
            });
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
                match self.config.save() {
                    Ok(_) => {
                        self.config_error = None;
                        self.log("config", "config.yml 已保存成功");
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
    }

    fn ui_logs(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("命令日志");
            if ui.button("清空命令日志").clicked() {
                self.logs.clear();
            }
        });
        egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).max_height(220.0).show(ui, |ui| {
            for l in self.logs.iter().take(400) {
                ui.horizontal_wrapped(|ui| {
                    ui.weak(&l.time);
                    ui.weak(format!("[{}]", l.tag));
                    ui.monospace(&l.text);
                });
            }
            if self.logs.is_empty() {
                ui.weak("暂无命令日志。");
            }
        });
        ui.add_space(10.0);
        ui.separator();
        ui.horizontal(|ui| {
            ui.heading("隧道运行日志");
            if self.run_in_progress {
                ui.spinner();
                if ui.button("停止隧道").clicked() {
                    self.stop_tunnel_run();
                }
            }
        });
        egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            let lines = self.run_log.lock().unwrap().clone();
            for l in lines.iter() {
                ui.monospace(l);
            }
            if lines.is_empty() {
                ui.weak("（隧道未运行或无输出）");
            }
        });
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
            let _ = ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        egui::SidePanel::left("nav").resizable(false).exact_width(170.0).show(ctx, |ui| {
            ui.add_space(8.0);
            ui.heading("CF 隧道管理器");
            ui.add_space(10.0);
            for (tab, label) in [(Tab::Status, "状态"), (Tab::Tunnels, "隧道"), (Tab::Config, "配置"), (Tab::Logs, "日志")] {
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
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.add_space(6.0);
                match self.tab {
                    Tab::Status => self.ui_status(ui),
                    Tab::Tunnels => self.ui_tunnels(ui),
                    Tab::Config => self.ui_config(ui),
                    Tab::Logs => self.ui_logs(ui),
                }
            });
        });

        // Keep animating so queued jobs / log streaming repaint.
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }
}