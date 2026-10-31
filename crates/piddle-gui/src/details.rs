//! Process details window with confirmed terminate / kill actions.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::f64::consts::TAU;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gtk::prelude::*;
use gtk::{gio, glib};

use crate::view::View;
use piddle_core::actions::{Assessment, Signal, assess, exe_path, fmt_age, send_signal, state_name};
use piddle_core::encode::{cpu_color, fmt_bytes, fmt_cpu, fmt_rate};
use piddle_core::net;
use piddle_core::sampler::io_totals;

/// Seconds of CPU history shown in the graph (one point per refresh).
const HISTORY: usize = 120;
const REFRESH: Duration = Duration::from_millis(500);

pub fn open(parent: &gtk::ApplicationWindow, view: &Rc<RefCell<View>>, pid: i32) {
    let Some(info) = view.borrow().proc_info(pid).cloned() else { return };
    let start_ticks = info.start_ticks;

    let header = gtk::HeaderBar::new();
    let title = gtk::Label::new(Some("Process details"));
    title.add_css_class("title");
    header.set_title_widget(Some(&title));

    let win = gtk::Window::builder()
        .title(format!("{} — PID {}", info.name, pid))
        .transient_for(parent)
        .default_width(640)
        .titlebar(&header)
        .build();
    win.add_css_class("details");

    let root = gtk::Box::new(gtk::Orientation::Vertical, 16);
    root.set_margin_top(18);
    root.set_margin_bottom(18);
    root.set_margin_start(20);
    root.set_margin_end(20);

    // ---- identity: CPU-colored badge, name, subtitle
    let cpu_now = Rc::new(std::cell::Cell::new(info.cpu as f64));
    let badge = gtk::DrawingArea::builder().content_width(44).content_height(44).valign(gtk::Align::Center).build();
    badge.set_draw_func(glib::clone!(
        #[strong]
        cpu_now,
        move |_, cr, w, h| {
            let c = cpu_color(cpu_now.get());
            let (cx, cy, r) = (w as f64 / 2.0, h as f64 / 2.0, w.min(h) as f64 / 2.0 - 1.0);
            let g = gtk::cairo::RadialGradient::new(cx - r * 0.35, cy - r * 0.4, r * 0.05, cx, cy, r);
            g.add_color_stop_rgb(0.0, c.0 + (1.0 - c.0) * 0.35, c.1 + (1.0 - c.1) * 0.35, c.2 + (1.0 - c.2) * 0.35);
            g.add_color_stop_rgb(1.0, c.0 * 0.75, c.1 * 0.75, c.2 * 0.75);
            let _ = cr.set_source(&g);
            cr.arc(cx, cy, r, 0.0, TAU);
            let _ = cr.fill();
        }
    ));
    let name = gtk::Label::builder().label(&*info.name).xalign(0.0).selectable(true).build();
    name.add_css_class("details-title");
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let subtitle = gtk::Label::builder().xalign(0.0).build();
    subtitle.add_css_class("details-subtitle");
    let state_pill = gtk::Label::new(None);
    state_pill.add_css_class("pill");
    let names = gtk::Box::new(gtk::Orientation::Vertical, 2);
    names.set_hexpand(true);
    names.set_valign(gtk::Align::Center);
    names.append(&name);
    names.append(&subtitle);
    let ident = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    ident.append(&badge);
    ident.append(&names);
    state_pill.set_valign(gtk::Align::Center);
    ident.append(&state_pill);
    root.append(&ident);

    // ---- stat cards
    let cards = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    cards.set_homogeneous(true);
    let (cpu_val, cpu_sub) = stat_card(&cards, "CPU");
    let (mem_val, mem_sub) = stat_card(&cards, "MEMORY");
    let (thr_val, thr_sub) = stat_card(&cards, "THREADS");
    let (io_val, io_sub) = stat_card(&cards, "DISK I/O");
    root.append(&cards);

    // ---- CPU history graph
    let history: Rc<RefCell<VecDeque<f64>>> = Rc::new(RefCell::new(VecDeque::with_capacity(HISTORY)));
    let graph_card = gtk::Box::new(gtk::Orientation::Vertical, 6);
    graph_card.add_css_class("details-card");
    let graph_head = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let gl = gtk::Label::builder().label("CPU HISTORY").xalign(0.0).hexpand(true).build();
    gl.add_css_class("card-label");
    let gr = gtk::Label::new(Some("last 60 s"));
    gr.add_css_class("card-sub");
    graph_head.append(&gl);
    graph_head.append(&gr);
    graph_card.append(&graph_head);
    let graph = gtk::DrawingArea::builder().content_height(70).hexpand(true).build();
    graph.set_draw_func(glib::clone!(
        #[strong]
        history,
        move |_, cr, w, h| draw_history(cr, w as f64, h as f64, &history.borrow())
    ));
    graph_card.append(&graph);
    root.append(&graph_card);

    // ---- info list
    let grid = gtk::Grid::builder().row_spacing(8).column_spacing(18).build();
    grid.add_css_class("details-card");
    let mut values: Vec<(&'static str, gtk::Label)> = Vec::new();
    for (row, key) in ["Parent", "User", "Service", "Cgroup", "Started", "Priority", "Virtual size", "Executable"]
        .into_iter()
        .enumerate()
    {
        let k = gtk::Label::builder().label(key).xalign(0.0).yalign(0.0).build();
        k.add_css_class("info-key");
        // Equal minimum and maximum width: GTK takes the window's minimum
        // height at the content's minimum width, so a narrower minimum would
        // wrap long values (executable paths) onto extra lines there and
        // trigger "Trying to measure GtkWindow ... needs at least ..." warnings.
        let v = gtk::Label::builder()
            .xalign(0.0)
            .selectable(true)
            .wrap(true)
            .hexpand(true)
            .width_chars(52)
            .max_width_chars(52)
            .build();
        v.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        v.add_css_class("info-val");
        grid.attach(&k, 0, row as i32, 1, 1);
        grid.attach(&v, 1, row as i32, 1, 1);
        values.push((key, v));
    }
    root.append(&grid);

    let cmd_head = gtk::Label::builder().label("COMMAND LINE").xalign(0.0).build();
    cmd_head.add_css_class("card-label");
    let cmd = gtk::Label::builder()
        .label(if info.cmdline.is_empty() { format!("[{}]", info.name) } else { info.cmdline.to_string() })
        .xalign(0.0)
        .yalign(0.0)
        .selectable(true)
        .wrap(true)
        .width_chars(48)
        .build();
    cmd.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    cmd.add_css_class("cmdline");
    let cmd_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(44)
        .max_content_height(110)
        .propagate_natural_height(true)
        .child(&cmd)
        .build();
    cmd_scroll.add_css_class("cmdline-box");
    let cmd_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    cmd_box.append(&cmd_head);
    cmd_box.append(&cmd_scroll);
    root.append(&cmd_box);

    // ---- network connections (looked up for this process only)
    let net_head = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let nh = gtk::Label::builder().label("NETWORK").xalign(0.0).hexpand(true).build();
    nh.add_css_class("card-label");
    let net_summary = gtk::Label::builder().xalign(1.0).build();
    net_summary.add_css_class("card-sub");
    net_head.append(&nh);
    net_head.append(&net_summary);
    let net_list = gtk::Label::builder().xalign(0.0).yalign(0.0).selectable(true).width_chars(48).build();
    net_list.add_css_class("cmdline");
    let net_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .min_content_height(30)
        .max_content_height(130)
        .propagate_natural_height(true)
        .child(&net_list)
        .build();
    net_scroll.add_css_class("cmdline-box");
    let net_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    net_box.append(&net_head);
    net_box.append(&net_scroll);
    root.append(&net_box);

    // ---- risk banner, shown for blocked or critical processes
    let risk = gtk::Label::builder().xalign(0.0).wrap(true).width_chars(40).max_width_chars(70).visible(false).build();
    risk.add_css_class("risk-banner");
    root.append(&risk);
    let assessment: Rc<RefCell<Assessment>> = Rc::default();

    // ---- footer: status on the left, actions on the right
    let status = gtk::Label::builder().xalign(0.0).hexpand(true).wrap(true).width_chars(20).build();
    status.add_css_class("details-status");
    let term = gtk::Button::with_label("Terminate");
    term.set_tooltip_text(Some("Send SIGTERM: ask the process to exit cleanly"));
    term.add_css_class("action");
    let kill = gtk::Button::with_label("Force Kill");
    kill.set_tooltip_text(Some("Send SIGKILL: stop the process immediately"));
    kill.add_css_class("destructive-action");
    kill.add_css_class("action");
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    footer.set_valign(gtk::Align::End);
    footer.set_vexpand(true);
    footer.append(&status);
    footer.append(&term);
    footer.append(&kill);
    root.append(&footer);

    win.set_child(Some(&root));

    let exe = exe_path(pid).unwrap_or_else(|| "not accessible".into());

    let refresh = {
        let view = view.clone();
        let status = status.clone();
        let (term, kill) = (term.clone(), kill.clone());
        let (badge, graph, history) = (badge.clone(), graph.clone(), history.clone());
        let (risk, assessment) = (risk.clone(), assessment.clone());
        let prev_io: std::cell::Cell<Option<(std::time::Instant, u64, u64)>> = Default::default();
        let ticks = std::cell::Cell::new(0u32);
        move || -> bool {
            let v = view.borrow();
            let Some(p) = v.proc_info(pid).filter(|p| p.start_ticks == start_ticks) else {
                status.set_text("This process has exited.");
                state_pill.set_text("Exited");
                term.set_sensitive(false);
                kill.set_sensitive(false);
                return false;
            };
            let a = assess(p, v.procs(), std::process::id() as i32);
            if *assessment.borrow() != a {
                match (&a.blocked, a.warnings.first()) {
                    (Some(why), _) => {
                        risk.set_text(&format!("⛔  {why}"));
                        risk.set_visible(true);
                    }
                    (None, Some(_)) => {
                        risk.set_text(&format!("⚠  Critical process. {}", a.warnings.join(" ")));
                        risk.set_visible(true);
                    }
                    (None, None) => risk.set_visible(false),
                }
                let allowed = a.blocked.is_none();
                term.set_sensitive(allowed);
                kill.set_sensitive(allowed);
                *assessment.borrow_mut() = a;
            }
            let m = &v.meta;
            let cpu = p.cpu as f64;
            cpu_now.set(cpu);
            badge.queue_draw();
            {
                let mut h = history.borrow_mut();
                if h.len() == HISTORY {
                    h.pop_front();
                }
                h.push_back(cpu);
            }
            graph.queue_draw();

            set(&subtitle, &format!("PID {}  ·  {}", p.pid, p.user));
            set(&state_pill, state_name(p.state));
            set(&cpu_val, &fmt_cpu(cpu));
            set(&cpu_sub, "of one core");
            cpu_sub.set_tooltip_text(Some(&format!(
                "{:.1}% of all {} cores combined",
                cpu / m.ncpu.max(1) as f64,
                m.ncpu
            )));
            set(&mem_val, &fmt_bytes(p.rss));
            let mem_pct = if m.mem_total > 0 { p.rss as f64 / m.mem_total as f64 * 100.0 } else { 0.0 };
            set(&mem_sub, &format!("{mem_pct:.1}% of RAM"));
            mem_sub.set_tooltip_text(Some("Resident memory (RSS)"));
            set(&thr_val, &p.threads.to_string());
            set(&thr_sub, if p.kthread { "kernel thread" } else { "in this process" });

            // Disk I/O: rate from consecutive readings of this process only.
            match io_totals(pid) {
                Some((r, w)) => {
                    let now = std::time::Instant::now();
                    let rates = prev_io.get().map(|(at, pr, pw)| {
                        let dt = now.duration_since(at).as_secs_f64().max(1e-3);
                        (r.saturating_sub(pr) as f64 / dt, w.saturating_sub(pw) as f64 / dt)
                    });
                    prev_io.set(Some((now, r, w)));
                    match rates {
                        Some((rr, wr)) => {
                            set(&io_val, &format!("{}/s", fmt_rate(rr + wr)));
                            set(&io_sub, &format!("R {}/s · W {}/s", fmt_rate(rr), fmt_rate(wr)));
                        }
                        None => {
                            set(&io_val, "…");
                            set(&io_sub, &format!("{} read · {} written", fmt_bytes(r), fmt_bytes(w)));
                        }
                    }
                    io_sub.set_tooltip_text(Some(&format!(
                        "Total since start: {} read, {} written",
                        fmt_bytes(r),
                        fmt_bytes(w)
                    )));
                }
                None => {
                    set(&io_val, "–");
                    set(&io_sub, "not accessible");
                    io_sub.set_tooltip_text(Some("Another user's process: run PIDdle as root to see its disk I/O."));
                }
            }

            // Network: walking the fd table is costlier, so every 2 s.
            if ticks.get().is_multiple_of(4) {
                match net::sockets_of(pid) {
                    Some(s) => {
                        set(&net_summary, &net::summary(&s));
                        let lines: Vec<String> = s.connections.iter().map(net::describe).collect();
                        set(
                            &net_list,
                            &if lines.is_empty() { "no TCP or UDP sockets".into() } else { lines.join("\n") },
                        );
                    }
                    None => {
                        set(&net_summary, "not accessible");
                        set(&net_list, "Another user's process: run PIDdle as root to see its sockets.");
                    }
                }
            }
            ticks.set(ticks.get().wrapping_add(1));

            let started = if let Some(since_boot) = p.start_ticks.checked_div(m.clk_tck) {
                let secs = m.boot_time + since_boot;
                let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(secs);
                let at = glib::DateTime::from_unix_local(secs as i64)
                    .ok()
                    .and_then(|d| d.format("%Y-%m-%d %H:%M:%S").ok())
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                format!("{at}  ({} ago)", fmt_age(now.saturating_sub(secs)))
            } else {
                String::new()
            };
            for (key, label) in &values {
                let text = match *key {
                    "Parent" => match v.name_of(p.ppid) {
                        Some(n) => format!("{n}  ({})", p.ppid),
                        None => p.ppid.to_string(),
                    },
                    "User" => format!("{}  (uid {})", p.user, p.uid),
                    "Service" => p.service.describe(),
                    "Cgroup" => p.service.path.to_string(),
                    "Started" => started.clone(),
                    "Priority" => format!("nice {}", p.nice),
                    "Virtual size" => fmt_bytes(p.vsize),
                    "Executable" => exe.clone(),
                    _ => String::new(),
                };
                set(label, &text);
            }
            true
        }
    };
    refresh();
    let weak = win.downgrade();
    glib::timeout_add_local(REFRESH, move || {
        if weak.upgrade().is_none() || !refresh() {
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });

    for (btn, sig, verb, detail) in [
        (
            &term,
            Signal::Terminate,
            "Terminate",
            "SIGTERM asks the process to shut down cleanly. Unsaved work may still be lost.",
        ),
        (
            &kill,
            Signal::Kill,
            "Force Kill",
            "SIGKILL stops the process immediately. It cannot clean up, and unsaved data will be lost.",
        ),
    ] {
        let name = info.name.to_string();
        let status = status.clone();
        let assessment = assessment.clone();
        btn.connect_clicked(glib::clone!(
            #[weak]
            win,
            move |_| {
                let a = assessment.borrow().clone();
                if a.blocked.is_some() {
                    return;
                }
                let mut detail = detail.to_string();
                if a.descendants > 0 {
                    detail.push_str(&format!(
                        "\n\nIt has {} child process{}; they may stop or be left orphaned.",
                        a.descendants,
                        if a.descendants == 1 { "" } else { "es" }
                    ));
                }
                if sig == Signal::Kill {
                    detail.push_str("\n\nTip: try Terminate first; it lets the process save and exit cleanly.");
                }
                let status = status.clone();
                let win2 = win.clone();
                let send = move || match send_signal(pid, start_ticks, sig) {
                    Ok(()) => status.set_text(&format!("Sent {} to PID {pid}.", sig.name())),
                    Err(msg) => {
                        status.set_text(&msg);
                        gtk::AlertDialog::builder()
                            .message("Could not signal process")
                            .detail(msg)
                            .build()
                            .show(Some(&win2));
                    }
                };
                if a.is_critical() {
                    confirm_typed(&win, verb, &name, pid, &a.warnings, &detail, send);
                } else {
                    let dialog = gtk::AlertDialog::builder()
                        .modal(true)
                        .message(format!("{verb} “{name}” (PID {pid})?"))
                        .detail(detail)
                        .buttons(["Cancel", verb])
                        .cancel_button(0)
                        .default_button(0)
                        .build();
                    dialog.choose(Some(&win), None::<&gio::Cancellable>, move |res| {
                        if res == Ok(1) {
                            send();
                        }
                    });
                }
            }
        ));
    }

    win.present();
}

/// Stronger confirmation for critical processes: the user must type the
/// process name before the action button becomes available.
fn confirm_typed(
    parent: &gtk::Window,
    verb: &str,
    name: &str,
    pid: i32,
    warnings: &[String],
    detail: &str,
    on_confirm: impl Fn() + 'static,
) {
    let dlg = gtk::Window::builder().transient_for(parent).modal(true).resizable(false).title(verb).build();
    dlg.add_css_class("details");
    dlg.add_css_class("confirm");
    let header = gtk::HeaderBar::new();
    header.set_show_title_buttons(false);
    let t = gtk::Label::new(Some("Critical process"));
    t.add_css_class("title");
    header.set_title_widget(Some(&t));
    dlg.set_titlebar(Some(&header));

    let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
    root.set_margin_top(18);
    root.set_margin_bottom(18);
    root.set_margin_start(20);
    root.set_margin_end(20);
    let heading = gtk::Label::builder()
        .label(format!("{verb} “{name}” (PID {pid})?"))
        .xalign(0.0)
        .wrap(true)
        .width_chars(44)
        .max_width_chars(56)
        .build();
    heading.add_css_class("confirm-heading");
    root.append(&heading);
    let warn = gtk::Label::builder()
        .label(warnings.join("\n\n"))
        .xalign(0.0)
        .wrap(true)
        .width_chars(44)
        .max_width_chars(56)
        .build();
    warn.add_css_class("risk-banner");
    root.append(&warn);
    let det =
        gtk::Label::builder().label(detail.trim()).xalign(0.0).wrap(true).width_chars(44).max_width_chars(56).build();
    det.add_css_class("card-sub");
    root.append(&det);
    let prompt = gtk::Label::builder().xalign(0.0).build();
    prompt.set_markup(&format!("Type <b>{}</b> to confirm:", crate::view::glib_escape(name)));
    root.append(&prompt);
    let entry = gtk::Entry::builder().placeholder_text("process name").build();
    entry.add_css_class("confirm-entry");
    root.append(&entry);

    let cancel = gtk::Button::with_label("Cancel");
    cancel.add_css_class("action");
    let go = gtk::Button::with_label(verb);
    go.add_css_class("action");
    go.add_css_class("destructive-action");
    go.set_sensitive(false);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(6);
    buttons.append(&cancel);
    buttons.append(&go);
    root.append(&buttons);
    dlg.set_child(Some(&root));

    let expected = name.to_string();
    entry.connect_changed(glib::clone!(
        #[weak]
        go,
        move |e| go.set_sensitive(e.text().trim() == expected)
    ));
    entry.connect_activate(glib::clone!(
        #[weak]
        go,
        move |_| {
            if go.is_sensitive() {
                go.emit_clicked();
            }
        }
    ));
    cancel.connect_clicked(glib::clone!(
        #[weak]
        dlg,
        move |_| dlg.close()
    ));
    go.connect_clicked(glib::clone!(
        #[weak]
        dlg,
        move |_| {
            dlg.close();
            on_confirm();
        }
    ));
    let esc = gtk::EventControllerKey::new();
    esc.connect_key_pressed(glib::clone!(
        #[weak]
        dlg,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                dlg.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    ));
    dlg.add_controller(esc);
    dlg.present();
    entry.grab_focus();
}

/// Adds a stat card to `row` and returns its (value, caption) labels.
fn stat_card(row: &gtk::Box, title: &str) -> (gtk::Label, gtk::Label) {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 2);
    card.add_css_class("details-card");
    let t = gtk::Label::builder().label(title).xalign(0.0).build();
    t.add_css_class("card-label");
    let v = gtk::Label::builder().xalign(0.0).build();
    v.add_css_class("big-num");
    let s = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
    s.add_css_class("card-sub");
    card.append(&t);
    card.append(&v);
    card.append(&s);
    row.append(&card);
    (v, s)
}

fn set(label: &gtk::Label, text: &str) {
    if label.text() != text {
        label.set_text(text);
    }
}

fn draw_history(cr: &gtk::cairo::Context, w: f64, h: f64, hist: &VecDeque<f64>) {
    // Faint 100% (one core) reference line; the scale grows if a process exceeds it.
    let max = hist.iter().copied().fold(100.0, f64::max);
    let y_of = |v: f64| h - 2.0 - (v / max) * (h - 6.0);
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.08);
    cr.set_line_width(1.0);
    cr.set_dash(&[3.0, 3.0], 0.0);
    let y100 = y_of(100.0).round() + 0.5;
    cr.move_to(0.0, y100);
    cr.line_to(w, y100);
    let _ = cr.stroke();
    cr.set_dash(&[], 0.0);

    if hist.len() < 2 {
        return;
    }
    let step = w / (HISTORY - 1) as f64;
    let x0 = w - step * (hist.len() - 1) as f64;
    let last = *hist.back().unwrap();
    let c = cpu_color(last);

    cr.move_to(x0, h);
    for (i, &v) in hist.iter().enumerate() {
        cr.line_to(x0 + step * i as f64, y_of(v));
    }
    cr.line_to(w, h);
    cr.close_path();
    let fill = gtk::cairo::LinearGradient::new(0.0, 0.0, 0.0, h);
    fill.add_color_stop_rgba(0.0, c.0, c.1, c.2, 0.35);
    fill.add_color_stop_rgba(1.0, c.0, c.1, c.2, 0.02);
    let _ = cr.set_source(&fill);
    let _ = cr.fill();

    for (i, &v) in hist.iter().enumerate() {
        let (x, y) = (x0 + step * i as f64, y_of(v));
        if i == 0 { cr.move_to(x, y) } else { cr.line_to(x, y) }
    }
    cr.set_source_rgb(c.0, c.1, c.2);
    cr.set_line_width(1.6);
    cr.set_line_join(gtk::cairo::LineJoin::Round);
    let _ = cr.stroke();
}
