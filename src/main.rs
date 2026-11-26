#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cloudflared;
mod config;
mod tasks;
mod tray;
mod updater;

use app::ManagerApp;
use eframe::egui;
use std::sync::Arc;

/// egui 默认字体没有中文字形，加载 Windows 系统中的中文 TTF 作为回退字体。
fn setup_cjk_fonts(ctx: &egui::Context) {
    use eframe::egui::{FontData, FontDefinitions, FontFamily};

    const CANDIDATES: &[&str] = &[
        "C:\\Windows\\Fonts\\Deng.ttf",
        "C:\\Windows\\Fonts\\simhei.ttf",
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\simsun.ttc",
        "/System/Library/Fonts/PingFang.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    ];

    let data = CANDIDATES
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .and_then(|p| std::fs::read(p).ok());

    if let Some(bytes) = data {
        let mut fonts = FontDefinitions::default();
        fonts
            .font_data
            .insert("cjk".to_owned(), FontData::from_owned(bytes));
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            if let Some(list) = fonts.families.get_mut(&family) {
                list.insert(0, "cjk".to_owned());
            }
        }
        ctx.set_fonts(fonts);
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 620.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("Cloudflare Tunnel 管理器"),
        ..Default::default()
    };

    // Tray icon lives for the whole run; a failure here only disables
    // hide-to-tray, the app still starts normally.
    let tray = match tray::Tray::new() {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("tray icon unavailable: {e}");
            None
        }
    };

    // Set to true only by the tray "退出" menu or the self-updater, so a plain
    // window-close (X) hides to the tray instead of quitting.
    let allow_exit = Arc::new(std::sync::atomic::AtomicBool::new(false));

    eframe::run_native(
        "Cloudflare Tunnel Manager",
        options,
        Box::new(|cc| {
            setup_cjk_fonts(&cc.egui_ctx);
            if let Some(t) = &tray {
                t.spawn_listener(cc.egui_ctx.clone(), allow_exit.clone());
            }
            Ok(Box::new(ManagerApp::new(allow_exit)))
        }),
    )
}