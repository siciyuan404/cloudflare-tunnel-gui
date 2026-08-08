# Cloudflare Tunnel 管理器

一个使用 Rust + egui 编写的 Windows 桌面 GUI，用于管理本机的 Cloudflare Tunnel（`cloudflared`）服务与配置。

## 功能

- **状态页**：显示 `cloudflared` 二进制路径、版本、Windows 服务状态；可安装/卸载/启动/停止/重启服务。
- **隧道页**：`login` 认证、创建/删除隧道、运行隧道（后台并实时抓取日志）、`cleanup`。
- **配置页**：可视化编辑 `~/.cloudflared/config.yml`（tunnel、protocol、ingress 规则），自动探测凭据文件。
- **日志页**：命令执行日志 + 隧道运行实时日志。

## 环境要求

- Windows
- 已安装 `cloudflared`（可执行文件在 PATH 中，或用环境变量 `CLOUDFLARED_PATH` 指定路径）
- 安装/卸载服务需要管理员权限

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