//! Design tokens + custom widgets for the OLED theme.

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Layout, Margin, Pos2, Rect, Response, Sense,
    Stroke, StrokeKind, Ui, Vec2,
};
use std::sync::Arc;

pub const BG: Color32 = Color32::from_rgb(0, 0, 0);
pub const CARD: Color32 = Color32::from_rgb(8, 8, 10);
pub const CARD_HI: Color32 = Color32::from_rgb(13, 13, 16);
pub const WELL: Color32 = Color32::from_rgb(17, 17, 21);
pub const WELL_HOVER: Color32 = Color32::from_rgb(25, 25, 31);
pub const SEL: Color32 = Color32::from_rgb(32, 32, 39);
pub const BORDER: Color32 = Color32::from_rgb(24, 24, 29);
pub const BORDER_HI: Color32 = Color32::from_rgb(40, 40, 48);
pub const TEXT: Color32 = Color32::from_rgb(244, 244, 245);
pub const MUTED: Color32 = Color32::from_rgb(161, 161, 170);
pub const DIM: Color32 = Color32::from_rgb(113, 113, 122);
pub const FAINT: Color32 = Color32::from_rgb(52, 52, 60);
pub const ACCENT: Color32 = Color32::from_rgb(94, 234, 212);
pub const ACCENT_DIM: Color32 = Color32::from_rgb(20, 60, 56);
pub const GREEN: Color32 = Color32::from_rgb(74, 222, 128);
pub const AMBER: Color32 = Color32::from_rgb(251, 191, 36);
pub const RED: Color32 = Color32::from_rgb(248, 87, 98);
pub const BLUE: Color32 = Color32::from_rgb(96, 165, 250);
pub const VIOLET: Color32 = Color32::from_rgb(167, 139, 250);
pub const PINK: Color32 = Color32::from_rgb(244, 114, 182);

/// Distinct series colours for graphs.
pub const SERIES: [Color32; 8] = [ACCENT, VIOLET, AMBER, BLUE, PINK, GREEN, RED, Color32::from_rgb(226, 232, 240)];

// Segoe Fluent Icons code points.
pub const I_HOME: &str = "\u{E80F}";
pub const I_FAN: &str = "\u{E9CA}";
pub const I_FLOW: &str = "\u{E945}";
pub const I_SETTINGS: &str = "\u{E713}";
pub const I_BOLT: &str = "\u{E945}";
pub const I_PAUSE: &str = "\u{E769}";
pub const I_PLAY: &str = "\u{E768}";
pub const I_ADD: &str = "\u{E710}";
pub const I_DELETE: &str = "\u{E74D}";
pub const I_WARN: &str = "\u{E7BA}";
pub const I_CHECK: &str = "\u{E73E}";
pub const I_TERMINAL: &str = "\u{E756}";
pub const I_RULER: &str = "\u{ECC6}";
pub const I_POWER: &str = "\u{E7E8}";
pub const I_COPY: &str = "\u{E8C8}";
pub const I_CHEVRON_D: &str = "\u{E70D}";
pub const I_CHEVRON_R: &str = "\u{E76C}";

pub fn semi(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semi".into()))
}
pub fn light(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("light".into()))
}
pub fn reg(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}
pub fn icon(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("icons".into()))
}
pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

