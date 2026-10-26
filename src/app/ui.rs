//! The Hush window: Overview · Fans · Automation · Settings.

use super::graph::{self, Live, Series};
use super::tray::{Action, Tray};
use super::widgets::*;
use eframe::egui::{self, Align, Align2, Color32, CornerRadius, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui, Vec2, ViewportCommand, pos2, vec2};
use hushcore::config::{Config, Curve, FanMode, Mix, Preset, Profile, Rule, Trigger};
use hushcore::engine::{Cmd, Shared, Snapshot};
use hushcore::winutil;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use tray_icon::menu::MenuId;

pub enum GuiCmd {
    Show(Option<String>),
    Menu(MenuId),
    TrayClick,
    Quit,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
    Overview,
    Fans,
    Automation,
    Settings,
}

impl Page {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_lowercase().as_str() {
            "overview" | "home" | "1" => Self::Overview,
            "fans" | "curves" | "2" => Self::Fans,
            "automation" | "rules" | "3" => Self::Automation,
            "settings" | "4" => Self::Settings,
            _ => return None,
        })
    }
}

pub struct App {
    sh: Arc<Shared>,
    rx: Receiver<GuiCmd>,
    tray: Option<Tray>,
    visible: Arc<AtomicBool>,
    hwnd: Arc<AtomicIsize>,
    page: Page,
    allow_close: bool,
    pending_hide: u8,
    sel_fan: Option<String>,
    edit_profile: Option<String>,
    show_hidden: bool,
    advanced: bool,
    range_s: f64,
    fans_rpm: bool,
    startup: bool,
    stash: HashMap<(String, String), Curve>,
    proc_buf: HashMap<usize, String>,
    running_apps: Vec<String>,
    new_profile: String,
    toast: Option<(String, Instant)>,
    seen_alerts: Vec<String>,
    last_tray_sync: Instant,
    /// Easter egg: the logo is a real fan (angle rad, angular velocity rad/s).
    spin: (f32, f32),
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, sh: Arc<Shared>, rx: Receiver<GuiCmd>, visible: Arc<AtomicBool>, hwnd: Arc<AtomicIsize>, start_visible: bool) -> Self {
        let ctx = cc.egui_ctx.clone();
        install_fonts(&ctx);
        install_style(&ctx);
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(h) = cc.window_handle() {
                if let RawWindowHandle::Win32(w) = h.as_raw() {
                    hwnd.store(w.hwnd.get(), Ordering::Release);
                    winutil::style_window(w.hwnd.get());
                    if !start_visible {
                        winutil::set_cloaked(w.hwnd.get(), true);
                    }
                }
            }
        }
        visible.store(start_visible, Ordering::Release);
        let names: Vec<String> = sh.config.read().profiles.iter().map(|p| p.name.clone()).collect();
        Self {
            sh,
            rx,
            tray: Tray::build(&names),
            visible,
            hwnd,
            page: Page::Overview,
            allow_close: false,
            pending_hide: if start_visible { 0 } else { 3 },
            sel_fan: None,
            edit_profile: None,
            show_hidden: false,
            advanced: false,
            range_s: 300.0,
            fans_rpm: true,
            startup: winutil::startup_enabled(),
            stash: HashMap::new(),
            proc_buf: HashMap::new(),
            running_apps: Vec::new(),
            new_profile: String::new(),
            toast: None,
            seen_alerts: Vec::new(),
            last_tray_sync: Instant::now() - Duration::from_secs(10),
            spin: (0.0, 0.0),
        }
    }

    fn show(&mut self, ctx: &egui::Context) {
        self.pending_hide = 0;
        let h = self.hwnd.load(Ordering::Acquire);
        if h != 0 {
            winutil::set_cloaked(h, false);
        }
        self.visible.store(true, Ordering::Release);
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        if h != 0 {
            winutil::show_window(h, true);
        }
    }

    fn hide(&mut self, ctx: &egui::Context) {
        self.visible.store(false, Ordering::Release);
        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
    }

    fn quit(&mut self, ctx: &egui::Context) {
        self.sh.send(Cmd::Quit);
        self.sh.quit.store(true, Ordering::Release);
        // Give the engine a moment to hand fans back to the BIOS.
        let t0 = Instant::now();
        while !self.sh.stopped.load(Ordering::Acquire) && t0.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.allow_close = true;
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(2));
            std::process::exit(0);
        });
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    fn toast(&mut self, s: impl Into<String>) {
        self.toast = Some((s.into(), Instant::now()));
    }

    fn handle_tray(&mut self, ctx: &egui::Context, id: MenuId) {
        let Some(action) = self.tray.as_ref().and_then(|t| t.action(&id)) else { return };
        let snap = self.sh.snapshot();
        match action {
            Action::Open => self.show(ctx),
            Action::Profile(p) => self.sh.send(Cmd::SetProfile(p)),
            Action::Boost => self.sh.send(Cmd::Boost(if snap.boost_left.is_some() { None } else { Some(60.0) })),
            Action::Pause => self.sh.send(Cmd::Pause(!snap.paused)),
            Action::Exit => self.quit(ctx),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Chrome
    // -----------------------------------------------------------------------------------------

    fn nav(&mut self, ui: &mut Ui) {
        ui.add_space(14.0);
        let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
        // Each click is a flick: velocity stacks (capped), then air drag + bearing friction
        // spin it down. Spam-click to wind it up.
        let (angle, vel) = &mut self.spin;
        if resp.clicked() || resp.double_clicked() {
            *vel = (*vel + 16.0).min(110.0);
        }
        let dt = ui.input(|i| i.stable_dt).min(0.05);
        if *vel > 0.0 {
            *angle = (*angle + *vel * dt).rem_euclid(std::f32::consts::TAU);
            *vel = (*vel - (0.035 * *vel * *vel + 1.2) * dt).max(0.0);
            ui.ctx().request_repaint();
        }
        logo_mark(ui, r.center(), 15.0, *angle, *vel);
        ui.add_space(14.0);
        for (p, ic, tip) in [
            (Page::Overview, I_HOME, "Overview  (1)"),
            (Page::Fans, I_FAN, "Fans & curves  (2)"),
            (Page::Automation, I_FLOW, "Automation  (3)"),
            (Page::Settings, I_SETTINGS, "Settings  (4)"),
        ] {
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::click());
            let sel = self.page == p;
            let t = ui.ctx().animate_bool_responsive(resp.id, sel);
            let pill = Rect::from_center_size(rect.center(), vec2(40.0, 40.0));
            if sel || resp.hovered() {
                ui.painter().rect_filled(pill, CornerRadius::same(10), if sel { SEL } else { WELL });
            }
            if t > 0.0 {
                let bar = Rect::from_min_size(pos2(rect.left(), rect.center().y - 9.0 * t), vec2(3.0, 18.0 * t));
                ui.painter().rect_filled(bar, CornerRadius::same(2), ACCENT);
            }
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, ic, icon(17.0), lerp_color(DIM, TEXT, t.max(if resp.hovered() { 0.6 } else { 0.0 })));
            if resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                self.page = p;
            }
        }
    }

    fn top_bar(&mut self, ui: &mut Ui, s: &Snapshot, cfg: &Config) {
        ui.horizontal(|ui| {
            let title = match self.page {
                Page::Overview => "Overview",
                Page::Fans => "Fans & curves",
                Page::Automation => "Automation",
                Page::Settings => "Settings",
            };
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new(title).font(semi(20.0)).color(TEXT));
                let sub = match &s.chip {
                    Some(c) => format!("{c} · profile {} · {}", s.profile, s.profile_reason.to_lowercase()),
                    None => "No fan controller detected".into(),
                };
                ui.label(RichText::new(sub).font(reg(11.5)).color(DIM));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // Pause
                let (ic, tip) = if s.paused { (I_PLAY, "Resume Hush control") } else { (I_PAUSE, "Pause — hand every fan to the BIOS") };
                if button(ui, ic, "", if s.paused { Kind::Primary } else { Kind::Secondary }, Some(32.0), 32.0).on_hover_text(tip).clicked() {
                    self.sh.send(Cmd::Pause(!s.paused));
                }
                // Boost
                let blabel = s.boost_left.map(|b| format!("{b:.0}s")).unwrap_or_else(|| "Boost".into());
                if button(ui, I_BOLT, &blabel, if s.boost_left.is_some() { Kind::Primary } else { Kind::Secondary }, None, 32.0)
                    .on_hover_text("All fans to 100 % for 60 s (click again to cancel)")
                    .clicked()
                {
                    self.sh.send(Cmd::Boost(if s.boost_left.is_some() { None } else { Some(60.0) }));
                }
                ui.add_space(4.0);
                // Quick profile switcher.
                let mut opts: Vec<(String, &str)> = vec![("Auto".to_string(), "Auto")];
                for p in &cfg.profiles {
                    opts.push((p.name.clone(), p.name.as_str()));
                }
                let mut cur = s.pinned_profile.clone().unwrap_or_else(|| "Auto".into());
                let w = (opts.len() as f32 * 86.0).min(460.0);
                if segmented(ui, "quick-profile", &mut cur, &opts, w) {
                    self.sh.send(Cmd::SetProfile((cur != "Auto").then_some(cur)));
                }
                ui.add_space(4.0);
                let (dot, txt, pulse) = if s.critical {
                    (RED, "Critical".to_string(), true)
                } else if s.paused {
                    (AMBER, "Paused".to_string(), false)
                } else if s.chip.is_none() {
                    (RED, "No controller".to_string(), false)
                } else if s.calibration.is_some() {
                    (VIOLET, "Calibrating".to_string(), true)
                } else {
                    (GREEN, "Live".to_string(), false)
                };
                chip(ui, dot, &txt, pulse);
            });
        });
    }

    // -----------------------------------------------------------------------------------------
    // Overview
    // -----------------------------------------------------------------------------------------

    fn overview(&mut self, ui: &mut Ui, s: &Snapshot, cfg: &Config) {
        if let Some((_, _, e)) = s.backends.iter().find(|(n, ok, _)| n == "Fans" && !ok) {
            banner(ui, RED, &format!("Fan controller unavailable: {e}"));
        }
        let hist = self.sh.history.read();
        let w = ui.available_width();
        let gap = 12.0;
        let tw = (w - gap * 2.0) / 3.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            let cpu = s.sensor("cpu");
            let ccd = s.sensors.iter().filter(|x| x.id.starts_with("ccd")).map(|x| x.value).fold(f32::MIN, f32::max);
            metric_tile(
                ui,
                tw,
                "CPU",
                cpu.map(|c| c.value),
                &[
                    s.cpu_load.map(|l| format!("{l:.0}% load")).unwrap_or_default(),
                    if ccd > 0.0 { format!("CCD {ccd:.0}°") } else { String::new() },
                ],
                hist.get("t:cpu"),
            );
            let gpu = s.sensor("gpu");
            metric_tile(
                ui,
                tw,
                gpu.map(|g| g.name.as_str()).unwrap_or("GPU"),
                gpu.map(|g| g.value),
                &[s.gpu_load.map(|l| format!("{l:.0}% load")).unwrap_or_default(), s.gpu_power.map(|p| format!("{p:.0} W")).unwrap_or_default()],
                hist.get("t:gpu"),
            );
            let board = ["mb.systin", "mb.system", "mb.temp1"]
                .iter()
                .find_map(|id| s.sensor(id))
                .or_else(|| s.sensors.iter().find(|x| x.group == "Motherboard" && !x.hidden));
            let socket = s.sensor("mb.cputin").or_else(|| s.sensor("mb.socket"));
            metric_tile(
                ui,
                tw,
                "Motherboard",
                board.map(|b| b.value),
                &[socket.map(|x| format!("Socket {:.0}°", x.value)).unwrap_or_default()],
                board.and_then(|b| hist.get(&format!("t:{}", b.id))),
            );
        });
        ui.add_space(4.0);

        // Fan cards.
        let fans: Vec<_> = s.fans.iter().filter(|f| !f.hidden).collect();
        if !fans.is_empty() {
            let cols = ((w + gap) / (220.0 + gap)).floor().max(1.0) as usize;
            let cw = (w - gap * (cols as f32 - 1.0)) / cols as f32;
            for chunk in fans.chunks(cols) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for f in chunk {
                        let calib = cfg.fans.get(&f.id).and_then(|i| i.calibration.as_ref());
                        if fan_card(ui, cw, f, calib, hist.get(&format!("rpm:{}", f.id))).clicked() {
                            self.sel_fan = Some(f.id.clone());
                            self.page = Page::Fans;
                        }
                    }
                });
            }
        } else if s.chip.is_some() {
            banner(ui, AMBER, "No fans are visible. Unhide headers in Fans & curves.");
        }
        ui.add_space(4.0);

        card(ui, "", |ui| {
            ui.horizontal(|ui| {
                section(ui, "Temperatures");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let mut r = self.range_s as i32;
                    if segmented(ui, "range", &mut r, &[(60, "1m"), (300, "5m"), (900, "15m"), (1800, "30m"), (3600, "60m")], 250.0) {
                        self.range_s = r as f64;
                    }
                });
            });
            let temps: Vec<Series> = s
                .sensors
                .iter()
                .filter(|x| !x.hidden && !x.id.starts_with("ccd"))
                .enumerate()
                .map(|(i, x)| Series { key: format!("t:{}", x.id), label: &x.name, color: SERIES[i % SERIES.len()] })
                .collect();
            legend(ui, &temps);
            graph::history_chart(ui, "temps", &hist, &temps, self.range_s, "°C", None, 190.0);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                section(ui, "Fans");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let mut m = self.fans_rpm;
                    if segmented(ui, "fan-metric", &mut m, &[(true, "RPM"), (false, "Duty")], 130.0) {
                        self.fans_rpm = m;
                    }
                });
            });
            let fs: Vec<Series> = fans
                .iter()
                .enumerate()
                .map(|(i, f)| Series {
                    key: format!("{}:{}", if self.fans_rpm { "rpm" } else { "duty" }, f.id),
                    label: &f.name,
                    color: SERIES[(i + 1) % SERIES.len()],
                })
                .collect();
            legend(ui, &fs);
            graph::history_chart(ui, "fans", &hist, &fs, self.range_s, if self.fans_rpm { "rpm" } else { "%" }, if self.fans_rpm { None } else { Some((0.0, 100.0)) }, 170.0);
        });
    }

    // -----------------------------------------------------------------------------------------
    // Fans & curves
    // -----------------------------------------------------------------------------------------

    fn fans_page(&mut self, ui: &mut Ui, s: &Snapshot, cfg: &Config) {
        if s.fans.is_empty() {
            banner(ui, AMBER, "No fan headers available.");
            return;
        }
        if self.sel_fan.as_ref().is_none_or(|id| s.fan(id).is_none()) {
            self.sel_fan = s.fans.iter().find(|f| !f.hidden).or(s.fans.first()).map(|f| f.id.clone());
        }
        let profile = self.edit_profile.clone().filter(|p| cfg.profile(p).is_some()).unwrap_or_else(|| s.profile.clone());
        let list_w = 230.0;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            ui.vertical(|ui| {
                ui.set_width(list_w);
                for f in s.fans.iter().filter(|f| self.show_hidden || !f.hidden) {
                    let sel = self.sel_fan.as_deref() == Some(&f.id);
                    let (rect, resp) = ui.allocate_exact_size(vec2(list_w, 54.0), Sense::click());
                    let hov = resp.hovered();
                    let fill = if sel { SEL } else if hov { WELL } else { CARD };
                    ui.painter().rect(rect, CornerRadius::same(10), fill, Stroke::new(1.0, if sel { BORDER_HI } else { BORDER }), StrokeKind::Inside);
                    let alpha = if f.hidden { 0.45 } else { 1.0 };
                    ui.painter().text(rect.min + vec2(14.0, 18.0), Align2::LEFT_CENTER, &f.name, semi(13.0), TEXT.gamma_multiply(alpha));
                    let mode = cfg.profile(&profile).and_then(|p| p.fans.get(&f.id)).map(|m| m.label()).unwrap_or("BIOS");
                    ui.painter().text(rect.min + vec2(14.0, 37.0), Align2::LEFT_CENTER, format!("{} · {}", f.id, mode), reg(11.0), DIM.gamma_multiply(alpha));
                    let rpm = f.rpm.map(|r| format!("{r:.0}")).unwrap_or_else(|| "—".into());
                    ui.painter().text(pos2(rect.right() - 14.0, rect.top() + 18.0), Align2::RIGHT_CENTER, rpm, mono(13.0), if f.stalled { RED } else { TEXT.gamma_multiply(alpha) });
                    ui.painter().text(pos2(rect.right() - 14.0, rect.top() + 37.0), Align2::RIGHT_CENTER, format!("{:.0}%", f.duty), mono(11.0), DIM);
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        self.sel_fan = Some(f.id.clone());
                    }
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    toggle(ui, &mut self.show_hidden);
                    ui.label(RichText::new("Show hidden headers").font(reg(12.0)).color(MUTED));
                });
                ui.add_space(8.0);
                if button(ui, I_RULER, "Calibrate all fans", Kind::Secondary, Some(list_w), 32.0)
                    .on_hover_text("Measures each visible fan's max RPM and the duty where it stalls / starts (~90 s per fan). Other fans keep running normally.")
                    .clicked()
                {
                    let ids = s.fans.iter().filter(|f| !f.hidden).map(|f| f.id.clone()).collect();
                    self.sh.send(Cmd::Calibrate(ids));
                }
            });
            ui.vertical(|ui| {
                let Some(fid) = self.sel_fan.clone() else { return };
                let Some(f) = s.fan(&fid).cloned() else { return };
                self.fan_editor(ui, s, cfg, &profile, &f);
            });
        });
    }

    fn fan_editor(&mut self, ui: &mut Ui, s: &Snapshot, cfg: &Config, profile: &str, f: &hushcore::engine::FanView) {
        let info = cfg.fans.get(&f.id).cloned().unwrap_or_default();
        let calib = info.calibration.clone();
        let mode = cfg.profile(profile).and_then(|p| p.fans.get(&f.id)).cloned().unwrap_or(FanMode::Bios);

        card(ui, "", |ui| {
            ui.horizontal(|ui| {
                let mut name = info.name.clone();
                let r = text_field(ui, &mut name, &f.id, 200.0);
                if r.changed() {
                    let id = f.id.clone();
                    self.sh.edit_config(|c| c.fans.entry(id).or_default().name = name);
                }
                ui.label(RichText::new(f.id.as_str()).font(mono(11.0)).color(FAINT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let mut vis = !info.hidden;
                    if toggle(ui, &mut vis).on_hover_text("Show this header in the overview, boost and critical override").changed() {
                        let id = f.id.clone();
                        self.sh.edit_config(|c| c.fans.entry(id).or_default().hidden = !vis);
                    }
                    ui.label(RichText::new("Visible").font(reg(12.0)).color(MUTED));
                    ui.add_space(10.0);
                    let calibrating = s.calibration.as_ref().is_some_and(|c| c.fan == f.id);
                    if calibrating {
                        if button(ui, "", "Cancel", Kind::Danger, None, 30.0).clicked() {
                            self.sh.send(Cmd::CancelCalibration);
                        }
                    } else if button(ui, I_RULER, "Calibrate", Kind::Secondary, None, 30.0).clicked() {
                        self.sh.send(Cmd::Calibrate(vec![f.id.clone()]));
                    }
                });
            });
            ui.add_space(6.0);
            // Live readout strip.
            ui.horizontal(|ui| {
                stat(ui, "Speed", f.rpm.map(|r| format!("{r:.0} rpm")).unwrap_or_else(|| "—".into()), if f.stalled { RED } else { TEXT });
                stat(ui, "Duty", format!("{:.0} %", f.duty), TEXT);
                stat(ui, "Target", f.target.map(|t| format!("{t:.0} %")).unwrap_or_else(|| "BIOS".into()), ACCENT);
                stat(ui, "Sees", f.source_temp.map(|t| format!("{t:.1} °C")).unwrap_or_else(|| "—".into()), f.source_temp.map(temp_color).unwrap_or(DIM));
                stat(ui, "State", f.mode.clone(), MUTED);
            });
            if let Some(c) = s.calibration.as_ref().filter(|c| c.fan == f.id) {
                ui.add_space(6.0);
                progress(ui, c.progress, &c.phase);
            }
            if f.overridden {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    tag(ui, "OVERRIDE", Color32::BLACK, AMBER);
                    ui.label(RichText::new("Set from the command line — the profile below is ignored until cleared.").font(reg(12.0)).color(MUTED));
                    if button(ui, "", "Clear", Kind::Ghost, None, 26.0).clicked() {
                        self.sh.send(Cmd::Override { fan: f.id.clone(), mode: None });
                    }
                });
            }
        });

        card(ui, "", |ui| {
            ui.horizontal(|ui| {
                section(ui, "Profile");
                ui.add_space(6.0);
                let mut p = profile.to_string();
                let opts: Vec<(String, &str)> = cfg.profiles.iter().map(|p| (p.name.clone(), p.name.as_str())).collect();
                if segmented(ui, "edit-profile", &mut p, &opts, (opts.len() as f32 * 104.0).min(420.0)) {
                    self.edit_profile = Some(p);
                }
                if profile == s.profile {
                    tag(ui, "ACTIVE", Color32::BLACK, ACCENT);
                } else {
                    ui.label(RichText::new(format!("editing · active is {}", s.profile)).font(reg(11.5)).color(DIM));
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                section(ui, "Mode");
                ui.add_space(14.0);
                let mut kind = mode.label().to_string();
                let opts = [("BIOS".to_string(), "BIOS"), ("Fixed".to_string(), "Fixed"), ("Curve".to_string(), "Smart curve")];
                if segmented(ui, ("mode", &f.id), &mut kind, &opts, 330.0) {
                    let key = (profile.to_string(), f.id.clone());
                    if let FanMode::Curve(c) = &mode {
                        self.stash.insert(key.clone(), c.clone());
                    }
                    let new = match kind.as_str() {
                        "BIOS" => FanMode::Bios,
                        "Fixed" => FanMode::Fixed { duty: f.duty.round().max(30.0) },
                        _ => FanMode::Curve(self.stash.get(&key).cloned().unwrap_or_default()),
                    };
                    self.set_mode(profile, &f.id, new);
                }
            });
            ui.add_space(8.0);
            match mode.clone() {
                FanMode::Bios => {
                    ui.label(RichText::new("The motherboard's own Smart Fan curve drives this header (Hush only monitors it).").font(reg(12.5)).color(MUTED));
                }
                FanMode::Fixed { mut duty } => {
                    if slider(ui, "Duty", "Constant speed", &mut duty, 0.0..=100.0, 1.0, 50.0, |v| format!("{v:.0} %")) {
                        self.set_mode(profile, &f.id, FanMode::Fixed { duty });
                    }
                    if let Some(c) = &calib {
                        if let Some(r) = c.rpm_at(duty) {
                            ui.label(RichText::new(format!("≈ {r:.0} rpm (from calibration)")).font(reg(11.5)).color(DIM));
                        }
                    }
                }
                FanMode::Curve(mut c) => {
                    let mut changed = false;
                    // Presets + copy.
                    ui.horizontal(|ui| {
                        for (p, n) in [(Preset::Silent, "Silent"), (Preset::Balanced, "Balanced"), (Preset::Performance, "Performance")] {
                            if button(ui, "", n, Kind::Ghost, None, 26.0).on_hover_text("Load preset points & tuning (keeps the sensor choice)").clicked() {
                                let src = c.source.clone();
                                c = Curve::preset(p);
                                c.source = src;
                                changed = true;
                            }
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if button(ui, I_COPY, "Apply to all fans", Kind::Ghost, None, 26.0)
                                .on_hover_text("Copy this curve to every visible fan in this profile")
                                .clicked()
                            {
                                let ids: Vec<String> = s.fans.iter().filter(|x| !x.hidden).map(|x| x.id.clone()).collect();
                                let (pn, cc) = (profile.to_string(), c.clone());
                                self.sh.edit_config(|cfg| {
                                    if let Some(p) = cfg.profile_mut(&pn) {
                                        for id in ids {
                                            p.fans.insert(id, FanMode::Curve(cc.clone()));
                                        }
                                    }
                                });
                                self.toast(format!("Curve copied to all fans in {profile}"));
                            }
                        });
                    });
                    // Source sensors.
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Follows").font(semi(12.0)).color(MUTED));
                        let shown: Vec<_> = s.sensors.iter().filter(|x| !x.hidden || c.source.sensors.contains(&x.id)).collect();
                        for x in shown {
                            let on = c.source.sensors.contains(&x.id);
                            if sensor_chip(ui, &x.name, x.value, on).clicked() {
                                if on {
                                    if c.source.sensors.len() > 1 {
                                        c.source.sensors.retain(|i| i != &x.id);
                                        changed = true;
                                    }
                                } else {
                                    c.source.sensors.push(x.id.clone());
                                    changed = true;
                                }
                            }
                        }
                        if c.source.sensors.len() > 1 {
                            ui.add_space(4.0);
                            let mut m = c.source.mix;
                            if segmented(ui, ("mix", &f.id), &mut m, &[(Mix::Max, "Hottest"), (Mix::Avg, "Average")], 150.0) {
                                c.source.mix = m;
                                changed = true;
                            }
                        }
                    });
                    ui.add_space(4.0);
                    let live = Live { temp: f.source_temp.filter(|_| profile == s.profile), duty: f.duty, target: f.target };
                    changed |= graph::curve_editor(ui, &format!("curve-{}", f.id), &mut c, &live, calib.as_ref(), 300.0);
                    ui.label(RichText::new("Drag points · double-click to add · right-click to remove · Shift for 0.5 steps").font(reg(11.0)).color(FAINT));

                    ui.add_space(6.0);
                    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::click());
                    ui.painter().text(r.left_center(), Align2::LEFT_CENTER, if self.advanced { I_CHEVRON_D } else { I_CHEVRON_R }, icon(10.0), MUTED);
                    ui.painter().text(r.left_center() + vec2(18.0, 0.0), Align2::LEFT_CENTER, "Smart tuning", semi(12.5), TEXT);
                    ui.painter().text(
                        r.right_center(),
                        Align2::RIGHT_CENTER,
                        format!("hyst {:.0}° · smooth {:.0}s · look-ahead {:.0}s · ramp ↑{:.0} ↓{:.0} %/s", c.hysteresis, c.smoothing, c.lookahead, c.ramp_up, c.ramp_down),
                        reg(11.0),
                        DIM,
                    );
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        self.advanced = !self.advanced;
                    }
                    if self.advanced {
                        let half = (ui.available_width() - 24.0) / 2.0;
                        ui.horizontal_top(|ui| {
                            ui.spacing_mut().item_spacing.x = 24.0;
                            ui.vertical(|ui| {
                                ui.set_width(half);
                                changed |= slider(ui, "Hysteresis", "Temperature must drop this far before the fan slows down. Stops hunting around a point.", &mut c.hysteresis, 0.0..=10.0, 0.5, 3.0, |v| format!("{v:.1} °C"));
                                changed |= slider(ui, "Smoothing", "Time constant of the temperature filter. CPU temperatures spike a lot; 3–6 s keeps fans calm.", &mut c.smoothing, 0.0..=20.0, 0.5, 4.0, |v| format!("{v:.1} s"));
                                changed |= slider(ui, "Look-ahead", "Extrapolates rising temperatures so fans spin up before heat soaks in.", &mut c.lookahead, 0.0..=10.0, 0.5, 3.0, |v| format!("{v:.1} s"));
                                changed |= slider(ui, "Minimum duty", "Never run slower than this while spinning.", &mut c.min_duty, 0.0..=60.0, 1.0, 20.0, |v| format!("{v:.0} %"));
                            });
                            ui.vertical(|ui| {
                                ui.set_width(half);
                                changed |= slider(ui, "Ramp up", "Max speed-up per second.", &mut c.ramp_up, 1.0..=100.0, 1.0, 25.0, |v| format!("{v:.0} %/s"));
                                changed |= slider(ui, "Ramp down", "Max slow-down per second. Low values make fans fade out gently.", &mut c.ramp_down, 0.5..=50.0, 0.5, 5.0, |v| format!("{v:.1} %/s"));
                                row(ui, "Zero RPM", "Let the fan stop when the curve asks for less than the threshold", 44.0, |ui| {
                                    changed |= toggle(ui, &mut c.zero_rpm).changed();
                                });
                                if c.zero_rpm {
                                    changed |= slider(ui, "Stop below", "Curve values under this stop the fan (restarts 5 % higher, with a kick-start).", &mut c.stop_below, 0.0..=50.0, 1.0, 15.0, |v| format!("{v:.0} %"));
                                }
                            });
                        });
                    }
                    if changed {
                        c.sort();
                        self.set_mode(profile, &f.id, FanMode::Curve(c));
                    }
                }
            }
        });

        card(ui, "Response", |ui| match &calib {
            Some(c) => {
                ui.horizontal(|ui| {
                    stat(ui, "Max", format!("{:.0} rpm", c.max_rpm), TEXT);
                    stat(ui, "Stalls below", format!("{:.0} %", c.stop_duty), if c.stop_duty > 0.0 { AMBER } else { DIM });
                    stat(ui, "Starts at", format!("{:.0} %", c.start_duty), TEXT);
                });
                graph::rpm_chart(ui, c, f.duty, 120.0);
            }
            None => {
                ui.label(
                    RichText::new("Not calibrated yet. Calibration measures this fan's speed range so Hush never drives it into a stall and kick-starts it reliably.")
                        .font(reg(12.0))
                        .color(DIM),
                );
            }
        });
    }

    fn set_mode(&self, profile: &str, fan: &str, m: FanMode) {
        let (p, f) = (profile.to_string(), fan.to_string());
        self.sh.edit_config(|c| {
            if let Some(pr) = c.profile_mut(&p) {
                pr.fans.insert(f, m);
            }
        });
    }

    // -----------------------------------------------------------------------------------------
    // Automation
    // -----------------------------------------------------------------------------------------

    fn automation(&mut self, ui: &mut Ui, s: &Snapshot, cfg: &Config) {
        let names: Vec<String> = cfg.profiles.iter().map(|p| p.name.clone()).collect();
        card(ui, "", |ui| {
            row(ui, "Automatic profiles", "Rules pick the profile; pinning one from the top bar, tray or CLI overrides them", 44.0, |ui| {
                let mut on = cfg.settings.automation;
                if toggle(ui, &mut on).changed() {
                    self.sh.edit_config(|c| c.settings.automation = on);
                }
            });
            row(ui, "Default profile", "Used when no rule matches", 180.0, |ui| {
                let mut b = cfg.base_profile.clone();
                if dropdown(ui, "base", &mut b, &names, 170.0) {
                    self.sh.edit_config(|c| c.base_profile = b);
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Now").font(semi(12.0)).color(MUTED));
                tag(ui, &s.profile.to_uppercase(), Color32::BLACK, ACCENT);
                ui.label(RichText::new(&s.profile_reason).font(reg(12.0)).color(DIM));
            });
        });

        card(ui, "Rules · first match wins", |ui| {
            let mut rules = cfg.rules.clone();
            let mut changed = false;
            let mut remove = None;
            let mut swap = None;
            if rules.is_empty() {
                ui.label(RichText::new("No rules yet. Add one below, e.g. Performance while a game is fullscreen, Silent at night.").font(reg(12.0)).color(DIM));
            }
            let n = rules.len();
            for (i, r) in rules.iter_mut().enumerate() {
                egui::Frame::new().fill(WELL).corner_radius(CornerRadius::same(10)).inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        changed |= toggle(ui, &mut r.enabled).changed();
                        ui.add_space(4.0);
                        ui.label(RichText::new(r.trigger.label()).font(semi(12.5)).color(if r.enabled { TEXT } else { DIM }));
                        ui.add_space(4.0);
                        changed |= self.trigger_editor(ui, i, &mut r.trigger, s);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if button(ui, I_DELETE, "", Kind::Ghost, Some(28.0), 28.0).on_hover_text("Delete rule").clicked() {
                                remove = Some(i);
                            }
                            if i + 1 < n && button(ui, "\u{E74B}", "", Kind::Ghost, Some(28.0), 28.0).on_hover_text("Lower priority").clicked() {
                                swap = Some((i, i + 1));
                            }
                            if i > 0 && button(ui, "\u{E74A}", "", Kind::Ghost, Some(28.0), 28.0).on_hover_text("Higher priority").clicked() {
                                swap = Some((i, i - 1));
                            }
                            changed |= dropdown(ui, ("rule-p", i), &mut r.profile, &names, 130.0);
                            ui.label(RichText::new("→").font(reg(14.0)).color(DIM));
                        });
                    });
                });
            }
            if let Some(i) = remove {
                rules.remove(i);
                self.proc_buf.clear();
                changed = true;
            }
            if let Some((a, b)) = swap {
                rules.swap(a, b);
                self.proc_buf.clear();
                changed = true;
            }
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                let perf = names.iter().find(|n| *n == "Performance").cloned().unwrap_or_else(|| names[0].clone());
                let quiet = names.iter().find(|n| *n == "Silent").cloned().unwrap_or_else(|| names[0].clone());
                let mut add = |t: Trigger, p: &str| {
                    rules.push(Rule { enabled: true, trigger: t, profile: p.to_string() });
                    changed = true;
                };
                if button(ui, I_ADD, "App running", Kind::Secondary, None, 30.0).clicked() {
                    add(Trigger::Process { names: vec![] }, &perf);
                }
                if button(ui, I_ADD, "Fullscreen", Kind::Secondary, None, 30.0).clicked() {
                    add(Trigger::Fullscreen, &perf);
                }
                if button(ui, I_ADD, "Schedule", Kind::Secondary, None, 30.0).clicked() {
                    add(Trigger::Time { from: "23:00".into(), to: "07:00".into() }, &quiet);
                }
                if button(ui, I_ADD, "Idle", Kind::Secondary, None, 30.0).clicked() {
                    add(Trigger::Idle { minutes: 10 }, &quiet);
                }
                if button(ui, I_ADD, "Temperature", Kind::Secondary, None, 30.0).clicked() {
                    add(Trigger::Above { sensor: "gpu".into(), temp: 70.0 }, &perf);
                }
            });
            if changed {
                self.sh.edit_config(|c| c.rules = rules);
            }
        });

        card(ui, "Profiles", |ui| {
            let mut del = None;
            for p in &cfg.profiles {
                ui.horizontal(|ui| {
                    let mut name = p.name.clone();
                    let r = text_field(ui, &mut name, "name", 200.0);
                    if r.lost_focus() && name.trim() != p.name && !name.trim().is_empty() && cfg.profile(name.trim()).is_none() {
                        let (old, new) = (p.name.clone(), name.trim().to_string());
                        self.sh.edit_config(|c| {
                            if let Some(x) = c.profile_mut(&old) {
                                x.name = new.clone();
                            }
                            if c.base_profile == old {
                                c.base_profile = new.clone();
                            }
                            for r in &mut c.rules {
                                if r.profile == old {
                                    r.profile = new.clone();
                                }
                            }
                        });
                        self.sh.send(Cmd::SetProfile(None));
                    }
                    let modes: Vec<String> = s.fans.iter().filter(|f| !f.hidden).map(|f| p.fans.get(&f.id).map(|m| m.label()).unwrap_or("BIOS").to_string()).collect();
                    ui.label(RichText::new(modes.join(" · ")).font(reg(11.5)).color(DIM));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if cfg.profiles.len() > 1 && button(ui, I_DELETE, "", Kind::Ghost, Some(28.0), 28.0).on_hover_text("Delete profile").clicked() {
                            del = Some(p.name.clone());
                        }
                    });
                });
            }
            if let Some(d) = del {
                self.sh.edit_config(|c| {
                    c.profiles.retain(|p| p.name != d);
                    let first = c.profiles[0].name.clone();
                    if c.base_profile == d {
                        c.base_profile = first.clone();
                    }
                    for r in &mut c.rules {
                        if r.profile == d {
                            r.profile = first.clone();
                        }
                    }
                });
                self.sh.send(Cmd::SetProfile(None));
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                text_field(ui, &mut self.new_profile, "New profile name", 200.0);
                let name = self.new_profile.trim().to_string();
                let ok = !name.is_empty() && cfg.profile(&name).is_none();
                if button(ui, I_ADD, &format!("Copy of {}", s.profile), if ok { Kind::Primary } else { Kind::Secondary }, None, 28.0).clicked() && ok {
                    let src = cfg.profile(&s.profile).cloned().unwrap_or_default();
                    self.sh.edit_config(|c| c.profiles.push(Profile { name: name.clone(), fans: src.fans.clone() }));
                    self.new_profile.clear();
                    self.edit_profile = Some(name);
                }
            });
        });
    }

    fn trigger_editor(&mut self, ui: &mut Ui, i: usize, t: &mut Trigger, s: &Snapshot) -> bool {
        let mut changed = false;
        match t {
            Trigger::Process { names } => {
                let buf = self.proc_buf.entry(i).or_insert_with(|| names.join(", "));
                if text_field(ui, buf, "game.exe, blender.exe", 240.0).changed() {
                    *names = buf.split([',', ';']).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
                    changed = true;
                }
                let resp = egui::ComboBox::from_id_salt(("pick", i)).width(28.0).selected_text("＋").show_ui(ui, |ui| {
                    ui.set_min_width(220.0);
                    egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                        for a in &self.running_apps {
                            if ui.selectable_label(false, a).clicked() {
                                if !names.iter().any(|n| n.eq_ignore_ascii_case(a)) {
                                    names.push(a.clone());
                                    changed = true;
                                }
                            }
                        }
                    });
                });
                if resp.response.clicked() {
                    self.running_apps = running_apps();
                }
                if changed {
                    self.proc_buf.insert(i, names.join(", "));
                }
            }
            Trigger::Time { from, to } => {
                ui.label(RichText::new("from").font(reg(12.0)).color(DIM));
                changed |= text_field(ui, from, "23:00", 64.0).changed();
                ui.label(RichText::new("to").font(reg(12.0)).color(DIM));
                changed |= text_field(ui, to, "07:00", 64.0).changed();
                let valid = hushcore::config::parse_hm(from).is_some() && hushcore::config::parse_hm(to).is_some();
                if !valid {
                    ui.label(RichText::new("use HH:MM").font(reg(11.5)).color(RED));
                }
            }
            Trigger::Fullscreen => {
                ui.label(RichText::new("a game or video is fullscreen in the foreground").font(reg(12.0)).color(DIM));
            }
            Trigger::Idle { minutes } => {
                let mut m = *minutes as f32;
                ui.label(RichText::new("for").font(reg(12.0)).color(DIM));
                if ui.add(egui::DragValue::new(&mut m).range(1.0..=240.0).speed(0.3).suffix(" min")).changed() {
                    *minutes = m as u32;
                    changed = true;
                }
            }
            Trigger::Above { sensor, temp } => {
                let ids: Vec<String> = s.sensors.iter().map(|x| x.id.clone()).collect();
                changed |= dropdown(ui, ("above-s", i), sensor, &ids, 110.0);
                ui.label(RichText::new("≥").font(reg(13.0)).color(DIM));
                changed |= ui.add(egui::DragValue::new(temp).range(30.0..=100.0).speed(0.3).suffix(" °C")).changed();
            }
        }
        changed
    }

    // -----------------------------------------------------------------------------------------
    // Settings
    // -----------------------------------------------------------------------------------------

    fn settings(&mut self, ui: &mut Ui, s: &Snapshot, cfg: &Config) {
        let st = cfg.settings.clone();
        card(ui, "General", |ui| {
            row(ui, "Start with Windows", "Runs elevated at logon through Task Scheduler — no UAC prompt", 44.0, |ui| {
                let mut on = self.startup;
                if toggle(ui, &mut on).changed() {
                    match winutil::set_startup(on) {
                        Ok(()) => self.startup = winutil::startup_enabled(),
                        Err(e) => self.toast(format!("Startup task failed: {e}")),
                    }
                }
            });
            row(ui, "Start in the tray", "Don't open this window at logon", 44.0, |ui| {
                let mut v = st.start_minimized;
                if toggle(ui, &mut v).changed() {
                    self.sh.edit_config(|c| c.settings.start_minimized = v);
                }
            });
            row(ui, "Close to tray", "The × button hides Hush; fan control keeps running", 44.0, |ui| {
                let mut v = st.close_to_tray;
                if toggle(ui, &mut v).changed() {
                    self.sh.edit_config(|c| c.settings.close_to_tray = v);
                }
            });
        });
        card(ui, "Control", |ui| {
            row(ui, "Update rate", "How often sensors are read and fans adjusted", 240.0, |ui| {
                let mut t = st.tick_ms;
                if segmented(ui, "tick", &mut t, &[(500u64, "0.5 s"), (1000, "1 s"), (2000, "2 s")], 230.0) {
                    self.sh.edit_config(|c| c.settings.tick_ms = t);
                }
            });
            row(ui, "History", "Graph memory", 240.0, |ui| {
                let mut h = st.history_minutes;
                if segmented(ui, "hist", &mut h, &[(10u32, "10m"), (30, "30m"), (60, "60m")], 230.0) {
                    self.sh.edit_config(|c| c.settings.history_minutes = h);
                }
            });
            let mut crit = st.critical_temp;
            if slider(ui, "Critical temperature", "Any CPU/GPU sensor at or above this sends every visible fan to 100 % until it drops 5 °C.", &mut crit, 70.0..=105.0, 1.0, 90.0, |v| format!("{v:.0} °C")) {
                self.sh.edit_config(|c| c.settings.critical_temp = crit);
            }
            let mut fs = st.failsafe_duty;
            if slider(ui, "Failsafe duty", "Used when a curve's sensors stop reporting.", &mut fs, 30.0..=100.0, 5.0, 80.0, |v| format!("{v:.0} %")) {
                self.sh.edit_config(|c| c.settings.failsafe_duty = fs);
            }
            row(ui, "Stall warnings", "Alert when a fan reads 0 RPM while it should spin", 44.0, |ui| {
                let mut v = st.notify_stall;
                if toggle(ui, &mut v).changed() {
                    self.sh.edit_config(|c| c.settings.notify_stall = v);
                }
            });
        });
        card(ui, "Sensors", |ui| {
            for x in &s.sensors {
                ui.horizontal(|ui| {
                    let mut name = cfg.sensor_names.get(&x.id).cloned().unwrap_or_default();
                    if text_field(ui, &mut name, &x.name, 200.0).changed() {
                        let id = x.id.clone();
                        self.sh.edit_config(|c| {
                            if name.trim().is_empty() {
                                c.sensor_names.remove(&id);
                            } else {
                                c.sensor_names.insert(id, name);
                            }
                        });
                    }
                    ui.label(RichText::new(&x.id).font(mono(11.0)).color(FAINT));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let mut vis = !x.hidden;
                        if toggle(ui, &mut vis).changed() {
                            let id = x.id.clone();
                            self.sh.edit_config(|c| {
                                c.hidden_sensors.retain(|h| *h != id);
                                if !vis {
                                    c.hidden_sensors.push(id);
                                }
                            });
                        }
                        ui.label(RichText::new(format!("{:.1} °C", x.value)).font(mono(12.0)).color(temp_color(x.value)));
                    });
                });
            }
        });
        card(ui, "Hardware", |ui| {
            for (n, ok, d) in &s.backends {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(if *ok { I_CHECK } else { I_WARN }).font(icon(12.0)).color(if *ok { GREEN } else { AMBER }));
                    ui.label(RichText::new(n).font(semi(12.5)).color(TEXT));
                    ui.label(RichText::new(d).font(reg(12.0)).color(DIM));
                });
            }
            if let Some(name) = &s.nbfc_found {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if button(ui, I_FAN, "Use it", Kind::Primary, None, 28.0).clicked() {
                        match hushcore::hw::nbfc::install() {
                            Ok(_) => self.sh.send(Cmd::Rescan),
                            Err(e) => hushcore::log(format!("nbfc install: {e}")),
                        }
                    }
                    ui.label(
                        RichText::new(format!("nbfc-linux has a fan config for {name}. Community-made and untested by Hush: watch the fans and temperatures after switching it on."))
                            .font(reg(12.0))
                            .color(AMBER),
                    );
                });
            }
            ui.label(RichText::new("Driver: PawnIO (signed, sandboxed modules) · GPU fans are never touched, the graphics driver keeps them.").font(reg(11.5)).color(FAINT));
        });
        card(ui, "Command line", |ui| {
            let dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.display().to_string())).unwrap_or_default();
            let in_path = std::env::var("PATH").unwrap_or_default().split(';').any(|p| p.trim_end_matches('\\').eq_ignore_ascii_case(dir.trim_end_matches('\\')));
            row(ui, "hush.exe", &dir, 150.0, |ui| {
                if in_path || user_path_contains(&dir) {
                    tag(ui, "IN PATH", Color32::BLACK, GREEN);
                } else if button(ui, I_TERMINAL, "Add to PATH", Kind::Secondary, None, 28.0).clicked() {
                    match add_to_user_path(&dir) {
                        Ok(()) => self.toast("Added — open a new terminal and run `hush status`"),
                        Err(e) => self.toast(format!("Could not update PATH: {e}")),
                    }
                }
            });
            let ex = "hush status              hush status --watch --json\nhush profile silent      hush profile auto\nhush set fan2 60         hush set pump bios\nhush set fan7 performance  hush reset\nhush boost 30            hush pause / resume\nhush calibrate all       hush quit";
            egui::Frame::new().fill(Color32::from_rgb(5, 5, 7)).corner_radius(CornerRadius::same(8)).inner_margin(Margin::same(12)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(ex).font(mono(12.0)).color(MUTED));
            });
            ui.label(RichText::new(r"Scripts can also talk JSON to \\.\pipe\hush directly (see README).").font(reg(11.5)).color(FAINT));
        });
        card(ui, "", |ui| {
            row(ui, "Exit Hush", "Every fan goes back to the BIOS Smart Fan curve", 170.0, |ui| {
                if button(ui, I_POWER, "Exit & restore BIOS", Kind::Danger, None, 30.0).clicked() {
                    self.quit(ui.ctx());
                }
            });
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Hush {} · by visiuun ·", env!("CARGO_PKG_VERSION"))).font(reg(11.5)).color(FAINT));
            ui.hyperlink_to(RichText::new("github.com/visiuun/hush").font(reg(11.5)).color(DIM), "https://github.com/visiuun/hush");
        });
    }
}

