mod canvas;
mod details;
mod journal;
mod legend;
mod screenshot;
mod view;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::Ordering;

use gtk::prelude::*;
use gtk::{gdk, glib};

use canvas::Canvas;
use piddle_core::encode::{self, fmt_bytes, fmt_cpu};
use piddle_core::sampler::{self, Control};
use view::{Layout, Motion, View};

const APP_ID: &str = "io.github.piddle";

// Loaded above user priority on purpose: desktops such as KDE inject a light
// color scheme via ~/.config/gtk-4.0/gtk.css, which would otherwise leave
// light-theme text on PIDdle's dark surfaces. The *_breeze names cover KDE's
// Breeze theme; the explicit rules below keep the header readable elsewhere.
const CSS: &str = r#"
@define-color theme_bg_color_breeze #151820;
@define-color theme_fg_color_breeze #e6e9ef;
@define-color theme_base_color_breeze #0f1218;
@define-color theme_text_color_breeze #e6e9ef;
@define-color theme_selected_bg_color_breeze #3b82f6;
@define-color theme_selected_fg_color_breeze #ffffff;
@define-color theme_unfocused_bg_color_breeze #151820;
@define-color theme_unfocused_fg_color_breeze #c9cfdb;
@define-color theme_unfocused_base_color_breeze #0f1218;
@define-color theme_unfocused_text_color_breeze #c9cfdb;
@define-color theme_unfocused_view_bg_color_breeze #0f1218;
@define-color theme_unfocused_view_text_color_breeze #c9cfdb;
@define-color theme_unfocused_selected_bg_color_breeze #2c5fb8;
@define-color theme_unfocused_selected_bg_color_alt_breeze #2c5fb8;
@define-color theme_unfocused_selected_fg_color_breeze #ffffff;
@define-color theme_hovering_selected_bg_color_breeze #3b82f6;
@define-color content_view_bg_breeze #0f1218;
@define-color theme_button_background_normal_breeze #232834;
@define-color theme_button_background_backdrop_breeze #20242e;
@define-color theme_button_background_insensitive_breeze #1a1e26;
@define-color theme_button_background_backdrop_insensitive_breeze #1a1e26;
@define-color theme_button_foreground_normal_breeze #e6e9ef;
@define-color theme_button_foreground_backdrop_breeze #c9cfdb;
@define-color theme_button_foreground_active_breeze #ffffff;
@define-color theme_button_foreground_active_backdrop_breeze #e6e9ef;
@define-color theme_button_foreground_insensitive_breeze #6b7280;
@define-color theme_button_foreground_backdrop_insensitive_breeze #6b7280;
@define-color theme_button_foreground_active_insensitive_breeze #6b7280;
@define-color theme_button_foreground_active_backdrop_insensitive_breeze #6b7280;
@define-color theme_header_background_breeze #12151c;
@define-color theme_header_background_backdrop_breeze #12151c;
@define-color theme_header_background_light_breeze #1a1e27;
@define-color theme_header_foreground_breeze #e6e9ef;
@define-color theme_header_foreground_backdrop_breeze #c9cfdb;
@define-color theme_header_foreground_insensitive_breeze #6b7280;
@define-color theme_header_foreground_insensitive_backdrop_breeze #6b7280;
@define-color theme_titlebar_background_breeze #12151c;
@define-color theme_titlebar_background_backdrop_breeze #12151c;
@define-color theme_titlebar_background_light_breeze #1a1e27;
@define-color theme_titlebar_foreground_breeze #e6e9ef;
@define-color theme_titlebar_foreground_backdrop_breeze #c9cfdb;
@define-color theme_titlebar_foreground_insensitive_breeze #6b7280;
@define-color theme_titlebar_foreground_insensitive_backdrop_breeze #6b7280;
@define-color borders_breeze #2c3240;
@define-color unfocused_borders_breeze #2c3240;
@define-color insensitive_borders_breeze #232834;
@define-color unfocused_insensitive_borders_breeze #232834;
@define-color insensitive_bg_color_breeze #1a1e26;
@define-color insensitive_fg_color_breeze #6b7280;
@define-color insensitive_base_color_breeze #141820;
@define-color insensitive_base_fg_color_breeze #6b7280;
@define-color insensitive_selected_bg_color_breeze #232834;
@define-color insensitive_selected_fg_color_breeze #6b7280;
@define-color insensitive_unfocused_bg_color_breeze #1a1e26;
@define-color insensitive_unfocused_fg_color_breeze #6b7280;
@define-color insensitive_unfocused_selected_bg_color_breeze #232834;
@define-color insensitive_unfocused_selected_fg_color_breeze #6b7280;
@define-color tooltip_background_breeze #232834;
@define-color tooltip_text_breeze #e6e9ef;
@define-color tooltip_border_breeze #3a4150;
@define-color theme_view_hover_decoration_color_breeze #3b82f6;
@define-color theme_view_active_decoration_color_breeze #3b82f6;
@define-color theme_bg_color #151820;
@define-color theme_fg_color #e6e9ef;
@define-color theme_base_color #0f1218;
@define-color theme_text_color #e6e9ef;
@define-color theme_selected_bg_color #3b82f6;
@define-color theme_selected_fg_color #ffffff;
@define-color borders #2c3240;
@define-color insensitive_fg_color #6b7280;
@define-color insensitive_bg_color #1a1e26;

