//! Custom-painted graphs: history lines, sparklines, and the draggable fan-curve editor.

use super::widgets::*;
use eframe::egui::{self, Align2, Color32, CornerRadius, Mesh, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui, Vec2, pos2, vec2};
use hushcore::config::{Calibration, Curve};
use hushcore::engine::History;
use std::collections::VecDeque;

/// Filled gradient area under a polyline (vertical alpha fade).
fn area(painter: &egui::Painter, pts: &[Pos2], bottom: f32, color: Color32, alpha: f32) {
    if pts.len() < 2 {
        return;
    }
    let mut mesh = Mesh::default();
    let top_c = color.gamma_multiply(alpha);
    let bot_c = color.gamma_multiply(0.0);
    for p in pts {
        mesh.colored_vertex(*p, top_c);
        mesh.colored_vertex(pos2(p.x, bottom), bot_c);
    }
    for i in 0..pts.len() as u32 - 1 {
        let a = i * 2;
        mesh.add_triangle(a, a + 1, a + 2);
        mesh.add_triangle(a + 1, a + 3, a + 2);
    }
    painter.add(Shape::mesh(mesh));
}

pub fn sparkline(ui: &mut Ui, rect: Rect, data: Option<&VecDeque<f32>>, last_n: usize, color: Color32, range: Option<(f32, f32)>) {
    let Some(d) = data else { return };
    let start = d.len().saturating_sub(last_n);
    let vals: Vec<f32> = d.iter().skip(start).cloned().collect();
    let finite: Vec<f32> = vals.iter().cloned().filter(|v| v.is_finite()).collect();
    if finite.len() < 2 {
        return;
    }
    let (mut lo, mut hi) = range.unwrap_or_else(|| (finite.iter().cloned().fold(f32::MAX, f32::min), finite.iter().cloned().fold(f32::MIN, f32::max)));
    if hi - lo < 4.0 {
        let m = (hi + lo) / 2.0;
        lo = m - 2.0;
        hi = m + 2.0;
    }
    let n = last_n.max(2) as f32 - 1.0;
    let off = last_n - vals.len();
    let pts: Vec<Pos2> = vals
        .iter()
        .enumerate()
        .filter(|(_, v)| v.is_finite())
        .map(|(i, v)| pos2(rect.left() + (i + off) as f32 / n * rect.width(), rect.bottom() - (v - lo) / (hi - lo) * rect.height()))
        .collect();
    let p = ui.painter_at(rect.expand(2.0));
    area(&p, &pts, rect.bottom(), color, 0.18);
    p.add(Shape::line(pts, Stroke::new(1.5, color)));
}

pub struct Series<'a> {
    pub key: String,
    pub label: &'a str,
    pub color: Color32,
}