// ---------------------------------------------------------------------------------------------
// Small composites
// ---------------------------------------------------------------------------------------------

fn logo_mark(ui: &mut Ui, c: Pos2, r: f32, angle: f32, vel: f32) {
    let p = ui.painter();
    let blades = 5;
    // Motion blur: at speed the blades smear into a translucent disc, like a real fan.
    let blur = (vel / 70.0).clamp(0.0, 1.0);
    if blur > 0.0 {
        p.circle_filled(c, r * 1.02, ACCENT.gamma_multiply(0.16 * blur));
    }
    let ghosts = if vel > 12.0 { 3 } else { 1 };
    for g in (0..ghosts).rev() {
        let lag = g as f32 * vel * 0.006;
        let alpha = if g == 0 { 1.0 - 0.65 * blur } else { 0.22 * (1.0 - g as f32 / ghosts as f32) };
        let col = ACCENT.gamma_multiply(alpha);
        for b in 0..blades {
            let a0 = angle - lag + b as f32 / blades as f32 * std::f32::consts::TAU;
            let mut pts = Vec::new();
            for i in 0..=10 {
                let t = i as f32 / 10.0;
                pts.push(c + Vec2::angled(a0 + t * 1.2) * r * (0.3 + 0.7 * t));
            }
            for i in (0..=10).rev() {
                let t = i as f32 / 10.0;
                pts.push(c + Vec2::angled(a0 + t * 1.2 + 0.62 - 0.3 * t) * r * (0.3 + 0.7 * t));
            }
            p.add(egui::Shape::convex_polygon(pts, col, Stroke::NONE));
        }
    }
    p.circle_filled(c, r * 0.22, Color32::BLACK);
    p.circle_stroke(c, r * 0.22, Stroke::new(1.5, ACCENT));
}