window, .main { background: #0b0d12; color: #e6e9ef; }
headerbar, headerbar label { color: #e6e9ef; }
headerbar button, headerbar dropdown > button, headerbar menubutton > button {
  background: #1d222c; color: #e6e9ef; border: 1px solid rgba(255,255,255,0.10);
  border-radius: 7px; box-shadow: none; padding: 4px 10px; min-height: 26px; margin: 0;
}
headerbar button:hover, headerbar dropdown > button:hover { background: #272d3a; }
headerbar button:checked { background: #3b82f6; color: #ffffff; border-color: #3b82f6; }
headerbar button:checked:hover { background: #4b8ff7; }
headerbar windowcontrols button { background: transparent; border: none; padding: 4px; min-height: 0; margin: 0; }
headerbar windowcontrols button:hover { background: rgba(255,255,255,0.10); }
headerbar button image, headerbar dropdown arrow { color: #e6e9ef; -gtk-icon-style: symbolic; }
headerbar searchentry, headerbar entry {
  background: #0f1218; color: #e6e9ef; border: 1px solid rgba(255,255,255,0.12); border-radius: 7px;
  box-shadow: none; min-height: 28px;
}
headerbar searchentry:focus-within { border-color: #3b82f6; }
headerbar searchentry text, headerbar searchentry image { color: #e6e9ef; }
headerbar searchentry text placeholder { color: rgba(230,233,239,0.45); }
popover > contents, popover > arrow { background: #1a1e27; color: #e6e9ef; border: 1px solid rgba(255,255,255,0.10); }
popover listview row, popover row { color: #e6e9ef; }
popover row:selected, popover row:hover { background: rgba(59,130,246,0.25); }
tooltip, tooltip.background { background: #232834; color: #e6e9ef; }
scrollbar, scrollbar trough { background: transparent; }
scrollbar slider { background: rgba(255,255,255,0.18); min-width: 6px; min-height: 6px; border-radius: 3px; margin: 0; border: none; }
window.details, .details { background: #0f1218; color: #e6e9ef; }
.details headerbar { background: #0f1218; border-bottom: 1px solid rgba(255,255,255,0.05); }
.details-title { font-size: 21px; font-weight: 800; }
.details-subtitle { color: rgba(200,210,230,0.65); font-feature-settings: "tnum"; }
.pill { background: rgba(255,255,255,0.07); border: 1px solid rgba(255,255,255,0.10); border-radius: 999px;
  padding: 3px 10px; font-size: 11px; font-weight: 600; color: rgba(220,226,238,0.9); }
.details-card { background: #161a22; border: 1px solid rgba(255,255,255,0.06); border-radius: 10px; padding: 12px 14px; }
.card-label { font-size: 10px; font-weight: 700; letter-spacing: 1px; color: rgba(158,173,204,0.9); }
.card-sub { font-size: 11px; color: rgba(200,210,230,0.55); }
.big-num { font-size: 22px; font-weight: 800; font-feature-settings: "tnum"; margin-top: 2px; }
.info-key { color: rgba(200,210,230,0.55); }
.info-val { color: #e6e9ef; }
.cmdline-box { background: #0a0c11; border: 1px solid rgba(255,255,255,0.08); border-radius: 8px; }
.cmdline { font-family: monospace; font-size: 11.5px; color: #c9d4e5; padding: 8px 10px; }
.mono { font-family: monospace; }
expander-widget title > expander { color: rgba(158,173,204,0.9); min-width: 16px; min-height: 16px; }
expander-widget title:hover > expander, expander-widget title:hover > label { color: #e6edf7; }
textview.journal, textview.journal text { background: transparent; color: #c9d4e5; font-size: 11.5px; }
.details-status { color: rgba(200,210,230,0.7); font-size: 12px; }
button.action { background: #1d222c; color: #e6e9ef; border: 1px solid rgba(255,255,255,0.12); border-radius: 8px;
  padding: 6px 16px; box-shadow: none; margin: 0; }
button.action:hover { background: #272d3a; }
button.action.destructive-action { background: #b3261e; border-color: #c9372d; color: #ffffff; }
button.action.destructive-action:hover { background: #c9372d; }
button.action:disabled { opacity: 0.4; }
.dim-label { color: rgba(200,210,230,0.6); }
.risk-banner { background: rgba(245,158,11,0.10); border: 1px solid rgba(245,158,11,0.35); border-radius: 8px;
  padding: 9px 12px; color: #f5c56b; }
.confirm-heading { font-size: 16px; font-weight: 800; }
.confirm-entry { background: #0a0c11; color: #e6e9ef; border: 1px solid rgba(255,255,255,0.15); border-radius: 7px; min-height: 30px; }
.confirm-entry:focus-within { border-color: #c9372d; }
.confirm-entry text placeholder { color: rgba(230,233,239,0.3); font-style: italic; }
headerbar { background: #12151c; border-bottom: 1px solid rgba(255,255,255,0.06); box-shadow: none; min-height: 46px; }
headerbar .title { font-weight: 800; letter-spacing: 0.5px; }
.brand { font-weight: 800; font-size: 15px; letter-spacing: 0.5px; }
.brand-accent { color: #f2c94c; }
.sidebar { background: #10131a; border-left: 1px solid rgba(255,255,255,0.06); }
.section { font-size: 10.5px; font-weight: 700; color: rgba(158,173,204,0.9); letter-spacing: 1px; margin-top: 6px; }
.stat-key { color: rgba(200,210,230,0.6); }
.stat-val { font-weight: 600; font-feature-settings: "tnum"; }
.top-row { padding: 3px 6px; border-radius: 6px; }
.top-row:hover { background: rgba(255,255,255,0.06); }
searchentry { min-width: 260px; }
.matches { color: rgba(200,210,230,0.65); font-size: 12px; }
.toast { background: rgba(22,26,34,0.97); color: #e6e9ef; border: 1px solid rgba(255,255,255,0.12);
  border-radius: 999px; padding: 6px 6px 6px 16px; box-shadow: 0 6px 24px rgba(0,0,0,0.45); }
.toast button { background: #2a3140; color: #e6e9ef; border: none; border-radius: 999px; padding: 4px 12px;
  box-shadow: none; margin: 0; min-height: 0; }
.toast button:hover { background: #343c4e; }
"#;

fn main() -> glib::ExitCode {
    silence_foreign_config_warning();
    // Headless render for testing/docs: piddle-gui --snapshot out.png [free|app|service|parent|tree] [query]
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--snapshot") {
        let path = args.get(2).map(String::as_str).unwrap_or("piddle.png");
        let layout = match args.get(3).map(String::as_str) {
            Some("app") => Layout::App,
            Some("service") => Layout::Service,
            Some("parent") => Layout::Parent,
            Some("tree") => Layout::Tree,
            _ => Layout::Free,
        };
        snapshot(path, layout, args.get(4).map(String::as_str).unwrap_or(""));
        return glib::ExitCode::SUCCESS;
    }
    if let Err(e) = gtk::init() {
        eprintln!("piddle-gui: cannot start the graphical interface: {e}");
        std::process::exit(piddle_core::GUI_UNAVAILABLE);
    }
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        if let Some(s) = gtk::Settings::default() {
            s.set_gtk_application_prefer_dark_theme(true);
        }
        let css = gtk::CssProvider::new();
        css.load_from_string(CSS);
        gtk::style_context_add_provider_for_display(
            &gdk::Display::default().expect("display"),
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_USER + 1,
        );
    });
    app.connect_activate(build_ui);
    app.run()
}

/// KDE writes the GTK3-only `gtk-modules` key into ~/.config/gtk-4.0/settings.ini,
/// so every GTK4 app warns about it on startup. Drop exactly that message and
/// pass everything else to GLib's default writer.
fn silence_foreign_config_warning() {
    glib::log_set_writer_func(|level, fields| {
        let noise = fields.iter().any(|f| {
            f.key() == "MESSAGE" && f.value_str().is_some_and(|m| m.starts_with("Unknown key gtk-modules in "))
        });
        if noise { glib::LogWriterOutput::Handled } else { glib::log_writer_default(level, fields) }
    });
}

struct Animator {
    canvas: Canvas,
    view: Rc<RefCell<View>>,
    ticking: Cell<bool>,
    last_frame: Cell<i64>,
    last_draw: Cell<i64>,
}

/// Minimum time between redraws while only colors are easing (~20 fps).
const SUBTLE_FRAME_US: i64 = 50_000;

impl Animator {
    /// Run the frame-clock driven animation until the scene comes to rest.
    /// When idle nothing is redrawn, which keeps PIDdle's own overhead low.
    fn kick(self: &Rc<Self>) {
        if self.ticking.replace(true) {
            return;
        }
        self.last_frame.set(0);
        let this = self.clone();
        self.canvas.add_tick_callback(move |w, clock| {
            let t = clock.frame_time();
            let last = this.last_frame.replace(t);
            let dt = if last == 0 { 1.0 / 60.0 } else { (t - last) as f64 / 1e6 };
            let motion = this.view.borrow_mut().step(dt);
            // Full frame rate for movement; color-only easing redraws at a
            // reduced rate since that is where most frames would otherwise go.
            if motion != Motion::Subtle || t - this.last_draw.get() >= SUBTLE_FRAME_US {
                this.last_draw.set(t);
                w.queue_draw();
            }
            if motion == Motion::Idle {
                this.ticking.set(false);
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }
}

fn build_ui(app: &gtk::Application) {
    let control = Control::new(500);
    let rx = sampler::spawn(control.clone());
    let view = Rc::new(RefCell::new(View::new()));

    // ---------------------------------------------------------------- header
    let header = gtk::HeaderBar::new();
    let brand = gtk::Label::new(None);
    brand.set_markup("PID<span foreground='#f2c94c'>dle</span>");
    brand.add_css_class("brand");
    header.set_title_widget(Some(&brand));

    let search = gtk::SearchEntry::builder().placeholder_text("Search name, PID or command…").build();
    let matches = gtk::Label::new(None);
    matches.add_css_class("matches");
    header.pack_start(&search);
    header.pack_start(&matches);

    let layout_dd = gtk::DropDown::from_strings(&["Free", "By application", "By service", "By parent", "Process tree"]);
    layout_dd.set_tooltip_text(Some("Arrange bubbles"));
    let interval_dd = gtk::DropDown::from_strings(&["250 ms", "500 ms", "1 s", "2 s"]);
    interval_dd.set_selected(1);
    interval_dd.set_tooltip_text(Some("Sampling interval"));
    let kernel_btn = gtk::ToggleButton::with_label("Kernel");
    kernel_btn.set_tooltip_text(Some("Show kernel threads"));
    let active_btn = gtk::ToggleButton::with_label("Active only");
    active_btn.set_tooltip_text(Some("Only show processes that used CPU in the last few seconds"));
    let pause_btn = gtk::ToggleButton::new();
    pause_btn.set_icon_name("media-playback-pause-symbolic");
    pause_btn.set_tooltip_text(Some("Pause sampling"));

    let shot_btn = gtk::Button::from_icon_name("camera-photo-symbolic");
    shot_btn.set_tooltip_text(Some("Save a screenshot to Pictures and copy it (Ctrl+S)"));
    shot_btn.set_action_name(Some("win.screenshot"));

    header.pack_end(&shot_btn);
    header.pack_end(&pause_btn);
    header.pack_end(&interval_dd);
    header.pack_end(&active_btn);
    header.pack_end(&kernel_btn);
    header.pack_end(&layout_dd);

    // ---------------------------------------------------------------- canvas
    let canvas = Canvas::new(view.clone());
    canvas.set_hexpand(true);
    canvas.set_vexpand(true);
    canvas.set_focusable(true);
    let anim = Rc::new(Animator {
        canvas: canvas.clone(),
        view: view.clone(),
        ticking: Cell::new(false),
        last_frame: Cell::new(0),
        last_draw: Cell::new(0),
    });
    canvas.connect_resized(glib::clone!(
        #[weak]
        anim,
        move || anim.kick()
    ));

    // ---------------------------------------------------------------- sidebar
    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 4);
    sidebar.add_css_class("sidebar");
    sidebar.set_size_request(280, -1);

    let legend_area = gtk::DrawingArea::new();
    legend_area.set_content_height(300);
    legend_area.set_margin_top(12);
    let legend_scale = Rc::new(Cell::new(1.0f64));
    let legend_ncpu = Rc::new(Cell::new(1usize));
    legend_area.set_draw_func(glib::clone!(
        #[strong]
        legend_scale,
        #[strong]
        legend_ncpu,
        move |w, cr, width, _| {
            let want = legend::draw(cr, width as f64, legend_scale.get(), legend_ncpu.get()).ceil() as i32;
            if w.content_height() != want {
                w.set_content_height(want);
            }
        }
    ));
    sidebar.append(&legend_area);

    let stats_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    stats_box.set_margin_start(16);
    stats_box.set_margin_end(16);
    let sec = gtk::Label::new(Some("SYSTEM"));
    sec.add_css_class("section");
    sec.set_xalign(0.0);
    stats_box.append(&sec);
    let grid = gtk::Grid::builder().row_spacing(4).column_spacing(12).build();
    let mut stat_vals = Vec::new();
    for (i, k) in ["CPU (all cores)", "Memory", "Processes", "Threads", "Sample cost", "PIDdle CPU"].iter().enumerate()
    {
        let kl = gtk::Label::new(Some(k));
        kl.add_css_class("stat-key");
        kl.set_xalign(0.0);
        kl.set_hexpand(true);
        let vl = gtk::Label::new(Some("–"));
        vl.add_css_class("stat-val");
        vl.set_xalign(1.0);
        grid.attach(&kl, 0, i as i32, 1, 1);
        grid.attach(&vl, 1, i as i32, 1, 1);
        stat_vals.push(vl);
    }
    stats_box.append(&grid);

    let sec = gtk::Label::new(Some("TOP CPU"));
    sec.add_css_class("section");
    sec.set_xalign(0.0);
    sec.set_margin_top(14);
    stats_box.append(&sec);
    let top_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    stats_box.append(&top_box);
    sidebar.append(&stats_box);

    let sidebar_scroll =
        gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&sidebar).build();
    sidebar_scroll.add_css_class("sidebar");
    // Keep the sidebar a fixed column; expanding stat labels must not widen it.
    sidebar_scroll.set_hexpand(false);
    sidebar_scroll.set_propagate_natural_width(false);
    sidebar_scroll.set_size_request(290, -1);

    // Transient notice shown after a screenshot, floating over the canvas.
    let toast_label = gtk::Label::new(None);
    toast_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    toast_label.set_max_width_chars(60);
    let toast_open = gtk::Button::with_label("Show in folder");
    let toast_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    toast_box.add_css_class("toast");
    toast_box.append(&toast_label);
    toast_box.append(&toast_open);
    let toast = gtk::Revealer::builder()
        .child(&toast_box)
        .transition_type(gtk::RevealerTransitionType::SlideUp)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::End)
        .margin_bottom(20)
        .build();
    let canvas_overlay = gtk::Overlay::new();
    canvas_overlay.set_child(Some(&canvas));
    canvas_overlay.add_overlay(&toast);

    let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    body.add_css_class("main");
    body.append(&canvas_overlay);
    body.append(&sidebar_scroll);

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("PIDdle")
        .default_width(1360)
        .default_height(860)
        .child(&body)
        .build();
    window.set_titlebar(Some(&header));

    // ---------------------------------------------------------------- screenshot
    let last_shot: Rc<RefCell<Option<std::path::PathBuf>>> = Rc::default();
    let toast_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
    let show_toast = {
        let (toast, toast_label, toast_open, toast_timer) =
            (toast.clone(), toast_label.clone(), toast_open.clone(), toast_timer.clone());
        move |text: &str, can_open: bool| {
            toast_label.set_text(text);
            toast_open.set_visible(can_open);
            toast.set_visible(true);
            toast.set_reveal_child(true);
            if let Some(id) = toast_timer.borrow_mut().take() {
                id.remove();
            }
            let (toast, timer) = (toast.clone(), toast_timer.clone());
            *toast_timer.borrow_mut() =
                Some(glib::timeout_add_local_once(std::time::Duration::from_secs(4), move || {
                    timer.borrow_mut().take();
                    toast.set_reveal_child(false);
                }));
        }
    };
    let shot_action = gtk::gio::SimpleAction::new("screenshot", None);
    shot_action.connect_activate(glib::clone!(
        #[weak]
        body,
        #[weak]
        toast,
        #[strong]
        last_shot,
        move |_, _| {
            // Keep a previous notice out of the picture.
            toast.set_visible(false);
            let (body2, last_shot, show_toast) = (body.clone(), last_shot.clone(), show_toast.clone());
            screenshot::capture(&body, move |result| {
                match result.and_then(|tex| {
                    body2.clipboard().set_texture(&tex);
                    screenshot::save(&tex)
                }) {
                    Ok(path) => {
                        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        show_toast(&format!("Saved “{name}” to Pictures · copied to clipboard"), true);
                        *last_shot.borrow_mut() = Some(path);
                    }
                    Err(e) => {
                        eprintln!("PIDdle: screenshot failed: {e}");
                        show_toast(&format!("Screenshot failed: {e}"), false);
                    }
                }
            });
        }
    ));
    window.add_action(&shot_action);
    app.set_accels_for_action("win.screenshot", &["<Control>s"]);
    toast_open.connect_clicked(glib::clone!(
        #[weak]
        window,
        #[strong]
        last_shot,
        move |_| {
            if let Some(path) = last_shot.borrow().as_ref() {
                gtk::FileLauncher::new(Some(&gtk::gio::File::for_path(path))).open_containing_folder(
                    Some(&window),
                    None::<&gtk::gio::Cancellable>,
                    |_| {},
                );
            }
        }
    ));

    // ---------------------------------------------------------------- input
    let motion = gtk::EventControllerMotion::new();
    motion.connect_motion(glib::clone!(
        #[strong]
        anim,
        move |_, x, y| {
            let mut v = anim.view.borrow_mut();
            v.mouse = (x, y);
            let h = v.pick(x, y);
            let changed = h != v.hover;
            v.hover = h;
            drop(v);
            anim.canvas.set_cursor_from_name(if h.is_some() { Some("pointer") } else { None });
            if changed || h.is_some() {
                anim.canvas.queue_draw();
            }
        }
    ));
    motion.connect_leave(glib::clone!(
        #[strong]
        anim,
        move |_| {
            anim.view.borrow_mut().hover = None;
            anim.canvas.queue_draw();
        }
    ));
    canvas.add_controller(motion);

    let click = gtk::GestureClick::new();
    click.connect_released(glib::clone!(
        #[strong]
        view,
        #[weak]
        window,
        move |_, _, x, y| {
            let pid = view.borrow().pick(x, y);
            if let Some(pid) = pid {
                details::open(&window, &view, pid);
            }
        }
    ));
    canvas.add_controller(click);

    let update_matches = {
        let view = view.clone();
        let matches = matches.clone();
        move || {
            let v = view.borrow();
            matches.set_text(&if v.has_query() {
                format!("{} match{}", v.matches, if v.matches == 1 { "" } else { "es" })
            } else {
                String::new()
            });
        }
    };

    search.connect_search_changed(glib::clone!(
        #[strong]
        anim,
        #[strong]
        update_matches,
        move |e| {
            anim.view.borrow_mut().set_query(&e.text());
            update_matches();
            anim.canvas.queue_draw();
        }
    ));
    search.connect_stop_search(|e| e.set_text(""));
    search.set_key_capture_widget(Some(&window));

    layout_dd.connect_selected_notify(glib::clone!(
        #[strong]
        anim,
        move |d| {
            let l = match d.selected() {
                1 => Layout::App,
                2 => Layout::Service,
                3 => Layout::Parent,
                4 => Layout::Tree,
                _ => Layout::Free,
            };
            anim.view.borrow_mut().set_layout(l);
            anim.kick();
        }
    ));
    interval_dd.connect_selected_notify(glib::clone!(
        #[strong]
        control,
        move |d| {
            let ms = [250, 500, 1000, 2000][d.selected().min(3) as usize];
            control.interval_ms.store(ms, Ordering::Relaxed);
        }
    ));
    kernel_btn.connect_toggled(glib::clone!(
        #[strong]
        anim,
        move |b| {
            let mut v = anim.view.borrow_mut();
            v.show_kernel = b.is_active();
            v.refilter();
            drop(v);
            anim.kick();
        }
    ));
    active_btn.connect_toggled(glib::clone!(
        #[strong]
        anim,
        move |b| {
            let mut v = anim.view.borrow_mut();
            v.active_only = b.is_active();
            v.refilter();
            drop(v);
            anim.kick();
        }
    ));
    pause_btn.connect_toggled(glib::clone!(
        #[strong]
        control,
        move |b| {
            control.paused.store(b.is_active(), Ordering::Relaxed);
            b.set_icon_name(if b.is_active() {
                "media-playback-start-symbolic"
            } else {
                "media-playback-pause-symbolic"
            });
            b.set_tooltip_text(Some(if b.is_active() { "Resume sampling" } else { "Pause sampling" }));
        }
    ));

    // ---------------------------------------------------------------- data
    glib::spawn_future_local(glib::clone!(
        #[strong]
        anim,
        #[strong]
        view,
        #[weak]
        window,
        async move {
            let mut top_rows: Vec<gtk::Button> = Vec::new();
            while let Ok(snap) = rx.recv().await {
                view.borrow_mut().apply(snap);
                update_matches();
                anim.kick();

                let v = view.borrow();
                let m = v.meta;
                stat_vals[0].set_text(&format!("{:.1}%", m.total_cpu));
                stat_vals[1].set_text(&format!("{} / {}", fmt_bytes(m.mem_used), fmt_bytes(m.mem_total)));
                stat_vals[2].set_text(&m.procs.to_string());
                stat_vals[3].set_text(&m.threads.to_string());
                stat_vals[4].set_text(&format!("{:.1} ms", m.sample_cost.as_secs_f64() * 1000.0));
                stat_vals[5].set_text(&fmt_cpu(m.self_cpu as f64));

                while top_rows.len() < v.top.len() {
                    let btn = gtk::Button::new();
                    btn.add_css_class("flat");
                    btn.add_css_class("top-row");
                    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                    let name = gtk::Label::new(None);
                    name.set_xalign(0.0);
                    name.set_hexpand(true);
                    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
                    let val = gtk::Label::new(None);
                    val.add_css_class("stat-val");
                    row.append(&name);
                    row.append(&val);
                    btn.set_child(Some(&row));
                    btn.connect_clicked(glib::clone!(
                        #[strong]
                        view,
                        #[weak]
                        window,
                        move |b| {
                            let pid: i32 = b.widget_name().parse().unwrap_or(0);
                            if view.borrow().bubble(pid).is_some() {
                                details::open(&window, &view, pid);
                            }
                        }
                    ));
                    top_box.append(&btn);
                    top_rows.push(btn);
                }
                for (btn, (name, pid, cpu)) in top_rows.iter().zip(&v.top) {
                    btn.set_widget_name(&pid.to_string());
                    let row = btn.child().and_downcast::<gtk::Box>().unwrap();
                    let nl = row.first_child().and_downcast::<gtk::Label>().unwrap();
                    let vl = row.last_child().and_downcast::<gtk::Label>().unwrap();
                    nl.set_markup(&format!("{}  <span alpha='45%'>{}</span>", view::glib_escape(name), pid));
                    vl.set_text(&fmt_cpu(*cpu as f64));
                    let c = encode::cpu_color(*cpu as f64);
                    vl.set_markup(&format!(
                        "<span foreground='#{:02x}{:02x}{:02x}'>{}</span>",
                        (c.0 * 255.0) as u8,
                        (c.1 * 255.0) as u8,
                        (c.2 * 255.0) as u8,
                        fmt_cpu(*cpu as f64)
                    ));
                }

                let scale = v.scale();
                if (legend_scale.get() - scale).abs() / scale.max(1e-6) > 0.02 || legend_ncpu.get() != m.ncpu {
                    legend_scale.set(scale);
                    legend_ncpu.set(m.ncpu);
                    legend_area.queue_draw();
                }
            }
        }
    ));

    window.present();
    canvas.grab_focus();
}

fn snapshot(path: &str, layout: Layout, query: &str) {
    gtk::init().expect("GTK init (needs a display)");
    let control = Control::new(400);
    let rx = sampler::spawn(control);
    let (w, h) = (1100.0, 760.0);
    let mut v = View::new();
    v.size = (w, h);
    v.set_layout(layout);
    v.set_query(query);
    for _ in 0..4 {
        v.apply(rx.recv_blocking().expect("sampler"));
        for _ in 0..30 {
            v.step(1.0 / 60.0);
        }
    }
    let mut frames = 0;
    while v.step(1.0 / 60.0) != Motion::Idle && frames < 1500 {
        frames += 1;
    }
    eprintln!("settled after {frames} extra frames, {} bubbles", v.bubbles.len());
    if query.is_empty()
        && let Some(b) = v.bubbles.iter().max_by(|a, b| a.info.cpu.total_cmp(&b.info.cpu))
    {
        v.hover = Some(b.info.pid);
        v.mouse = v.screen_pos(b.info.pid).unwrap_or((w / 2.0, h / 2.0));
    }

    let snap = gtk::Snapshot::new();
    let pctx = pangocairo::FontMap::default().create_context();
    v.render(&snap, &pctx);
    let node = snap.to_node().expect("render node");
    let renderer = gtk::gsk::CairoRenderer::new();
    renderer.realize_for_display(&gdk::Display::default().expect("display")).expect("renderer");
    let tex = renderer.render_texture(&node, Some(&gtk::graphene::Rect::new(0.0, 0.0, w as f32, h as f32)));
    renderer.unrealize();
    tex.save_to_png(path).expect("write png");
}