/// Time-series chart with axes, hover crosshair and min/max-per-pixel decimation.
#[allow(clippy::too_many_arguments)]
pub fn history_chart(ui: &mut Ui, id: &str, hist: &History, series: &[Series], window_s: f64, unit: &str, floor: Option<(f32, f32)>, height: f32) {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, height), Sense::hover());
    let plot = Rect::from_min_max(rect.min + vec2(38.0, 6.0), rect.max - vec2(8.0, 20.0));
    let p = ui.painter_at(rect);
    let Some(&t_end) = hist.t.back() else {
        p.text(rect.center(), Align2::CENTER_CENTER, "Collecting data…", reg(12.0), DIM);
        return;
    };
    let t0 = t_end - window_s;
    let first = hist.t.partition_point(|&t| t < t0);

    // Y range from visible data.
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    for s in series {
        if let Some(d) = hist.get(&s.key) {
            for v in d.iter().skip(first).filter(|v| v.is_finite()) {
                lo = lo.min(*v);
                hi = hi.max(*v);
            }
        }
    }
    if lo > hi {
        lo = 0.0;
        hi = 100.0;
    }
    if let Some((a, b)) = floor {
        lo = lo.min(a);
        hi = hi.max(b);
    }
    let pad = ((hi - lo) * 0.12).max(2.0);
    let (lo, hi) = ((lo - pad).max(if unit == "rpm" || unit == "%" { 0.0 } else { -50.0 }), hi + pad);
    let step = nice_step((hi - lo) / 4.0);
    let x_of = |t: f64| plot.left() + ((t - t0) / window_s) as f32 * plot.width();
    let y_of = |v: f32| plot.bottom() - (v - lo) / (hi - lo) * plot.height();

    // Grid.
    let mut g = (lo / step).ceil() * step;
    while g <= hi {
        let y = y_of(g);
        p.hline(plot.x_range(), y, Stroke::new(1.0, Color32::from_rgb(18, 18, 22)));
        // The unit label owns the top-left corner.
        if y - plot.top() > 14.0 {
            p.text(pos2(plot.left() - 8.0, y), Align2::RIGHT_CENTER, format!("{g:.0}"), mono(10.0), FAINT);
        }
        g += step;
    }
    let tick = if window_s <= 120.0 { 30.0 } else if window_s <= 600.0 { 60.0 } else if window_s <= 1800.0 { 300.0 } else { 600.0 };
    let mut k = 0.0;
    while k <= window_s {
        let x = x_of(t_end - k);
        p.vline(x, plot.y_range(), Stroke::new(1.0, Color32::from_rgb(14, 14, 17)));
        let lbl = if k == 0.0 { "now".to_string() } else if k < 60.0 { format!("-{k:.0}s") } else { format!("-{:.0}m", k / 60.0) };
        p.text(pos2(x, plot.bottom() + 11.0), Align2::CENTER_CENTER, lbl, mono(9.5), FAINT);
        k += tick;
    }
    p.text(pos2(plot.left() - 8.0, plot.top() - 1.0), Align2::RIGHT_TOP, unit, mono(9.5), FAINT);

    // Lines (decimated to ~1 point per pixel, keeping extremes).
    let n = hist.t.len();
    let px = plot.width().max(1.0) as usize;
    let stride = ((n - first) / px.max(1)).max(1);
    for s in series {
        let Some(d) = hist.get(&s.key) else { continue };
        let mut pts = Vec::with_capacity(px * 2);
        let mut i = first;
        while i < n {
            let j = (i + stride).min(n);
            let (mut mn, mut mx, mut any) = (f32::MAX, f32::MIN, false);
            for v in d.range(i..j).filter(|v| v.is_finite()) {
                mn = mn.min(*v);
                mx = mx.max(*v);
                any = true;
            }
            if any {
                let x = x_of(hist.t[i]);
                if stride > 1 && (mx - mn) > 0.01 {
                    pts.push(pos2(x, y_of(mx)));
                    pts.push(pos2(x, y_of(mn)));
                } else {
                    pts.push(pos2(x, y_of(mx)));
                }
            }
            i = j;
        }
        if series.len() <= 2 {
            area(&p, &pts, plot.bottom(), s.color, 0.10);
        }
        p.add(Shape::line(pts, Stroke::new(1.6, s.color)));
    }

    // Hover crosshair + readout.
    if let Some(hp) = resp.hover_pos().filter(|h| plot.contains(*h)) {
        let th = t0 + ((hp.x - plot.left()) / plot.width()) as f64 * window_s;
        let idx = hist.t.partition_point(|&t| t < th).min(n - 1);
        let x = x_of(hist.t[idx]);
        p.vline(x, plot.y_range(), Stroke::new(1.0, BORDER_HI));
        let mut lines = Vec::new();
        for s in series {
            if let Some(v) = hist.get(&s.key).and_then(|d| d.get(idx)).filter(|v| v.is_finite()) {
                p.circle_filled(pos2(x, y_of(*v)), 3.5, s.color);
                lines.push((s.color, format!("{}  {v:.0} {unit}", s.label)));
            }
        }
        let ago = t_end - hist.t[idx];
        let head = if ago < 1.0 { "now".to_string() } else { format!("{ago:.0}s ago") };
        let bw = 150.0;
        let bh = 22.0 + lines.len() as f32 * 17.0;
        let bx = if x + bw + 14.0 < plot.right() { x + 10.0 } else { x - bw - 10.0 };
        let b = Rect::from_min_size(pos2(bx, plot.top() + 4.0), vec2(bw, bh));
        let fp = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, ui.scope_id().with(id).with("tip")));
        fp.rect(b, CornerRadius::same(8), Color32::from_rgba_premultiplied(10, 10, 13, 245), Stroke::new(1.0, BORDER_HI), StrokeKind::Inside);
        fp.text(b.min + vec2(10.0, 11.0), Align2::LEFT_CENTER, head, mono(10.0), DIM);
        for (i, (c, l)) in lines.iter().enumerate() {
            let y = b.min.y + 28.0 + i as f32 * 17.0;
            fp.circle_filled(pos2(b.min.x + 13.0, y), 3.0, *c);
            fp.text(pos2(b.min.x + 22.0, y), Align2::LEFT_CENTER, l, reg(11.5), TEXT);
        }
    }
}