fn banner(ui: &mut Ui, col: Color32, text: &str) {
    egui::Frame::new()
        .fill(col.gamma_multiply(0.08))
        .stroke(Stroke::new(1.0, col.gamma_multiply(0.35)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(I_WARN).font(icon(13.0)).color(col));
                ui.label(RichText::new(text).font(reg(12.5)).color(TEXT));
            });
        });
}

fn legend(ui: &mut Ui, series: &[Series]) {
    ui.horizontal_wrapped(|ui| {
        for s in series {
            let (r, _) = ui.allocate_exact_size(vec2(8.0, 14.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 3.5, s.color);
            ui.label(RichText::new(s.label).font(reg(11.5)).color(MUTED));
            ui.add_space(6.0);
        }
    });
}

fn stat(ui: &mut Ui, label: &str, value: String, col: Color32) {
    ui.vertical(|ui| {
        ui.set_width(118.0);
        ui.spacing_mut().item_spacing.y = 1.0;
        ui.label(RichText::new(label.to_uppercase()).font(semi(9.5)).color(DIM).extra_letter_spacing(1.0));
        ui.label(RichText::new(value).font(mono(15.0)).color(col));
    });
}

fn progress(ui: &mut Ui, f: f32, text: &str) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
    let rail = Rect::from_min_size(pos2(r.left(), r.bottom() - 4.0), vec2(r.width(), 4.0));
    ui.painter().rect_filled(rail, CornerRadius::same(2), WELL_HOVER);
    ui.painter().rect_filled(Rect::from_min_size(rail.min, vec2(rail.width() * f.clamp(0.0, 1.0), 4.0)), CornerRadius::same(2), VIOLET);
    ui.painter().text(r.left_top(), Align2::LEFT_TOP, text, reg(11.5), MUTED);
    ui.ctx().request_repaint_after(Duration::from_millis(200));
}

