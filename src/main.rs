#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cloudflared;
mod config;
mod tasks;

use app::ManagerApp;
use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 620.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("Cloudflare Tunnel 管理器"),
        ..Default::default()
    };
    eframe::run_native(
        "Cloudflare Tunnel Manager",
        options,
        Box::new(|_cc| Ok(Box::new(ManagerApp::new()))),
    )
}