pub fn install_fonts(ctx: &egui::Context) {
    let fonts = std::path::PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| r"C:\Windows".into())).join("Fonts");
    let read = |n: &str| std::fs::read(fonts.join(n)).ok();
    let mut defs = FontDefinitions::default();
    let mut add = |key: &str, names: &[&str]| -> bool {
        for n in names {
            if let Some(b) = read(n) {
                defs.font_data.insert(key.into(), Arc::new(FontData::from_owned(b)));
                return true;
            }
        }
        false
    };
    let has_reg = add("reg", &["segoeui.ttf"]);
    let has_semi = add("semi", &["seguisb.ttf", "segoeuib.ttf"]);
    let has_bold = add("bold", &["segoeuib.ttf"]);
    let has_light = add("light", &["segoeuisl.ttf", "segoeuil.ttf", "segoeui.ttf"]);
    let has_icons = add("icons", &["SegoeIcons.ttf", "segmdl2.ttf"]);
    let has_sym = add("sym", &["seguisym.ttf"]);
    let has_mono = add("mono", &["CascadiaMono.ttf", "consola.ttf"]);

    let egui_default = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let chain = |primary: Option<&str>| -> Vec<String> {
        let mut v: Vec<String> = primary.into_iter().map(String::from).collect();
        if has_reg {
            v.push("reg".into());
        }
        if has_sym {
            v.push("sym".into());
        }
        for d in &egui_default {
            if !v.contains(d) {
                v.push(d.clone());
            }
        }
        v
    };
    defs.families.insert(FontFamily::Proportional, chain(None));
    let mut m = chain(has_mono.then_some("mono"));
    if let Some(def_mono) = FontDefinitions::default().families.get(&FontFamily::Monospace) {
        m.extend(def_mono.iter().cloned());
    }
    defs.families.insert(FontFamily::Monospace, m);
    defs.families.insert(FontFamily::Name("semi".into()), chain(has_semi.then_some("semi")));
    defs.families.insert(FontFamily::Name("bold".into()), chain(has_bold.then_some("bold")));
    defs.families.insert(FontFamily::Name("light".into()), chain(has_light.then_some("light")));
    defs.families.insert(FontFamily::Name("icons".into()), chain(has_icons.then_some("icons")));
    ctx.set_fonts(defs);
}

pub fn install_style(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.all_styles_mut(|s| {
        let v = &mut s.visuals;
        v.dark_mode = true;
        v.panel_fill = BG;
        v.window_fill = CARD_HI;
        v.window_stroke = Stroke::new(1.0, BORDER_HI);
        v.extreme_bg_color = WELL;
        v.faint_bg_color = CARD_HI;
        v.override_text_color = Some(TEXT);
        v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        v.hyperlink_color = ACCENT;
        v.window_corner_radius = CornerRadius::same(12);
        v.menu_corner_radius = CornerRadius::same(10);
        v.popup_shadow = egui::epaint::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(200) };
        for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
            w.corner_radius = CornerRadius::same(8);
        }
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        v.widgets.inactive.bg_fill = WELL;
        v.widgets.inactive.weak_bg_fill = WELL;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER_HI);
        v.widgets.hovered.bg_fill = WELL_HOVER;
        v.widgets.hovered.weak_bg_fill = WELL_HOVER;
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, BORDER_HI);
        v.widgets.active.bg_fill = SEL;
        v.widgets.active.weak_bg_fill = SEL;
        v.widgets.open.weak_bg_fill = SEL;
        s.spacing.item_spacing = Vec2::new(8.0, 8.0);
        s.spacing.button_padding = Vec2::new(10.0, 5.0);
        s.spacing.interact_size.y = 28.0;
        s.spacing.scroll.bar_width = 6.0;
        s.spacing.scroll.floating = true;
        s.interaction.tooltip_delay = 0.25;
    });
}

pub fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// Cool → warm → hot colour for a temperature.
pub fn temp_color(t: f32) -> Color32 {
    if t < 55.0 {
        lerp_color(BLUE, ACCENT, ((t - 30.0) / 25.0).clamp(0.0, 1.0))
    } else if t < 75.0 {
        lerp_color(ACCENT, AMBER, (t - 55.0) / 20.0)
    } else {
        lerp_color(AMBER, RED, (t - 75.0) / 12.0)
    }
}

pub fn text_w(ui: &Ui, s: &str, f: FontId) -> f32 {
    ui.painter().layout_no_wrap(s.to_string(), f, TEXT).size().x
}

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Primary,
    Secondary,
    Ghost,
    Danger,
}