fn metric_tile(ui: &mut Ui, w: f32, title: &str, temp: Option<f32>, subs: &[String], hist: Option<&std::collections::VecDeque<f32>>) {
    let (rect, _) = ui.allocate_exact_size(vec2(w, 116.0), Sense::hover());
    let p = ui.painter();
    p.rect(rect, CornerRadius::same(14), CARD, Stroke::new(1.0, BORDER), StrokeKind::Inside);
    p.text(rect.min + vec2(16.0, 20.0), Align2::LEFT_CENTER, title.to_uppercase(), semi(10.5), DIM);
    let col = temp.map(temp_color).unwrap_or(DIM);
    let big = temp.map(|t| format!("{t:.0}")).unwrap_or_else(|| "—".into());
    let br = p.text(rect.min + vec2(14.0, 58.0), Align2::LEFT_CENTER, &big, light(40.0), TEXT);
    p.text(pos2(br.right() + 3.0, br.top() + 9.0), Align2::LEFT_CENTER, "°C", reg(13.0), col);
    let subs: Vec<&String> = subs.iter().filter(|s| !s.is_empty()).collect();
    for (i, s) in subs.iter().enumerate() {
        p.text(rect.min + vec2(16.0, 92.0 + 0.0 * i as f32) + vec2(i as f32 * 86.0, 0.0), Align2::LEFT_CENTER, s.as_str(), mono(11.0), MUTED);
    }
    p.circle_filled(pos2(rect.right() - 18.0, rect.top() + 20.0), 3.5, col);
    let spark = Rect::from_min_max(pos2(rect.left() + w * 0.48, rect.top() + 34.0), pos2(rect.right() - 14.0, rect.bottom() - 30.0));
    graph::sparkline(ui, spark, hist, 180, col, None);
}

