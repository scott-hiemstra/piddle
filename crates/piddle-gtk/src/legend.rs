//! Color and size legend, drawn at the canvas's current zoom so the size
//! references match on-screen bubbles exactly.

use std::f64::consts::TAU;

use gtk::{cairo, pango};

use piddle_core::encode::{cpu_color, fmt_bytes, mem_radius, CPU_SCALE_MAX};
use crate::view::font;

/// Draws the legend and returns the height it needs.
pub fn draw(cr: &cairo::Context, w: f64, scale: f64, ncpu: usize) -> f64 {
    let pad = 16.0;
    let mut y = 6.0;

    heading(cr, pad, y, "CPU LOAD  ·  COLOR");
    y += 24.0;

    let bw = w - pad * 2.0;
    let grad = cairo::LinearGradient::new(pad, 0.0, pad + bw, 0.0);
    for i in 0..=20 {
        let t = i as f64 / 20.0;
        let c = cpu_color(t * CPU_SCALE_MAX);
        grad.add_color_stop_rgb(t, c.0, c.1, c.2);
    }
    rounded_rect(cr, pad, y, bw, 12.0, 6.0);
    let _ = cr.set_source(&grad);
    let _ = cr.fill();
    y += 16.0;

    for (v, txt) in [(0.0, "0%"), (25.0, "25"), (50.0, "50"), (75.0, "75"), (100.0, "100%+")] {
        let x = pad + bw * v / CPU_SCALE_MAX;
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.35);
        cr.rectangle(x - 0.5, y - 3.0, 1.0, 4.0);
        let _ = cr.fill();
        let l = text(cr, txt, 10.5, false);
        let (lw, _) = l.pixel_size();
        let lx = (x - lw as f64 / 2.0).clamp(pad - 2.0, pad + bw - lw as f64 + 2.0);
        cr.set_source_rgba(0.8, 0.84, 0.9, 0.85);
        cr.move_to(lx, y + 2.0);
        pangocairo::functions::show_layout(cr, &l);
    }
    y += 22.0;

    let note = text(cr, 
        &format!("100% = one fully used core.\nThis system has {ncpu} cores, so a process can reach {}%.", ncpu * 100),
        10.5,
        false,
    );
    note.set_width(((bw) * pango::SCALE as f64) as i32);
    note.set_wrap(pango::WrapMode::WordChar);
    cr.set_source_rgba(0.7, 0.74, 0.82, 0.75);
    cr.move_to(pad, y);
    pangocairo::functions::show_layout(cr, &note);
    y += note.pixel_size().1 as f64 + 18.0;

    heading(cr, pad, y, "MEMORY (RSS)  ·  SIZE");
    y += 24.0;

    // Draw as many true-scale references as fit side by side.
    let refs: [u64; 4] = [10 << 20, 100 << 20, 1 << 30, 4 << 30];
    let gap = 10.0;
    let mut chosen: Vec<(u64, f64)> = Vec::new();
    let mut used = 0.0;
    for &bytes in &refs {
        let r = mem_radius(bytes) * scale;
        // Leave room for the caption under small circles.
        let need = (r * 2.0).max(46.0) + if chosen.is_empty() { 0.0 } else { gap };
        if used + need > bw || r > 70.0 {
            break;
        }
        used += need;
        chosen.push((bytes, r));
    }
    let maxr = chosen.iter().map(|c| c.1).fold(0.0, f64::max);
    let base = y + maxr * 2.0;
    let mut x = pad + (bw - used) / 2.0;
    for (bytes, r) in &chosen {
        let cx = x + r.max(23.0);
        cr.new_path();
        cr.arc(cx, base - r, *r, 0.0, TAU);
        cr.set_source_rgba(0.3, 0.5, 1.0, 0.25);
        let _ = cr.fill_preserve();
        cr.set_source_rgba(0.55, 0.7, 1.0, 0.7);
        cr.set_line_width(1.0);
        let _ = cr.stroke();
        let l = text(cr, &fmt_bytes(*bytes).replace(".0 ", " ").replace(".00 ", " "), 10.5, false);
        let (lw, _) = l.pixel_size();
        cr.set_source_rgba(0.8, 0.84, 0.9, 0.85);
        cr.move_to(cx - lw as f64 / 2.0, base + 4.0);
        pangocairo::functions::show_layout(cr, &l);
        x += (r * 2.0).max(46.0) + gap;
    }
    y = base + 24.0;

    let note = text(cr, "Bubble area grows with resident memory. References are shown at the current zoom.", 10.5, false);
    note.set_width((bw * pango::SCALE as f64) as i32);
    note.set_wrap(pango::WrapMode::WordChar);
    cr.set_source_rgba(0.7, 0.74, 0.82, 0.75);
    cr.move_to(pad, y);
    pangocairo::functions::show_layout(cr, &note);
    y += note.pixel_size().1 as f64 + 8.0;

    y
}

fn heading(cr: &cairo::Context, x: f64, y: f64, s: &str) {
    let l = text(cr, s, 10.5, true);
    cr.set_source_rgba(0.62, 0.68, 0.8, 0.9);
    cr.move_to(x, y);
    pangocairo::functions::show_layout(cr, &l);
}

fn text(cr: &cairo::Context, s: &str, px: f64, bold: bool) -> pango::Layout {
    let l = new_layout(cr, s);
    l.set_font_description(Some(&font(px, bold)));
    l
}

fn new_layout(cr: &cairo::Context, s: &str) -> pango::Layout {
    let l = pangocairo::functions::create_layout(cr);
    l.set_text(s);
    l
}

fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::PI;
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}
