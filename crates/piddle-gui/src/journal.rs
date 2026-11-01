//! Logs pane for the details window: the journal of the process's service,
//! followed live. journalctl only runs while the pane is expanded.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use piddle_core::logs::{self, Event, Query, Stream};
use piddle_core::sampler::ProcInfo;

/// Lines kept in the view; older ones are dropped from the top.
const MAX_LINES: i32 = 2000;

pub fn pane(info: &ProcInfo) -> gtk::Widget {
    let expander = gtk::Expander::new(None);
    let head = gtk::Label::builder().label("LOGS").xalign(0.0).build();
    head.add_css_class("card-label");
    expander.set_label_widget(Some(&head));

    let Some(service_q) = Query::for_process(info, false) else {
        let note = gtk::Label::builder()
            .label("Kernel threads don't write to the journal; see journalctl -k for kernel messages.")
            .xalign(0.0)
            .wrap(true)
            .width_chars(40)
            .build();
        note.add_css_class("card-sub");
        expander.set_child(Some(&note));
        return expander.upcast();
    };
    let pid_q = Query::for_process(info, true);

    let command = gtk::Label::builder().xalign(0.0).hexpand(true).selectable(true).build();
    command.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    command.add_css_class("card-sub");
    command.add_css_class("mono");
    let only_pid = gtk::CheckButton::with_label("This process only");
    only_pid.set_tooltip_text(Some("Show only entries written by this PID, not the whole service"));
    // Without a unit the query is already per-process.
    only_pid.set_visible(service_q.is_service());
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    bar.append(&command);
    bar.append(&only_pid);

    let notice =
        gtk::Label::builder().xalign(0.0).wrap(true).width_chars(40).max_width_chars(70).visible(false).build();
    notice.add_css_class("risk-banner");

    let text = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(10)
        .right_margin(10)
        .top_margin(8)
        .bottom_margin(8)
        .build();
    text.add_css_class("journal");
    let buf = text.buffer();
    for (name, color, bold) in [
        ("time", "#6f7d96", false),
        ("ident", "#8fa3c8", false),
        ("crit", "#ff6b6b", true),
        ("err", "#ff8a80", false),
        ("warn", "#f2c94c", false),
        ("notice", "#e6edf7", true),
        ("debug", "#6f7d96", false),
    ] {
        let tag = gtk::TextTag::builder().name(name).foreground(color).build();
        if bold {
            tag.set_weight(700);
        }
        buf.tag_table().add(&tag);
    }
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(220)
        .max_content_height(220)
        .child(&text)
        .build();
    scroll.add_css_class("cmdline-box");

    let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
    body.set_margin_top(6);
    body.append(&bar);
    body.append(&notice);
    body.append(&scroll);
    expander.set_child(Some(&body));

    let stream: Rc<RefCell<Option<Stream>>> = Rc::default();
    // Bumped on every (re)start so a stopped stream's late events are ignored.
    let generation = Rc::new(Cell::new(0u64));

    let start = {
        let (stream, generation) = (stream.clone(), generation.clone());
        let (command, notice, buf, scroll, only_pid, text) =
            (command.clone(), notice.clone(), buf.clone(), scroll.clone(), only_pid.clone(), text.clone());
        move || {
            stream.borrow_mut().take();
            let g = generation.get() + 1;
            generation.set(g);
            buf.set_text("");
            let q = match (&pid_q, only_pid.is_active()) {
                (Some(p), true) => p.clone(),
                _ => service_q.clone(),
            };
            command.set_text(&q.command_line());
            command.set_tooltip_text(Some(&q.command_line()));
            notice.set_text(logs::NO_SYSTEM_JOURNAL);
            notice.set_visible(logs::hidden_by_permissions(&q));
            let s = match logs::follow(&q) {
                Ok(s) => s,
                Err(why) => {
                    notice.set_text(&why);
                    notice.set_visible(true);
                    return;
                }
            };
            let rx = s.rx.clone();
            *stream.borrow_mut() = Some(s);
            let (generation, notice, buf, scroll, text) =
                (generation.clone(), notice.clone(), buf.clone(), scroll.clone(), text.clone());
            glib::spawn_future_local(async move {
                while let Ok(ev) = rx.recv().await {
                    if generation.get() != g {
                        return;
                    }
                    let adj = scroll.vadjustment();
                    let at_bottom = adj.value() + adj.page_size() >= adj.upper() - 4.0;
                    match ev {
                        Event::Entry(e) => append(&buf, &e),
                        Event::Ended(why) => {
                            notice.set_text(&format!("journalctl stopped: {why}"));
                            notice.set_visible(true);
                        }
                    }
                    // Drain whatever else is queued before scrolling once.
                    while let Ok(Event::Entry(e)) = rx.try_recv() {
                        append(&buf, &e);
                    }
                    if buf.line_count() > MAX_LINES {
                        let mut a = buf.start_iter();
                        let mut b = buf.iter_at_line(buf.line_count() - MAX_LINES).unwrap_or(buf.start_iter());
                        buf.delete(&mut a, &mut b);
                    }
                    if at_bottom {
                        let end = buf.end_iter();
                        let mark = buf.create_mark(None, &end, false);
                        text.scroll_mark_onscreen(&mark);
                        buf.delete_mark(&mark);
                    }
                }
            });
        }
    };
    let start = Rc::new(start);

    expander.connect_expanded_notify(glib::clone!(
        #[strong]
        start,
        #[strong]
        stream,
        #[strong]
        generation,
        move |e| {
            if e.is_expanded() {
                start();
            } else {
                generation.set(generation.get() + 1);
                stream.borrow_mut().take();
            }
        }
    ));
    only_pid.connect_toggled(glib::clone!(
        #[strong]
        start,
        move |_| start()
    ));
    // Window closed: stop journalctl.
    expander.connect_unrealize(move |_| {
        generation.set(generation.get() + 1);
        stream.borrow_mut().take();
    });
    expander.upcast()
}

fn append(buf: &gtk::TextBuffer, e: &logs::Entry) {
    let mut end = buf.end_iter();
    if buf.char_count() > 0 {
        buf.insert(&mut end, "\n");
    }
    buf.insert_with_tags_by_name(&mut end, &logs::fmt_clock(e.time_us), &["time"]);
    buf.insert(&mut end, " ");
    if !e.ident.is_empty() {
        let who = match e.pid {
            Some(p) => format!("{}[{p}]", e.ident),
            None => e.ident.clone(),
        };
        buf.insert_with_tags_by_name(&mut end, &who, &["ident"]);
        buf.insert(&mut end, " ");
    }
    let tag = match e.priority {
        0..=2 => Some("crit"),
        3 => Some("err"),
        4 => Some("warn"),
        5 => Some("notice"),
        7 => Some("debug"),
        _ => None,
    };
    match tag {
        Some(t) => buf.insert_with_tags_by_name(&mut end, &e.message, &[t]),
        None => buf.insert(&mut end, &e.message),
    }
}
