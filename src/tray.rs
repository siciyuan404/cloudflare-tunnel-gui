//! System tray icon for "close to tray" behaviour.
//!
//! Creates a tray icon in the notification area (bottom-right of the taskbar)
//! with a menu to restore the window or quit the app for real. The main window
//! itself is hidden instead of closed when the user clicks the X button.
//!
//! Tray events are consumed on a dedicated background thread: when the window
//! is hidden eframe stops driving `update()` (no repaints), so event polling
//! must not live in the UI frame loop.

use anyhow::Result;
use eframe::egui;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// Tray menu item ids.
pub const MENU_SHOW: &str = "show-main-window";
pub const MENU_QUIT: &str = "quit-app";

/// Keeps the tray icon alive; `_icon` must not be dropped while the app runs.
pub struct Tray {
    _icon: TrayIcon,
}

impl Tray {
    /// Build the tray icon with a small menu. Callers should log and continue
    /// on error so a tray failure never blocks the app from starting.
    pub fn new() -> Result<Tray> {
        let show = MenuItem::with_id(
            MenuId::new(MENU_SHOW),
            "显示主窗口",
            true,
            None,
        );
        let quit = MenuItem::with_id(
            MenuId::new(MENU_QUIT),
            "退出",
            true,
            None,
        );
        let menu = Menu::new();
        menu.append_items(&[&show, &quit])?;

        let icon = Tray::make_icon();
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Cloudflare Tunnel 管理器")
            .with_icon(icon)
            .build()?;
        Ok(Tray { _icon: tray })
    }

    /// Spawn a background listener for tray events.
    ///
    /// - left click on the icon or "显示主窗口" → make the window visible again;
    /// - "退出" → set `allow_exit` and close for real.
    ///
    /// While the window is hidden, eframe stops driving `update()` and no
    /// repaint/viewport commands are processed, so restore/quit are performed
    /// with direct Win32 calls (`ShowWindow` / `PostMessage(WM_CLOSE)`) which
    /// the message loop always delivers.
    pub fn spawn_listener(&self, ctx: egui::Context, allow_exit: Arc<AtomicBool>) {
        let _ = ctx; // reserved: may be used for future non-Windows paths
        let menu_rx = MenuEvent::receiver();
        let tray_rx = TrayIconEvent::receiver();
        std::thread::spawn(move || loop {
            while let Ok(event) = menu_rx.try_recv() {
                match event.id().as_ref() {
                    MENU_SHOW => show_main_window(),
                    MENU_QUIT => {
                        allow_exit.store(true, Ordering::Relaxed);
                        quit_main_window();
                    }
                    _ => {}
                }
            }
            while let Ok(event) = tray_rx.try_recv() {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    ..
                } = event
                {
                    show_main_window();
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        });
    }

    /// Draw a simple 32x32 icon: orange rounded square (Cloudflare brand) with
    /// a white ring in the middle, no external asset needed.
    fn make_icon() -> Icon {
        const SIZE: usize = 32;
        const CORNER: f32 = 6.0;
        let mut rgba = vec![0u8; SIZE * SIZE * 4];

        let color_outer = (246, 130, 31); // Cloudflare orange
        let color_inner = (255, 255, 255);

        for y in 0..SIZE {
            for x in 0..SIZE {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;

                // Distance to the rounded-rectangle border.
                let dx = (px - SIZE as f32 / 2.0).abs().max(0.0);
                let dy = (py - SIZE as f32 / 2.0).abs().max(0.0);
                let half = SIZE as f32 / 2.0;
                let ox = (dx - (half - CORNER)).max(0.0);
                let oy = (dy - (half - CORNER)).max(0.0);
                let dist = (ox * ox + oy * oy).sqrt();

                if dist > CORNER {
                    continue; // transparent outside the rounded square
                }

                let idx = (y * SIZE + x) * 4;
                let mut rgb = color_outer;

                // White ring in the centre.
                let c = (px - SIZE as f32 / 2.0).powi(2) + (py - SIZE as f32 / 2.0).powi(2);
                let r_outer = 8.5;
                let r_inner = 4.5;
                if c <= r_outer * r_outer {
                    if c >= r_inner * r_inner {
                        rgb = color_inner;
                    } else {
                        rgb = color_outer; // re-fill the hole with orange
                    }
                }

                rgba[idx] = rgb.0;
                rgba[idx + 1] = rgb.1;
                rgba[idx + 2] = rgb.2;
                rgba[idx + 3] = 255;
            }
        }

        Icon::from_rgba(rgba, SIZE as u32, SIZE as u32).expect("invalid tray icon buffer")
    }
}

/// Find the main application window (winit creates it with class "Window Class").
fn main_window_hwnd() -> Option<isize> {
    use std::sync::atomic::AtomicUsize;
    use windows_sys::Win32::Foundation::{BOOL, FALSE, HWND, LPARAM, TRUE};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetWindowThreadProcessId,
    };

    static FOUND: AtomicUsize = AtomicUsize::new(0);
    FOUND.store(0, std::sync::atomic::Ordering::Relaxed);

    let found_ref = &FOUND;

    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let found_ref = &*(lparam as *const std::sync::atomic::AtomicUsize);
        let mut win_pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut win_pid);
        if win_pid == std::process::id() {
            let mut buf = [0u16; 64];
            let len = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
            if len > 0 && String::from_utf16_lossy(&buf[..len as usize]) == "Window Class" {
                found_ref.store(hwnd as usize, std::sync::atomic::Ordering::Relaxed);
                return FALSE; // stop enumerating
            }
        }
        TRUE
    }

    unsafe {
        EnumWindows(
            Some(callback),
            found_ref as *const _ as LPARAM,
        );
    }
    let hwnd = FOUND.load(std::sync::atomic::Ordering::Relaxed);
    if hwnd == 0 {
        None
    } else {
        Some(hwnd as isize)
    }
}

/// Show and focus the main window with a direct Win32 call. This works even
/// when the window is hidden and eframe is not driving `update()`.
fn show_main_window() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetForegroundWindow, ShowWindow, SW_SHOW,
    };
    if let Some(hwnd) = main_window_hwnd() {
        unsafe {
            ShowWindow(hwnd as _, SW_SHOW);
            SetForegroundWindow(hwnd as _);
        }
    }
}

/// Ask the main window to close. `WM_CLOSE` is delivered by the message loop
/// even while the window is hidden; eframe then runs one final `update()` where
/// `allow_exit` (already set by the caller) lets the close go through.
fn quit_main_window() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
    if let Some(hwnd) = main_window_hwnd() {
        unsafe {
            PostMessageW(hwnd as _, WM_CLOSE, 0, 0);
        }
    }
}