fn fan_card(ui: &mut Ui, w: f32, f: &hushcore::engine::FanView, calib: Option<&hushcore::config::Calibration>, hist: Option<&std::collections::VecDeque<f32>>) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 112.0), Sense::click());
    let hov = resp.hovered();
    ui.painter().rect(rect, CornerRadius::same(14), if hov { CARD_HI } else { CARD }, Stroke::new(1.0, if f.stalled { RED } else if hov { BORDER_HI } else { BORDER }), StrokeKind::Inside);
    let c = pos2(rect.left() + 44.0, rect.center().y + 2.0);
    let col = match f.mode.as_str() {
        "BIOS" | "Paused" => MUTED,
        "Boost" | "Critical" => AMBER,
        "Calibrating" => VIOLET,
        _ => ACCENT,
    };
    graph::ring(ui, c, 26.0, f.duty, col);
    let p = ui.painter();
    p.text(c, Align2::CENTER_CENTER, format!("{:.0}", f.duty), mono(14.0), TEXT);
    p.text(c + vec2(0.0, 17.0), Align2::CENTER_CENTER, "%", reg(9.5), DIM);
    let x = rect.left() + 86.0;
    p.text(pos2(x, rect.top() + 22.0), Align2::LEFT_CENTER, &f.name, semi(13.0), TEXT);
    let rpm = f.rpm.map(|r| format!("{r:.0}")).unwrap_or_else(|| "—".into());
    let rr = p.text(pos2(x, rect.top() + 52.0), Align2::LEFT_CENTER, &rpm, light(26.0), if f.stalled { RED } else { TEXT });
    p.text(pos2(rr.right() + 4.0, rr.center().y + 4.0), Align2::LEFT_CENTER, "rpm", reg(11.0), DIM);
    let sub = match (f.source_temp, f.stalled) {
        (_, true) => "STALLED".to_string(),
        (Some(t), _) => format!("{} · {t:.0}°", f.mode),
        _ => f.mode.clone(),
    };
    p.text(pos2(x, rect.bottom() - 18.0), Align2::LEFT_CENTER, sub, reg(11.0), if f.stalled { RED } else { col });
    let spark = Rect::from_min_max(pos2(rect.right() - w * 0.3, rect.top() + 40.0), pos2(rect.right() - 12.0, rect.bottom() - 30.0));
    let range = calib.map(|c| (0.0, c.max_rpm.max(100.0)));
    graph::sparkline(ui, spark, hist, 120, col.gamma_multiply(0.8), range);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn sensor_chip(ui: &mut Ui, name: &str, v: f32, on: bool) -> egui::Response {
    let txt = format!("{name}  {v:.0}°");
    let w = text_w(ui, &txt, semi(11.5)) + 22.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 26.0), Sense::click());
    let hov = resp.hovered();
    let (fill, stroke, fg) = if on { (ACCENT_DIM, ACCENT.gamma_multiply(0.6), ACCENT) } else { (if hov { WELL_HOVER } else { WELL }, BORDER_HI, MUTED) };
    ui.painter().rect(rect, CornerRadius::same(13), fill, Stroke::new(1.0, stroke), StrokeKind::Inside);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, txt, semi(11.5), fg);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn running_apps() -> Vec<String> {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, sysinfo::ProcessRefreshKind::nothing());
    let skip = ["svchost.exe", "system", "registry", "idle", "conhost.exe", "csrss.exe", "smss.exe", "wininit.exe", "services.exe", "lsass.exe", "dwm.exe", "fontdrvhost.exe", "winlogon.exe", "runtimebroker.exe", "dllhost.exe", "sihost.exe", "taskhostw.exe", "ctfmon.exe", "hush-app.exe", "memory compression", "wudfhost.exe", "searchindexer.exe"];
    let mut v: Vec<String> = sys
        .processes()
        .values()
        .map(|p| p.name().to_string_lossy().to_string())
        .filter(|n| !skip.contains(&n.to_lowercase().as_str()))
        .collect();
    v.sort_by_key(|a| a.to_lowercase());
    v.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    v
}