fn nice_step(raw: f32) -> f32 {
    let mag = 10f32.powf(raw.max(1e-3).log10().floor());
    let f = raw / mag;
    mag * if f < 1.5 { 1.0 } else if f < 3.5 { 2.0 } else if f < 7.5 { 5.0 } else { 10.0 }
}

// ---------------------------------------------------------------------------------------------
// Curve editor
// ---------------------------------------------------------------------------------------------

pub const T_MIN: f32 = 20.0;
pub const T_MAX: f32 = 100.0;

pub struct Live {
    pub temp: Option<f32>,
    pub duty: f32,
    pub target: Option<f32>,
}

/// Drag points · double-click to add · right-click to remove. Returns true on edit.
pub fn curve_editor(ui: &mut Ui, id: &str, curve: &mut Curve, live: &Live, calib: Option<&Calibration>, height: f32) -> bool {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, height), Sense::click_and_drag());
    let plot = Rect::from_min_max(rect.min + vec2(40.0, 10.0), rect.max - vec2(14.0, 26.0));
    let p = ui.painter_at(rect);
    let x_of = |t: f32| plot.left() + (t - T_MIN) / (T_MAX - T_MIN) * plot.width();
    let y_of = |d: f32| plot.bottom() - d / 100.0 * plot.height();
    let t_of = |x: f32| (T_MIN + (x - plot.left()) / plot.width() * (T_MAX - T_MIN)).clamp(T_MIN, T_MAX);
    let d_of = |y: f32| ((plot.bottom() - y) / plot.height() * 100.0).clamp(0.0, 100.0);

    p.rect(plot.expand(1.0), CornerRadius::same(6), Color32::from_rgb(4, 4, 6), Stroke::new(1.0, BORDER), StrokeKind::Outside);
    for d in (0..=100).step_by(20) {
        let y = y_of(d as f32);
        p.hline(plot.x_range(), y, Stroke::new(1.0, Color32::from_rgb(18, 18, 22)));
        p.text(pos2(plot.left() - 8.0, y), Align2::RIGHT_CENTER, format!("{d}%"), mono(10.0), FAINT);
    }
    for t in (20..=100).step_by(10) {
        let x = x_of(t as f32);
        p.vline(x, plot.y_range(), Stroke::new(1.0, Color32::from_rgb(14, 14, 17)));
        p.text(pos2(x, plot.bottom() + 13.0), Align2::CENTER_CENTER, format!("{t}°"), mono(10.0), FAINT);
    }
    // Heat zones along the bottom edge.
    for (a, b, c) in [(20.0, 55.0, BLUE), (55.0, 75.0, ACCENT), (75.0, 87.0, AMBER), (87.0, 100.0, RED)] {
        let r = Rect::from_min_max(pos2(x_of(a), plot.bottom() - 2.0), pos2(x_of(b), plot.bottom()));
        p.rect_filled(r, 0.0, c.gamma_multiply(0.5));
    }

    // Zero-RPM band / floor.
    if curve.zero_rpm {
        let r = Rect::from_min_max(pos2(plot.left(), y_of(curve.stop_below)), plot.max);
        p.rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(96, 165, 250, 10));
        p.text(pos2(plot.right() - 6.0, y_of(curve.stop_below) - 8.0), Align2::RIGHT_CENTER, "fan stops below", reg(10.0), BLUE.gamma_multiply(0.8));
        dashed_h(&p, plot, y_of(curve.stop_below), BLUE.gamma_multiply(0.5));
    } else {
        let floor = curve.min_duty.max(calib.map_or(0.0, |c| c.stop_duty + 3.0));
        dashed_h(&p, plot, y_of(floor), FAINT);
        p.text(pos2(plot.right() - 6.0, y_of(floor) - 8.0), Align2::RIGHT_CENTER, format!("floor {floor:.0}%"), reg(10.0), DIM);
    }
    if let Some(c) = calib.filter(|c| c.stop_duty > 0.0) {
        let y = y_of(c.stop_duty);
        p.text(pos2(plot.left() + 6.0, y - 8.0), Align2::LEFT_CENTER, format!("stall {:.0}%", c.stop_duty), reg(10.0), RED.gamma_multiply(0.7));
        dashed_h(&p, plot, y, RED.gamma_multiply(0.35));
    }

    // Interaction.
    let drag_id = ui.make_persistent_id(id).with("drag");
    let mut dragging: Option<usize> = ui.data(|d| d.get_temp(drag_id));
    let hover = resp.hover_pos();
    let nearest = |pos: Pos2, pts: &[[f32; 2]]| -> Option<usize> {
        pts.iter()
            .enumerate()
            .map(|(i, q)| (i, pos2(x_of(q[0]), y_of(q[1])).distance(pos)))
            .filter(|(_, d)| *d < 14.0)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    };
    let mut changed = false;
    if resp.drag_started() {
        dragging = resp.interact_pointer_pos().and_then(|pp| nearest(pp, &curve.points));
    }
    if let (Some(i), Some(pp)) = (dragging, resp.interact_pointer_pos()) {
        if resp.dragged() && i < curve.points.len() {
            let fine = ui.input(|inp| inp.modifiers.shift);
            let snap = |v: f32| if fine { (v * 2.0).round() / 2.0 } else { v.round() };
            let lo_t = if i > 0 { curve.points[i - 1][0] + 1.0 } else { T_MIN };
            let hi_t = if i + 1 < curve.points.len() { curve.points[i + 1][0] - 1.0 } else { T_MAX };
            let nt = snap(t_of(pp.x)).clamp(lo_t, hi_t.max(lo_t));
            let nd = snap(d_of(pp.y));
            if curve.points[i] != [nt, nd] {
                curve.points[i] = [nt, nd];
                changed = true;
            }
        }
    }
    if resp.drag_stopped() {
        dragging = None;
    }
    ui.data_mut(|d| match dragging {
        Some(i) => {
            d.insert_temp(drag_id, i);
        }
        None => {
            d.remove::<usize>(drag_id);
        }
    });
    if resp.double_clicked() {
        if let Some(pp) = resp.interact_pointer_pos().filter(|pp| plot.contains(*pp)) {
            if nearest(pp, &curve.points).is_none() && curve.points.len() < 12 {
                let t = t_of(pp.x).round();
                if !curve.points.iter().any(|q| (q[0] - t).abs() < 1.0) {
                    curve.points.push([t, d_of(pp.y).round()]);
                    curve.sort();
                    changed = true;
                }
            }
        }
    }
    if resp.secondary_clicked() {
        if let Some(i) = resp.interact_pointer_pos().and_then(|pp| nearest(pp, &curve.points)) {
            if curve.points.len() > 2 {
                curve.points.remove(i);
                changed = true;
            }
        }
    }

    // Curve line + fill.
    let mut pts: Vec<Pos2> = Vec::new();
    if let Some(f) = curve.points.first() {
        pts.push(pos2(plot.left(), y_of(f[1])));
    }
    pts.extend(curve.points.iter().map(|q| pos2(x_of(q[0]), y_of(q[1]))));
    if let Some(l) = curve.points.last() {
        pts.push(pos2(plot.right(), y_of(l[1])));
    }
    area(&p, &pts, plot.bottom(), ACCENT, 0.22);
    p.add(Shape::line(pts, Stroke::new(2.0, ACCENT)));

    // Live operating point.
    if let Some(t) = live.temp {
        let x = x_of(t.clamp(T_MIN, T_MAX));
        dashed_v(&p, plot, x, temp_color(t).gamma_multiply(0.7));
        let d = live.target.unwrap_or(live.duty);
        let pt = pos2(x, y_of(curve.eval(t)));
        let pulse = (ui.input(|i| i.time) as f32 * 2.0).sin() * 0.5 + 0.5;
        p.circle_filled(pt, 6.0 + pulse * 4.0, ACCENT.gamma_multiply(0.15));
        p.circle_filled(pt, 4.5, ACCENT);
        p.circle_filled(pos2(x, y_of(live.duty)), 3.0, Color32::WHITE);
        let label = format!("{t:.1}°C → {d:.0}%");
        let lx = if x + 120.0 < plot.right() { x + 8.0 } else { x - 8.0 };
        let al = if x + 120.0 < plot.right() { Align2::LEFT_TOP } else { Align2::RIGHT_TOP };
        p.text(pos2(lx, plot.top() + 6.0), al, label, mono(11.0), TEXT);
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
    }

    // Handles.
    let hov_i = hover.and_then(|h| nearest(h, &curve.points));
    for (i, q) in curve.points.iter().enumerate() {
        let c = pos2(x_of(q[0]), y_of(q[1]));
        let active = dragging == Some(i) || hov_i == Some(i);
        p.circle_filled(c, if active { 8.0 } else { 6.0 }, Color32::BLACK);
        p.circle_stroke(c, if active { 7.0 } else { 5.0 }, Stroke::new(2.0, if active { Color32::WHITE } else { ACCENT }));
        if active {
            let txt = format!("{:.0}° · {:.0}%", q[0], q[1]);
            let tr = Rect::from_center_size(c - vec2(0.0, 22.0), vec2(text_w(ui, &txt, mono(11.0)) + 14.0, 20.0));
            p.rect_filled(tr, CornerRadius::same(5), SEL);
            p.text(tr.center(), Align2::CENTER_CENTER, txt, mono(11.0), TEXT);
        }
    }
    let cursor = if dragging.is_some() {
        egui::CursorIcon::Grabbing
    } else if hov_i.is_some() {
        egui::CursorIcon::Grab
    } else {
        egui::CursorIcon::Crosshair
    };
    if resp.hovered() || dragging.is_some() {
        ui.ctx().set_cursor_icon(cursor);
    }
    changed
}