/// Button with optional icon glyph. `w` = None → fit content.
pub fn button(ui: &mut Ui, ic: &str, label: &str, kind: Kind, w: Option<f32>, h: f32) -> Response {
    let font = semi(12.5);
    let iw = if ic.is_empty() { 0.0 } else if label.is_empty() { 14.0 } else { 14.0 + 7.0 };
    let cw = text_w(ui, label, font.clone()) + iw + if label.is_empty() { 18.0 } else { 26.0 };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w.unwrap_or(cw).max(if w.is_some() { 0.0 } else { cw }), h), Sense::click());
    let hov = resp.hovered();
    let (fill, stroke, fg) = match kind {
        Kind::Primary => (if hov { lerp_color(ACCENT, Color32::WHITE, 0.15) } else { ACCENT }, Color32::TRANSPARENT, Color32::BLACK),
        Kind::Secondary => (if hov { WELL_HOVER } else { WELL }, BORDER_HI, TEXT),
        Kind::Ghost => (if hov { WELL } else { Color32::TRANSPARENT }, if hov { BORDER_HI } else { Color32::TRANSPARENT }, if hov { TEXT } else { MUTED }),
        Kind::Danger => (if hov { Color32::from_rgb(48, 14, 18) } else { Color32::from_rgb(28, 9, 12) }, Color32::from_rgb(80, 24, 30), RED),
    };
    let rect = if resp.is_pointer_button_down_on() { rect.shrink(0.5) } else { rect };
    ui.painter().rect(rect, CornerRadius::same(8), fill, Stroke::new(1.0, stroke), StrokeKind::Inside);
    if label.is_empty() {
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, ic, icon(14.0), fg);
    } else {
        let total = iw + text_w(ui, label, font.clone());
        let mut x = rect.center().x - total / 2.0;
        if !ic.is_empty() {
            ui.painter().text(Pos2::new(x + 7.0, rect.center().y), Align2::CENTER_CENTER, ic, icon(13.0), fg);
            x += iw;
        }
        ui.painter().text(Pos2::new(x, rect.center().y), Align2::LEFT_CENTER, label, font, fg);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Pill-shaped segmented control. Returns true if the value changed.
pub fn segmented<T: PartialEq + Clone>(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, value: &mut T, opts: &[(T, &str)], width: f32) -> bool {
    let h = 30.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, h), Sense::hover());
    ui.painter().rect(rect, CornerRadius::same(9), WELL, Stroke::new(1.0, BORDER), StrokeKind::Inside);
    let inner = rect.shrink(3.0);
    let seg_w = inner.width() / opts.len() as f32;
    let base = ui.make_persistent_id(id);
    let sel_idx = opts.iter().position(|(v, _)| v == value);
    // Animated selection pill.
    if let Some(i) = sel_idx {
        let x = ui.ctx().animate_value_with_time(base.with("pill"), inner.min.x + seg_w * i as f32, 0.12);
        let r = Rect::from_min_size(Pos2::new(x, inner.min.y), Vec2::new(seg_w, inner.height()));
        ui.painter().rect(r, CornerRadius::same(7), SEL, Stroke::new(1.0, BORDER_HI), StrokeKind::Inside);
    }
    let mut changed = false;
    for (i, (v, label)) in opts.iter().enumerate() {
        let r = Rect::from_min_size(Pos2::new(inner.min.x + seg_w * i as f32, inner.min.y), Vec2::new(seg_w, inner.height()));
        let resp = ui.interact(r, base.with(i), Sense::click());
        let sel = Some(i) == sel_idx;
        let col = if sel { TEXT } else if resp.hovered() { MUTED } else { DIM };
        ui.painter().text(r.center(), Align2::CENTER_CENTER, *label, semi(12.0), col);
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !sel {
            *value = v.clone();
            changed = true;
        }
    }
    changed
}

pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(38.0, 22.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let fill = lerp_color(WELL_HOVER, ACCENT, t);
    ui.painter().rect(rect, CornerRadius::same(11), fill, Stroke::new(1.0, lerp_color(BORDER_HI, ACCENT, t)), StrokeKind::Inside);
    let x = egui::lerp((rect.left() + 11.0)..=(rect.right() - 11.0), t);
    ui.painter().circle_filled(Pos2::new(x, rect.center().y), 7.5, if *on { Color32::BLACK } else { MUTED });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Compact labelled slider for f32 values; double-click resets to `default`.
#[allow(clippy::too_many_arguments)]
pub fn slider(ui: &mut Ui, label: &str, hint: &str, v: &mut f32, range: std::ops::RangeInclusive<f32>, step: f32, default: f32, fmt: impl Fn(f32) -> String) -> bool {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 42.0), Sense::click_and_drag());
    let (min, max) = (*range.start(), *range.end());
    let p = ui.painter();
    p.text(rect.left_top() + Vec2::new(0.0, 9.0), Align2::LEFT_CENTER, label, semi(12.5), TEXT);
    p.text(rect.right_top() + Vec2::new(0.0, 9.0), Align2::RIGHT_CENTER, fmt(*v), mono(12.0), ACCENT);
    let rail = Rect::from_min_max(Pos2::new(rect.left() + 7.0, rect.bottom() - 13.0), Pos2::new(rect.right() - 7.0, rect.bottom() - 9.0));
    let mut changed = false;
    if resp.double_clicked() {
        *v = default;
        changed = true;
    } else if let Some(pt) = resp.interact_pointer_pos().filter(|_| resp.dragged() || resp.is_pointer_button_down_on()) {
        let t = ((pt.x - rail.left()) / rail.width()).clamp(0.0, 1.0);
        let nv = ((min + t * (max - min)) / step).round() * step;
        if (nv - *v).abs() > f32::EPSILON {
            *v = nv.clamp(min, max);
            changed = true;
        }
    }
    let t = ((*v - min) / (max - min)).clamp(0.0, 1.0);
    let p = ui.painter();
    p.rect_filled(rail, CornerRadius::same(2), WELL_HOVER);
    p.rect_filled(Rect::from_min_max(rail.min, Pos2::new(rail.left() + t * rail.width(), rail.bottom())), CornerRadius::same(2), ACCENT);
    let kx = rail.left() + t * rail.width();
    let hov = resp.hovered() || resp.dragged();
    p.circle_filled(Pos2::new(kx, rail.center().y), if hov { 8.0 } else { 7.0 }, Color32::WHITE);
    let r = resp.on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
    if !hint.is_empty() {
        r.on_hover_text(format!("{hint}\n\nDouble-click to reset"));
    }
    changed
}