fn user_path_contains(dir: &str) -> bool {
    winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey("Environment")
        .and_then(|k| k.get_value::<String, _>("Path"))
        .is_ok_and(|p| p.split(';').any(|x| x.trim_end_matches('\\').eq_ignore_ascii_case(dir.trim_end_matches('\\'))))
}

fn add_to_user_path(dir: &str) -> std::io::Result<()> {
    let (env, _) = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER).create_subkey("Environment")?;
    let cur: String = env.get_value("Path").unwrap_or_default();
    let new = if cur.trim().is_empty() { dir.to_string() } else { format!("{};{dir}", cur.trim_end_matches(';')) };
    env.set_value("Path", &new)?;
    // Tell Explorer so new terminals pick it up.
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE};
        let s: Vec<u16> = "Environment\0".encode_utf16().collect();
        let _ = SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, windows::Win32::Foundation::WPARAM(0), windows::Win32::Foundation::LPARAM(s.as_ptr() as isize), SMTO_ABORTIFHUNG, 2000, None);
    }
    Ok(())
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.pending_hide > 0 {
            self.pending_hide -= 1;
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            if self.pending_hide == 0 {
                let h = self.hwnd.load(Ordering::Acquire);
                if h != 0 {
                    winutil::set_cloaked(h, false);
                }
            } else {
                ctx.request_repaint();
            }
        }
        while let Ok(cmd) = self.rx.try_recv() {
            match cmd {
                GuiCmd::Show(page) => {
                    if let Some(p) = page.as_deref().and_then(Page::parse) {
                        self.page = p;
                    }
                    self.show(ctx);
                }
                GuiCmd::TrayClick => self.show(ctx),
                GuiCmd::Menu(id) => self.handle_tray(ctx, id),
                GuiCmd::Quit => self.quit(ctx),
            }
        }
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close {
            if self.sh.config.read().settings.close_to_tray {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                self.hide(ctx);
            } else {
                self.quit(ctx);
            }
        }
        let minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        if minimized && self.visible.load(Ordering::Relaxed) {
            // Minimize behaves like hide: no rendering while out of sight.
            self.visible.store(false, Ordering::Release);
        } else if !minimized && ctx.input(|i| i.viewport().focused.unwrap_or(false)) {
            self.visible.store(true, Ordering::Release);
        }
        let snap = self.sh.snapshot();
        let names: Vec<String> = self.sh.config.read().profiles.iter().map(|p| p.name.clone()).collect();
        if self.tray.as_ref().is_some_and(|t| t.needs_rebuild(&names)) {
            self.tray = None;
            self.tray = Tray::build(&names);
        }
        if self.last_tray_sync.elapsed() > Duration::from_millis(900) {
            if let Some(t) = &mut self.tray {
                t.sync(&snap);
            }
            self.last_tray_sync = Instant::now();
        }
        for a in &snap.alerts {
            if !self.seen_alerts.contains(a) {
                self.seen_alerts.push(a.clone());
                self.toast = Some((a.clone(), Instant::now()));
            }
        }
        self.seen_alerts.retain(|a| snap.alerts.contains(a));
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if !ctx.egui_wants_keyboard_input() {
            ctx.input(|i| {
                for (k, p) in [(egui::Key::Num1, Page::Overview), (egui::Key::Num2, Page::Fans), (egui::Key::Num3, Page::Automation), (egui::Key::Num4, Page::Settings)] {
                    if i.key_pressed(k) {
                        self.page = p;
                    }
                }
            });
        }
        let snap = self.sh.snapshot();
        let cfg = self.sh.config.read().clone();

        egui::Panel::left("nav")
            .exact_size(66.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(BG).stroke(Stroke::new(1.0, Color32::from_rgb(14, 14, 17))))
            .show(ui, |ui| self.nav(ui));

        egui::Panel::top("top")
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin { left: 24, right: 24, top: 16, bottom: 12 }))
            .show(ui, |ui| self.top_bar(ui, &snap, &cfg));

        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(Margin { left: 24, right: 16, top: 4, bottom: 0 })).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.set_width(ui.available_width() - 8.0);
                ui.spacing_mut().item_spacing.y = 12.0;
                match self.page {
                    Page::Overview => self.overview(ui, &snap, &cfg),
                    Page::Fans => self.fans_page(ui, &snap, &cfg),
                    Page::Automation => self.automation(ui, &snap, &cfg),
                    Page::Settings => self.settings(ui, &snap, &cfg),
                }
                ui.add_space(16.0);
            });
        });

        // Toast.
        if let Some((msg, t)) = &self.toast {
            let age = t.elapsed().as_secs_f32();
            if age < 6.0 {
                let a = (1.0 - ((age - 5.0).max(0.0))).clamp(0.0, 1.0);
                let screen = ctx.content_rect();
                let font = reg(12.5);
                let w = (text_w(ui, msg, font.clone()) + 40.0).min(560.0);
                let r = Rect::from_min_size(pos2(screen.center().x - w / 2.0 + 33.0, screen.bottom() - 58.0), vec2(w, 36.0));
                let p = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, ui.scope_id().with("toast")));
                p.rect(r, CornerRadius::same(18), CARD_HI.gamma_multiply(a), Stroke::new(1.0, BORDER_HI.gamma_multiply(a)), StrokeKind::Inside);
                p.circle_filled(pos2(r.left() + 18.0, r.center().y), 3.5, ACCENT.gamma_multiply(a));
                p.text(pos2(r.left() + 30.0, r.center().y), Align2::LEFT_CENTER, msg, font, TEXT.gamma_multiply(a));
                ctx.request_repaint_after(Duration::from_millis(100));
            } else {
                self.toast = None;
            }
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 1.0]
    }
}
