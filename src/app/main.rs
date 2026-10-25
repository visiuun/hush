#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! `hush-app` — the elevated tray/GUI daemon that owns the fan controller.

mod graph;
mod icon_draw;
mod tray;
mod ui;
mod widgets;

use eframe::egui;
use hushcore::engine::{Cmd, Engine};
use hushcore::ipc::{self, Hooks, Request};
use hushcore::winutil;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tray_icon::menu::MenuEvent;
use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |f: &str| args.iter().any(|a| a == f);
    let tray_only = has("--tray");

    // `hush startup on|off` from a non-elevated shell lands here elevated.
    if has("--startup-on") || has("--startup-off") {
        if !winutil::is_elevated() {
            winutil::run_elevated(&std::env::current_exe().unwrap_or_default(), &args.join(" "));
            return;
        }
        let _ = winutil::set_startup(has("--startup-on"));
        return;
    }

    // Already running → just surface it.
    if ipc::call(&Request::Ping).is_ok() {
        if !tray_only {
            let _ = ipc::call(&Request::Show { page: None });
        }
        return;
    }

    // Fan control needs admin. Prefer the logon task (no UAC prompt), else ask UAC.
    if !winutil::is_elevated() {
        if winutil::start_via_task() {
            if !tray_only {
                let t0 = Instant::now();
                while t0.elapsed() < Duration::from_secs(10) {
                    if ipc::call(&Request::Show { page: None }).is_ok() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(150));
                }
            }
        } else {
            winutil::run_elevated(&std::env::current_exe().unwrap_or_default(), &args.join(" "));
        }
        return;
    }

    let Some(_instance) = winutil::SingleInstance::acquire() else { return };

    // First launch: get the driver and register the logon task, so there is nothing to set up.
    // (The installer asks about the startup task itself.)
    let installed = winutil::app_exe().with_file_name("unins000.exe").exists();
    let first_run = !hushcore::config::path().exists() && !installed;
    if !hushcore::hw::pawnio::installed() {
        match hushcore::hw::pawnio::install() {
            Ok(()) => hushcore::log("installed the PawnIO driver"),
            Err(e) => hushcore::log(format!("PawnIO install failed: {e}")),
        }
    }
    if first_run && !winutil::startup_enabled() {
        if let Err(e) = winutil::set_startup(true) {
            hushcore::log(format!("could not register the startup task: {e}"));
        }
    }

    let sh = Engine::start();

    // Whatever happens, try to give the fans back to the BIOS before dying.
    {
        let sh = sh.clone();
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            sh.send(Cmd::Quit);
            let t0 = Instant::now();
            while !sh.stopped.load(Ordering::Acquire) && t0.elapsed() < Duration::from_millis(1500) {
                std::thread::sleep(Duration::from_millis(10));
            }
            default(info);
        }));
    }

    let start_visible = !(tray_only || sh.config.read().settings.start_minimized);
    let (tx, rx) = std::sync::mpsc::channel::<ui::GuiCmd>();
    let ctx_cell: Arc<OnceLock<egui::Context>> = Arc::new(OnceLock::new());
    let visible = Arc::new(AtomicBool::new(start_visible));
    let hwnd = Arc::new(AtomicIsize::new(0));
    let repaint = {
        let c = ctx_cell.clone();
        move || {
            if let Some(ctx) = c.get() {
                ctx.request_repaint();
            }
        }
    };

    // External control.
    {
        let (tx1, tx2, r1, r2, h) = (std::sync::Mutex::new(tx.clone()), std::sync::Mutex::new(tx.clone()), repaint.clone(), repaint.clone(), hwnd.clone());
        ipc::serve(
            sh.clone(),
            Hooks {
                show: Box::new(move |page| {
                    let _ = tx1.lock().unwrap().send(ui::GuiCmd::Show(page));
                    let w = h.load(Ordering::Acquire);
                    if w != 0 {
                        winutil::set_cloaked(w, false);
                        winutil::show_window(w, true);
                    }
                    r1();
                }),
                quit: Box::new(move || {
                    let _ = tx2.lock().unwrap().send(ui::GuiCmd::Quit);
                    r2();
                }),
            },
        );
    }

    // Repaint on every engine tick while visible; every few ticks while hidden (tray tooltip).
    {
        let (v, r) = (visible.clone(), repaint.clone());
        let n = AtomicU32::new(0);
        sh.set_on_update(move || {
            let k = n.fetch_add(1, Ordering::Relaxed);
            if v.load(Ordering::Relaxed) || k.is_multiple_of(5) {
                r();
            }
        });
    }

    let icon = egui::IconData { rgba: icon_draw::logo_rgba(64), width: 64, height: 64 };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Hush")
            .with_inner_size([1240.0, 820.0])
            .with_min_inner_size([980.0, 640.0])
            .with_icon(Arc::new(icon))
            .with_visible(start_visible),
        ..Default::default()
    };
    let sh2 = sh.clone();
    let _ = eframe::run_native(
        "Hush",
        options,
        Box::new(move |cc| {
            let _ = ctx_cell.set(cc.egui_ctx.clone());
            {
                let (tx, c) = (tx.clone(), cc.egui_ctx.clone());
                MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                    let _ = tx.send(ui::GuiCmd::Menu(e.id));
                    c.request_repaint();
                }));
            }
            {
                let (tx, c, h) = (tx.clone(), cc.egui_ctx.clone(), hwnd.clone());
                TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
                    let open = matches!(
                        e,
                        TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                            | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }
                    );
                    if open {
                        let _ = tx.send(ui::GuiCmd::TrayClick);
                        let w = h.load(Ordering::Acquire);
                        if w != 0 {
                            winutil::set_cloaked(w, false);
                            winutil::show_window(w, true);
                        }
                        c.request_repaint();
                    }
                }));
            }
            Ok(Box::new(ui::App::new(cc, sh, rx, visible, hwnd, start_visible)))
        }),
    );
    // Window closed: make sure the engine has restored BIOS control.
    sh2.send(Cmd::Quit);
    let t0 = Instant::now();
    while !sh2.stopped.load(Ordering::Acquire) && t0.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(10));
    }
}