fn dashed_h(p: &egui::Painter, plot: Rect, y: f32, c: Color32) {
    p.extend(Shape::dashed_line(&[pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(1.0, c), 4.0, 4.0));
}

fn dashed_v(p: &egui::Painter, plot: Rect, x: f32, c: Color32) {
    p.extend(Shape::dashed_line(&[pos2(x, plot.top()), pos2(x, plot.bottom())], Stroke::new(1.0, c), 4.0, 4.0));
}

/// RPM-vs-duty response from calibration.
pub fn rpm_chart(ui: &mut Ui, c: &Calibration, duty_now: f32, height: f32) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, height), Sense::hover());
    let plot = Rect::from_min_max(rect.min + vec2(40.0, 6.0), rect.max - vec2(8.0, 18.0));
    let p = ui.painter_at(rect);
    let max = (c.max_rpm * 1.1).max(500.0);
    let x_of = |d: f32| plot.left() + d / 100.0 * plot.width();
    let y_of = |r: f32| plot.bottom() - r / max * plot.height();
    p.hline(plot.x_range(), plot.bottom(), Stroke::new(1.0, BORDER));
    p.text(pos2(plot.left() - 6.0, y_of(c.max_rpm)), Align2::RIGHT_CENTER, format!("{:.0}", c.max_rpm), mono(9.5), FAINT);
    p.text(pos2(plot.left() - 6.0, plot.bottom()), Align2::RIGHT_CENTER, "0", mono(9.5), FAINT);
    for d in [0, 25, 50, 75, 100] {
        p.text(pos2(x_of(d as f32), plot.bottom() + 10.0), Align2::CENTER_CENTER, format!("{d}%"), mono(9.5), FAINT);
    }
    let mut t = c.table.clone();
    t.sort_by(|a, b| a[0].total_cmp(&b[0]));
    let pts: Vec<Pos2> = t.iter().map(|q| pos2(x_of(q[0]), y_of(q[1]))).collect();
    area(&p, &pts, plot.bottom(), VIOLET, 0.2);
    p.add(Shape::line(pts.clone(), Stroke::new(1.6, VIOLET)));
    for q in &pts {
        p.circle_filled(*q, 2.0, VIOLET);
    }
    let x = x_of(duty_now);
    p.vline(x, plot.y_range(), Stroke::new(1.0, Color32::WHITE.gamma_multiply(0.4)));
    if let Some(r) = c.rpm_at(duty_now) {
        p.circle_filled(pos2(x, y_of(r)), 3.5, Color32::WHITE);
    }
}

/// Circular duty gauge used on fan cards.
pub fn ring(ui: &mut Ui, center: Pos2, radius: f32, pct: f32, color: Color32) {
    let p = ui.painter();
    let n = 64;
    let start = std::f32::consts::PI * 0.75;
    let sweep = std::f32::consts::PI * 1.5;
    let arc = |frac: f32| -> Vec<Pos2> {
        let k = ((n as f32 * frac).ceil() as usize).max(2);
        (0..=k).map(|i| {
            let a = start + sweep * frac * i as f32 / k as f32;
            center + Vec2::angled(a) * radius
        }).collect()
    };
    p.add(Shape::line(arc(1.0), Stroke::new(4.0, WELL_HOVER)));
    let f = (pct / 100.0).clamp(0.0, 1.0);
    if f > 0.005 {
        p.add(Shape::line(arc(f), Stroke::new(4.0, color)));
    }
}
