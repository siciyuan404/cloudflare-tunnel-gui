# Cloudflare Tunnel 管理器

一个使用 Rust + egui 编写的 Windows 桌面 GUI，用于管理本机的 Cloudflare Tunnel（`cloudflared`）服务与配置。

## 功能

- **状态页**：显示 `cloudflared` 二进制路径、版本、Windows 服务状态、隧道数量、配置与凭据路径；可安装/卸载/启动/停止/重启服务；一键校验配置（`ingress validate`）、查看选中隧道连接。
- **隧道页**：`login` 认证、创建/删除隧道、运行隧道（后台并实时抓取日志）、`cleanup`、`route dns`（域名绑定到隧道，穿透生效必需）、生成 `token`。
- **配置页**：可视化编辑整机全局的 `C:\ProgramData\cloudflared\config.yml`（tunnel、protocol、ingress 规则，支持 hostname/path/service 与规则排序），自动探测凭据文件并复制到全局目录供 SYSTEM 服务读取；保存采用合并模式，保留手写的未知字段（如 `originRequest`、`warp-routing`）；另提供**原始 YAML 编辑区**，可完整接管全部配置字段并做语法校验。旧的 `%USERPROFILE%\.cloudflared` 配置会被读取，保存时迁移到全局目录。
- **命令页**：通用命令终端，可执行任意 `cloudflared <参数>`（如 `tunnel ingress rule <url>`、`cloudflared update`），并内置常用快捷命令；支持 `tunnel run --token` 后台运行。
- **日志页**：命令执行日志 + 隧道运行实时日志。

## 环境要求

- Windows
- 已安装 `cloudflared`（可执行文件在 PATH 中，或用环境变量 `CLOUDFLARED_PATH` 指定路径）
- **建议以管理员身份运行**：整机全局模式把配置/凭据写入 `C:\ProgramData\cloudflared`，并管理以 SYSTEM 运行的 `Cloudflared` 服务。非管理员时界面提供「以管理员身份重启」按钮。
- 配置与凭据存放于 `C:\ProgramData\cloudflared`，服务以 `--config` 指向它，因此整机所有账户与开机自启的隧道路由共享同一份配置。

## 构建与运行

```bash
cargo run
```

发布版构建：

```bash
cargo build --release
```

二进制位于 `target/release/cloudflare-tunnel-manager.exe`。

## 项目结构

```
src/
  main.rs        # 入口，创建 eframe 窗口
  app.rs         # egui GUI 界面与状态
  cloudflared.rs # cloudflared / sc.exe 命令封装
  config.rs      # config.yml 读写与隧道列表解析
  tasks.rs       # 后台任务队列（避免阻塞 UI）
```