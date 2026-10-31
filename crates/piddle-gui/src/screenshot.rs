//! Save the current view as a PNG (and copy it to the clipboard).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

/// Render `widget` as it appears on screen, at the display's scale factor,
/// and hand the result to `done`.
///
/// A new `WidgetPaintable` only holds an image after the widget's next frame,
/// so this requests a redraw and captures once the paintable has updated.
pub fn capture(widget: &impl IsA<gtk::Widget>, done: impl FnOnce(Result<gdk::Texture, String>) + 'static) {
    let widget = widget.as_ref().clone();
    let paintable = gtk::WidgetPaintable::new(Some(&widget));
    let done = Rc::new(RefCell::new(Some(done)));

    let finish = {
        let (widget, paintable, done) = (widget.clone(), paintable.clone(), done.clone());
        move || {
            if let Some(done) = done.borrow_mut().take() {
                done(render(&widget, &paintable));
            }
        }
    };
    let on_frame = finish.clone();
    paintable.connect_invalidate_contents(move |_| {
        // Let the frame finish before reading the paintable.
        let f = on_frame.clone();
        glib::idle_add_local_once(f);
    });
    // Fallback in case no frame arrives (e.g. the window is minimized).
    glib::timeout_add_local_once(Duration::from_millis(500), finish);
    widget.queue_draw();
}

fn render(widget: &gtk::Widget, paintable: &gtk::WidgetPaintable) -> Result<gdk::Texture, String> {
    let (w, h) = (widget.width() as f32, widget.height() as f32);
    if w <= 0.0 || h <= 0.0 {
        return Err("Nothing to capture yet.".into());
    }
    let scale = widget.scale_factor() as f32;

    let snap = gtk::Snapshot::new();
    snap.scale(scale, scale);
    paintable.snapshot(&snap, w as f64, h as f64);
    let node = snap.to_node().ok_or("The window has not been drawn yet; try again.")?;
    // A one-off capture renders in software: the result is a plain memory
    // texture, so saving it never round-trips through GPU drivers (whose
    // readback paths can print driver warnings).
    let renderer = gsk::CairoRenderer::new();
    renderer.realize_for_display(&widget.display()).map_err(|e| format!("Renderer: {e}"))?;
    let tex = renderer.render_texture(&node, Some(&graphene::Rect::new(0.0, 0.0, w * scale, h * scale)));
    renderer.unrealize();
    Ok(tex)
}

/// Save `texture` to ~/Pictures (or $HOME) with a timestamped name.
pub fn save(texture: &gdk::Texture) -> Result<PathBuf, String> {
    let dir = glib::user_special_dir(glib::UserDirectory::Pictures).unwrap_or_else(glib::home_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    let stamp = glib::DateTime::now_local()
        .ok()
        .and_then(|d| d.format("%Y-%m-%d %H-%M-%S").ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "screenshot".into());
    let mut path = dir.join(format!("PIDdle {stamp}.png"));
    for n in 2.. {
        if !path.exists() {
            break;
        }
        path = dir.join(format!("PIDdle {stamp} ({n}).png"));
    }
    texture.save_to_png(&path).map_err(|e| format!("Cannot write {}: {e}", path.display()))?;
    Ok(path)
}