/// Card container with an optional small uppercase header.
pub fn card<R>(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if !title.is_empty() {
                section(ui, title);
                ui.add_space(4.0);
            }
            add(ui)
        })
        .inner
}

pub fn section(ui: &mut Ui, title: &str) {
    ui.label(egui::RichText::new(title.to_uppercase()).font(semi(10.5)).color(DIM).extra_letter_spacing(1.3));
}

/// Label + hint on the left, control on the right.
pub fn row(ui: &mut Ui, label: &str, hint: &str, ctrl_w: f32, ctrl: impl FnOnce(&mut Ui)) {
    let w = ui.available_width();
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_width((w - ctrl_w - 16.0).max(80.0));
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(egui::RichText::new(label).font(semi(12.5)).color(TEXT));
            if !hint.is_empty() {
                ui.label(egui::RichText::new(hint).font(reg(11.5)).color(DIM));
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| ctrl(ui));
    });
}

pub fn chip(ui: &mut Ui, dot: Color32, text: &str, pulse: bool) -> Response {
    let font = semi(11.5);
    let w = text_w(ui, text, font.clone()) + 30.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 26.0), Sense::hover());
    ui.painter().rect(rect, CornerRadius::same(13), CARD_HI, Stroke::new(1.0, BORDER), StrokeKind::Inside);
    let c = Pos2::new(rect.left() + 13.0, rect.center().y);
    if pulse {
        let t = ui.input(|i| i.time) as f32;
        let a = (t * 2.2).sin() * 0.5 + 0.5;
        ui.painter().circle_filled(c, 3.5 + a * 3.0, dot.gamma_multiply(0.25 * (1.0 - a) + 0.05));
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(40));
    }
    ui.painter().circle_filled(c, 3.5, dot);
    ui.painter().text(Pos2::new(rect.left() + 23.0, rect.center().y), Align2::LEFT_CENTER, text, font, TEXT);
    resp
}

/// Small rounded tag.
pub fn tag(ui: &mut Ui, text: &str, fg: Color32, bg: Color32) {
    let font = semi(10.5);
    let w = text_w(ui, text, font.clone()) + 14.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 19.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(6), bg);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, font, fg);
}

/// Text field styled like the rest of the UI.
pub fn text_field(ui: &mut Ui, s: &mut String, hint: &str, w: f32) -> Response {
    ui.add_sized([w, 28.0], egui::TextEdit::singleline(s).hint_text(egui::RichText::new(hint).color(FAINT)).font(reg(12.5)).margin(Margin::symmetric(8, 5)))
}

/// Dropdown over string options; returns true on change.
pub fn dropdown(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, value: &mut String, options: &[String], w: f32) -> bool {
    let mut changed = false;
    egui::ComboBox::from_id_salt(id).width(w).selected_text(egui::RichText::new(value.as_str()).font(reg(12.5))).show_ui(ui, |ui| {
        for o in options {
            if ui.selectable_label(*value == *o, o).clicked() && *value != *o {
                *value = o.clone();
                changed = true;
            }
        }
    });
    changed
}
