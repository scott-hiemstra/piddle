//! Rendering. Colors follow the GUI: CPU load runs from cool blue through
//! yellow to hot red, where 100% is one fully used core.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use piddle_core::actions::{exe_path, fmt_age, state_name};
use piddle_core::containers::{Filter, GroupKind};
use piddle_core::encode::{cpu_color, fmt_bytes, fmt_cpu, fmt_rate};
use piddle_core::logs;
use piddle_core::net;
use piddle_core::sampler::SYSTEM_FIGURES_MISSING;
use piddle_core::service::ServiceKind;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row as TRow, Sparkline, Table, Wrap};

use crate::app::{App, HISTORY, Mode, Row};

const DIM: Color = Color::Rgb(130, 140, 160);
const ACCENT: Color = Color::Rgb(242, 201, 76);
const BORDER: Color = Color::Rgb(60, 68, 84);
const SELECT_BG: Color = Color::Rgb(38, 46, 64);

/// Service kind tags, matching the GUI's hull tints.
fn kind_color(k: ServiceKind) -> Color {
    match k {
        ServiceKind::System => Color::Rgb(150, 175, 230),
        ServiceKind::User => Color::Rgb(115, 215, 140),
        ServiceKind::Scope => Color::Rgb(190, 150, 250),
        ServiceKind::Container => Color::Rgb(240, 130, 205),
        ServiceKind::Kernel | ServiceKind::Other => DIM,
    }
}

/// Compose project and pod tags in the containers view.
const MANAGER: Color = Color::Rgb(250, 170, 120);

fn manager_label(k: GroupKind) -> &'static str {
    k.label()
}

/// Map an RGB color to what the terminal supports.
pub struct Palette {
    truecolor: bool,
}

impl Palette {
    pub fn detect() -> Self {
        let ct = std::env::var("COLORTERM").unwrap_or_default().to_lowercase();
        Self { truecolor: ct.contains("truecolor") || ct.contains("24bit") }
    }

    fn rgb(&self, r: u8, g: u8, b: u8) -> Color {
        if self.truecolor {
            return Color::Rgb(r, g, b);
        }
        // Nearest entry in the xterm 6x6x6 color cube.
        let q = |v: u8| ((v as u16 * 5 + 127) / 255) as u8;
        Color::Indexed(16 + 36 * q(r) + 6 * q(g) + q(b))
    }

    fn fix(&self, c: Color) -> Color {
        match c {
            Color::Rgb(r, g, b) => self.rgb(r, g, b),
            other => other,
        }
    }

    fn cpu(&self, cpu: f64) -> Color {
        let c = cpu_color(cpu);
        self.rgb((c.0 * 255.0) as u8, (c.1 * 255.0) as u8, (c.2 * 255.0) as u8)
    }
}

pub fn draw(f: &mut Frame, app: &mut App, pal: &Palette) {
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(3), Constraint::Length(1)]).areas(f.area());
    draw_header(f, app, pal, header);
    draw_table(f, app, pal, body);
    draw_footer(f, app, pal, footer);

    match &app.mode {
        Mode::Details => draw_details(f, app, pal),
        Mode::Confirm(_) | Mode::ConfirmTyped(..) => draw_confirm(f, app, pal),
        Mode::Logs => draw_logs(f, app, pal),
        Mode::Help => draw_help(f, app, pal),
        Mode::Containers(at) => draw_picker(f, app, pal, *at),
        _ => {}
    }
}

fn draw_header(f: &mut Frame, app: &App, pal: &Palette, area: Rect) {
    let Some(s) = &app.snap else {
        f.render_widget(Paragraph::new("Sampling /proc…").style(Style::new().fg(pal.fix(DIM))), area);
        return;
    };
    let [l1, l2, l3] = Layout::vertical([Constraint::Length(1); 3]).areas(area);
    let mem_frac = s.memory.map_or(0.0, |m| m.used as f64 / m.total as f64);
    let threads: u64 = s.procs.iter().map(|p| p.threads as u64).sum();
    let self_cpu = s.procs.iter().find(|p| p.pid == s.self_pid).map(|p| p.cpu).unwrap_or(0.0);

    let title = Line::from(vec![
        Span::styled(" PID", Style::new().add_modifier(Modifier::BOLD)),
        Span::styled("dle", Style::new().fg(pal.fix(ACCENT)).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(
                "   {} processes · {} threads · {} cores · view: {} · sort: {} · every {} ms{}",
                s.procs.len(),
                threads,
                s.ncpu,
                app.view.name(),
                app.sort.name(),
                app.interval_ms(),
                if app.paused { " · PAUSED" } else { "" }
            ),
            Style::new().fg(pal.fix(DIM)),
        ),
    ]);
    f.render_widget(Paragraph::new(title), l1);
    // Unknown whole-machine figures (a subset=pid /proc) show as such, never as 0.
    let cpu_meter = match s.total_cpu {
        Some(c) => meter(" CPU ", c as f64 / 100.0, &format!("{c:.1}% of all cores"), pal.cpu(c as f64), pal),
        None => meter(" CPU ", 0.0, &format!("— {SYSTEM_FIGURES_MISSING}"), pal.fix(DIM), pal),
    };
    f.render_widget(cpu_meter, l2);
    let mem_text = match s.memory {
        Some(m) => format!("{} / {}", fmt_bytes(m.used), fmt_bytes(m.total)),
        None => "—".to_string(),
    };
    f.render_widget(
        meter(
            " MEM ",
            mem_frac,
            &format!(
                "{mem_text}   ·   PIDdle {} CPU, {:.1} ms per sample",
                fmt_cpu(self_cpu as f64),
                s.sample_cost.as_secs_f64() * 1e3
            ),
            pal.fix(Color::Rgb(90, 140, 255)),
            pal,
        ),
        l3,
    );
}

/// One-line meter: label, a bar, then text.
fn meter<'a>(label: &'a str, frac: f64, text: &str, color: Color, pal: &Palette) -> Paragraph<'a> {
    let width = 30usize;
    let filled = ((frac.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    Paragraph::new(Line::from(vec![
        Span::styled(label, Style::new().fg(pal.fix(DIM))),
        Span::styled("█".repeat(filled), Style::new().fg(color)),
        Span::styled("░".repeat(width - filled), Style::new().fg(pal.fix(BORDER))),
        Span::raw(format!("  {text}")),
    ]))
}

/// A bar for CPU percent (one core = full) that marks overflow beyond 100%.
fn cpu_bar(cpu: f64, width: usize) -> String {
    let filled = ((cpu / 100.0).clamp(0.0, 1.0) * width as f64).round() as usize;
    let mut s = "▮".repeat(filled) + &"·".repeat(width - filled);
    if cpu > 100.0 {
        s.pop();
        s.push('▶');
    }
    s
}

fn draw_table(f: &mut Frame, app: &mut App, pal: &Palette, area: Rect) {
    let show_io = app.show_io;
    let mut head = vec!["PID", "NAME", "USER", "CPU", "", "MEM", ""];
    if show_io {
        head.extend(["READ/s", "WRITE/s"]);
    }
    head.extend(["THR", "S"]);
    let header = TRow::new(head).style(Style::new().fg(pal.fix(DIM)).add_modifier(Modifier::BOLD));
    // Disk I/O cells: "–" when the process's I/O isn't readable (other users' processes).
    let io_cell = |rate: Option<f64>| -> Cell<'static> {
        match rate {
            None => Cell::from(Span::styled("–", Style::new().fg(pal.fix(BORDER)))),
            Some(r) if r < 1.0 => Cell::from(Span::styled("0", Style::new().fg(pal.fix(DIM)))),
            Some(r) => Cell::from(Span::styled(fmt_rate(r), Style::new().fg(pal.fix(Color::Rgb(120, 200, 160))))),
        }
    };
    let max_rss = app.max_rss as f64;
    let rows: Vec<TRow> = app
        .rows
        .iter()
        .map(|r| match r {
            Row::Group { label, kind, manager, depth, count, cpu, rss, io, collapsed, .. } => {
                let c = pal.cpu(*cpu as f64);
                let mut name = Vec::with_capacity(4);
                if *depth > 0 {
                    name.push(Span::raw("   ".repeat(*depth as usize)));
                }
                if let Some(m) = manager {
                    name.push(Span::styled(format!("{:<9} ", manager_label(*m)), Style::new().fg(pal.fix(MANAGER))));
                }
                if let Some(k) = kind {
                    name.push(Span::styled(format!("{:<9} ", k.label()), Style::new().fg(pal.fix(kind_color(*k)))));
                }
                name.push(Span::styled(label.clone(), Style::new().add_modifier(Modifier::BOLD)));
                name.push(Span::styled(format!("  ×{count}"), Style::new().fg(pal.fix(DIM))));
                let mut cells = vec![
                    Cell::from(if *collapsed { "▸" } else { "▾" }),
                    Cell::from(Line::from(name)),
                    Cell::from(""),
                    Cell::from(Span::styled(fmt_cpu(*cpu as f64), Style::new().fg(c).add_modifier(Modifier::BOLD))),
                    Cell::from(Span::styled(cpu_bar(*cpu as f64, 10), Style::new().fg(c))),
                    Cell::from(fmt_bytes(*rss)),
                    Cell::from(""),
                ];
                if show_io {
                    cells.extend([io_cell(Some(io.0)), io_cell(Some(io.1))]);
                }
                cells.extend([Cell::from(""), Cell::from("")]);
                TRow::new(cells).style(Style::new().bg(pal.fix(Color::Rgb(22, 26, 34))))
            }
            Row::Proc { info: p, prefix } => {
                let c = pal.cpu(p.cpu as f64);
                let mem_frac = (p.rss as f64 / max_rss).sqrt();
                let mem_w = (mem_frac * 6.0).round() as usize;
                let mut cells = vec![
                    Cell::from(Span::styled(p.pid.to_string(), Style::new().fg(pal.fix(DIM)))),
                    Cell::from(Line::from(vec![
                        Span::styled(prefix.clone(), Style::new().fg(pal.fix(BORDER))),
                        Span::raw(p.name.to_string()),
                    ])),
                    Cell::from(Span::styled(p.user.to_string(), Style::new().fg(pal.fix(DIM)))),
                    Cell::from(Span::styled(fmt_cpu(p.cpu as f64), Style::new().fg(c))),
                    Cell::from(Span::styled(cpu_bar(p.cpu as f64, 10), Style::new().fg(c))),
                    Cell::from(fmt_bytes(p.rss)),
                    Cell::from(Span::styled(
                        "■".repeat(mem_w.max(1)),
                        Style::new().fg(pal.fix(Color::Rgb(90, 140, 255))),
                    )),
                ];
                if show_io {
                    cells.extend([io_cell(p.io.map(|i| i.read_rate)), io_cell(p.io.map(|i| i.write_rate))]);
                }
                cells.extend([
                    Cell::from(p.threads.to_string()),
                    Cell::from(Span::styled(p.state.to_string(), Style::new().fg(pal.fix(DIM)))),
                ]);
                TRow::new(cells)
            }
        })
        .collect();

    let mut title = Vec::new();
    if let Some(t) = app.filter_text() {
        let color = if app.filter_gone { Color::Rgb(255, 120, 90) } else { kind_color(ServiceKind::Container) };
        title.push(Span::styled(format!(" {t} ·"), Style::new().fg(pal.fix(color)).add_modifier(Modifier::BOLD)));
    }
    if !app.query.is_empty() {
        title.push(Span::raw(" search: "));
        title.push(Span::styled(app.query.clone(), Style::new().fg(pal.fix(ACCENT)).add_modifier(Modifier::BOLD)));
        title.push(Span::raw(" ·"));
    }
    title.push(Span::raw(format!(" {} rows ", app.rows.len())));
    let title = Line::from(title);
    let mut widths = vec![
        Constraint::Length(8),
        Constraint::Fill(1),
        Constraint::Length(10),
        Constraint::Length(6),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(6),
    ];
    if show_io {
        widths.extend([Constraint::Length(7), Constraint::Length(7)]);
    }
    widths.extend([Constraint::Length(4), Constraint::Length(1)]);
    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(pal.fix(BORDER)))
                .title(title),
        )
        .row_highlight_style(Style::new().bg(pal.fix(SELECT_BG)).add_modifier(Modifier::BOLD))
        .column_spacing(1);
    f.render_stateful_widget(table, area, &mut app.table);
}

fn draw_footer(f: &mut Frame, app: &App, pal: &Palette, area: Rect) {
    let key = |k: &'static str| Span::styled(k, Style::new().fg(pal.fix(ACCENT)).add_modifier(Modifier::BOLD));
    let txt = |t: &'static str| Span::styled(t, Style::new().fg(pal.fix(DIM)));
    let line = if let Some((msg, at)) = &app.status
        && at.elapsed() < Duration::from_secs(4)
    {
        Line::from(Span::styled(format!(" {msg}"), Style::new().fg(pal.fix(ACCENT))))
    } else if matches!(app.mode, Mode::Search) {
        Line::from(vec![txt(" type to filter · "), key("Enter"), txt(" keep · "), key("Esc"), txt(" clear")])
    } else {
        let mut keys = vec![
            key(" ↑↓"),
            txt(" move  "),
            key("Enter"),
            txt(" details  "),
            key("/"),
            txt(" search  "),
            key("v"),
            txt(" view  "),
        ];
        // Only while there are containers (or the view is still limited to one).
        if !app.choices.is_empty() || !app.filter.is_all() {
            keys.extend([key("c"), txt(" containers  ")]);
        }
        keys.extend([
            key("s"),
            txt(" sort  "),
            key("t"),
            txt(" terminate  "),
            key("x"),
            txt(" kill  "),
            key("?"),
            txt(" help  "),
            key("q"),
            txt(" quit"),
        ]);
        Line::from(keys)
    };
    f.render_widget(Paragraph::new(line), area);
}

/// Centered popup area of at most `w` x `h` cells.
fn popup(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

fn popup_block<'a>(title: &'a str, color: Color, pal: &Palette) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .title(Line::from(Span::styled(format!(" {title} "), Style::new().add_modifier(Modifier::BOLD))))
        .style(Style::new().bg(pal.fix(Color::Rgb(16, 19, 26))))
}

fn draw_details(f: &mut Frame, app: &App, pal: &Palette) {
    let area = popup(f.area(), 92, 32);
    f.render_widget(Clear, area);
    let Some(p) = app.detail_proc() else {
        let block = popup_block("Process details", pal.fix(BORDER), pal);
        f.render_widget(Paragraph::new("\n  This process has exited.  (Esc to close)").block(block), area);
        return;
    };
    let s = app.snap.as_ref().unwrap();
    let c = pal.cpu(p.cpu as f64);
    let block = popup_block("Process details", c, pal);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [info, graph, keys] =
        Layout::vertical([Constraint::Min(8), Constraint::Length(5), Constraint::Length(1)]).margin(1).areas(inner);

    let started = p
        .start_ticks
        .checked_div(s.clk_tck)
        .zip(s.boot_time)
        .map(|(since_boot, boot)| {
            let at = boot + since_boot;
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(at);
            format!("{} ago", fmt_age(now.saturating_sub(at)))
        })
        .unwrap_or_else(|| "—".into());
    let parent = match app.name_of(p.ppid) {
        Some(n) => format!("{n} ({})", p.ppid),
        None => p.ppid.to_string(),
    };
    let mem_pct = s.memory.map(|m| m.percent_of(p.rss));
    let label = |k: &str| Span::styled(format!("{k:<12}"), Style::new().fg(pal.fix(DIM)));
    let mut lines = vec![
        Line::from(vec![
            Span::styled(p.name.to_string(), Style::new().add_modifier(Modifier::BOLD).fg(c)),
            Span::styled(
                format!("   PID {} · {} · {}", p.pid, p.user, state_name(p.state)),
                Style::new().fg(pal.fix(DIM)),
            ),
        ]),
        Line::raw(""),
        Line::from(vec![
            label("CPU"),
            Span::styled(fmt_cpu(p.cpu as f64), Style::new().fg(c).add_modifier(Modifier::BOLD)),
            Span::raw(format!(" of one core · {:.1}% of all {} cores", p.cpu as f64 / s.ncpu.max(1) as f64, s.ncpu)),
        ]),
        Line::from(vec![
            label("Memory"),
            Span::raw(format!(
                "{} resident{} · {} virtual",
                fmt_bytes(p.rss),
                mem_pct.map_or(String::new(), |m| format!(" ({m:.1}% of RAM)")),
                fmt_bytes(p.vsize)
            )),
        ]),
        Line::from(vec![label("Threads"), Span::raw(format!("{}   nice {}", p.threads, p.nice))]),
        Line::from(vec![
            label("Disk I/O"),
            Span::raw(match p.io {
                Some(io) => format!(
                    "read {}/s · write {}/s   (total {} read, {} written)",
                    fmt_rate(io.read_rate),
                    fmt_rate(io.write_rate),
                    fmt_bytes(io.read_total),
                    fmt_bytes(io.write_total)
                ),
                None if app.show_io => "not accessible (another user's process; run as root to see it)".into(),
                None => "collection off (press i in the list)".into(),
            }),
        ]),
        Line::from(vec![label("Parent"), Span::raw(parent)]),
        Line::from(vec![
            label("Service"),
            Span::styled(p.service.describe(), Style::new().fg(pal.fix(kind_color(p.service.kind)))),
        ]),
        Line::from(vec![label("Cgroup"), Span::styled(p.service.path.to_string(), Style::new().fg(pal.fix(DIM)))]),
    ];
    if let Some(pl) = app.placement(p) {
        lines.push(Line::from(vec![
            label("Container"),
            Span::styled(pl.name.to_string(), Style::new().fg(pal.fix(kind_color(ServiceKind::Container)))),
            Span::styled(format!("  {}", pl.detail), Style::new().fg(pal.fix(DIM))),
        ]));
        // Unnamed (the runtime couldn't be asked): say why.
        if Some(&pl.name) == Some(&p.service.label) {
            for note in s.containers.notes() {
                lines.push(Line::from(vec![label(""), Span::styled(note, Style::new().fg(pal.fix(DIM)))]));
            }
        }
    }
    lines.extend([
        Line::from(vec![label("Started"), Span::raw(started)]),
        Line::from(vec![label("Executable"), Span::raw(exe_path(p.pid).unwrap_or_else(|| "not accessible".into()))]),
        Line::from(vec![
            label("Command"),
            Span::raw(if p.cmdline.is_empty() { format!("[{}]", p.name) } else { p.cmdline.to_string() }),
        ]),
    ]);
    if p.kthread {
        lines.push(Line::styled("Kernel thread", Style::new().fg(pal.fix(DIM))));
    }
    // Network: sockets matched to the kernel's connection tables.
    match app.detail_net.as_ref().map(|(_, s)| s) {
        Some(Some(sockets)) => {
            lines.push(Line::from(vec![label("Network"), Span::raw(net::summary(sockets))]));
            let max_rows = 6;
            for c in sockets.connections.iter().take(max_rows) {
                let color = if c.state == "LISTEN" { pal.fix(ACCENT) } else { pal.fix(Color::Rgb(120, 200, 160)) };
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(12)),
                    Span::styled(net::describe(c), Style::new().fg(color)),
                ]));
            }
            if sockets.connections.len() > max_rows {
                lines.push(Line::styled(
                    format!("{}… and {} more", " ".repeat(12), sockets.connections.len() - max_rows),
                    Style::new().fg(pal.fix(DIM)),
                ));
            }
        }
        Some(None) => lines.push(Line::from(vec![
            label("Network"),
            Span::styled("not accessible (another user's process; run as root)", Style::new().fg(pal.fix(DIM))),
        ])),
        None => {}
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), info);

    // CPU history: scale to at least one core so small values look small.
    let data: Vec<u64> = app.history.iter().map(|v| (*v * 10.0) as u64).collect();
    let max = data.iter().copied().max().unwrap_or(0).max(1000);
    let secs = HISTORY as u64 * app.interval_ms() / 1000;
    f.render_widget(
        Sparkline::default()
            .block(Block::bordered().border_style(Style::new().fg(pal.fix(BORDER))).title(Span::styled(
                format!(" CPU history · up to {secs} s · top = 100% "),
                Style::new().fg(pal.fix(DIM)),
            )))
            .data(&data)
            .max(max)
            .style(Style::new().fg(c)),
        graph,
    );
    let key = |k: &'static str| Span::styled(k, Style::new().fg(pal.fix(ACCENT)).add_modifier(Modifier::BOLD));
    let txt = |t: &'static str| Span::styled(t, Style::new().fg(pal.fix(DIM)));
    f.render_widget(
        Paragraph::new(Line::from(vec![
            key("t"),
            txt(" terminate   "),
            key("x"),
            txt(" force kill   "),
            key("l"),
            txt(" logs   "),
            key("Esc"),
            txt(" close"),
        ])),
        keys,
    );
}

fn draw_logs(f: &mut Frame, app: &App, pal: &Palette) {
    let Some(v) = &app.logs else { return };
    let full = f.area();
    let area = popup(full, full.width.saturating_sub(4).max(40), full.height.saturating_sub(2).max(10));
    f.render_widget(Clear, area);
    let title = format!("Logs · {}", v.name);
    let block = popup_block(&title, pal.fix(ACCENT), pal);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let notice_h = v.notice.as_ref().map_or(0, |n| (n.chars().count() as u16).div_ceil(inner.width.max(1)) + 1);
    let [cmd, notice, body, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(notice_h),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    f.render_widget(Paragraph::new(Span::styled(format!(" {}", v.command), Style::new().fg(pal.fix(DIM)))), cmd);
    if let Some(n) = &v.notice {
        f.render_widget(
            Paragraph::new(Span::styled(n.as_str(), Style::new().fg(pal.fix(ACCENT)))).wrap(Wrap { trim: true }),
            notice,
        );
    }

    let h = body.height as usize;
    let end = v.lines.len().saturating_sub(v.scroll);
    let start = end.saturating_sub(h);
    let lines: Vec<Line> = v
        .lines
        .range(start..end)
        .map(|e| {
            let msg_style = match e.priority {
                0..=2 => Style::new().fg(pal.fix(Color::Rgb(255, 107, 107))).add_modifier(Modifier::BOLD),
                3 => Style::new().fg(pal.fix(Color::Rgb(255, 138, 128))),
                4 => Style::new().fg(pal.fix(ACCENT)),
                5 => Style::new().add_modifier(Modifier::BOLD),
                7 => Style::new().fg(pal.fix(DIM)),
                _ => Style::new(),
            };
            let who = match e.pid {
                Some(p) => format!("{}[{p}] ", e.ident),
                None if e.ident.is_empty() => String::new(),
                None => format!("{} ", e.ident),
            };
            Line::from(vec![
                Span::styled(format!(" {} ", logs::fmt_clock(e.time_us)), Style::new().fg(pal.fix(DIM))),
                Span::styled(who, Style::new().fg(pal.fix(Color::Rgb(143, 163, 200)))),
                Span::styled(e.message.as_str(), msg_style),
            ])
        })
        .collect();
    if lines.is_empty() && v.notice.is_none() {
        f.render_widget(Paragraph::new(Span::styled(" no entries yet", Style::new().fg(pal.fix(DIM)))), body);
    } else {
        f.render_widget(Paragraph::new(lines), body);
    }

    let key = |k: &'static str| Span::styled(k, Style::new().fg(pal.fix(ACCENT)).add_modifier(Modifier::BOLD));
    let txt = |t: String| Span::styled(t, Style::new().fg(pal.fix(DIM)));
    let mut hint = vec![key(" ↑↓ PgUp PgDn"), txt(" scroll  ".into()), key("End"), txt(" follow  ".into())];
    if v.per_pid.is_some() {
        hint.push(key("o"));
        hint.push(txt(if v.pid_only { " whole service  ".into() } else { " this process only  ".into() }));
    }
    hint.push(key("Esc"));
    hint.push(txt(" back".into()));
    if v.scroll > 0 {
        hint.push(txt(format!("   ⏸ {} newer below", v.scroll)));
    }
    f.render_widget(Paragraph::new(Line::from(hint)), keys);
}

fn draw_confirm(f: &mut Frame, app: &App, pal: &Palette) {
    let (p, typed) = match &app.mode {
        Mode::Confirm(p) => (p, None),
        Mode::ConfirmTyped(p, t) => (p, Some(t.as_str())),
        _ => return,
    };
    let red = pal.fix(Color::Rgb(220, 70, 60));
    let amber = pal.fix(Color::Rgb(245, 190, 90));
    let verb = match p.sig {
        piddle_core::actions::Signal::Terminate => "Terminate",
        piddle_core::actions::Signal::Kill => "Force kill",
    };
    let mut lines = vec![
        Line::from(Span::styled(
            format!("{verb} “{}” (PID {})?", p.info.name, p.info.pid),
            Style::new().add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
    ];
    for w in &p.assessment.warnings {
        lines.push(Line::from(Span::styled(format!("⚠ {w}"), Style::new().fg(amber))));
    }
    if !p.assessment.warnings.is_empty() {
        lines.push(Line::raw(""));
    }
    let detail = match p.sig {
        piddle_core::actions::Signal::Terminate => "SIGTERM asks the process to shut down cleanly.",
        piddle_core::actions::Signal::Kill => {
            "SIGKILL stops it immediately; it cannot clean up. Try terminate (t) first."
        }
    };
    lines.push(Line::styled(detail, Style::new().fg(pal.fix(DIM))));
    if p.assessment.descendants > 0 {
        lines.push(Line::styled(
            format!("It has {} child process(es); they may stop or be orphaned.", p.assessment.descendants),
            Style::new().fg(pal.fix(DIM)),
        ));
    }
    lines.push(Line::raw(""));
    match typed {
        None => lines.push(Line::from(vec![
            Span::raw("Press "),
            Span::styled("y", Style::new().fg(red).add_modifier(Modifier::BOLD)),
            Span::raw(" to confirm, any other key to cancel."),
        ])),
        Some(t) => {
            let ok = t.trim() == &*p.info.name;
            lines.push(Line::from(vec![
                Span::raw("Critical process. Type "),
                Span::styled(p.info.name.to_string(), Style::new().add_modifier(Modifier::BOLD)),
                Span::raw(" and press Enter (Esc cancels):"),
            ]));
            lines.push(Line::from(vec![
                Span::styled("> ", Style::new().fg(pal.fix(DIM))),
                Span::styled(
                    t.to_string(),
                    Style::new().fg(if ok { red } else { Color::Reset }).add_modifier(Modifier::BOLD),
                ),
                Span::styled("█", Style::new().fg(pal.fix(DIM))),
            ]));
        }
    }
    // Height must include lines that wrap inside the popup (74 wide, minus borders and padding).
    let inner_w = 74u16.min(f.area().width.saturating_sub(2)).saturating_sub(4).max(1) as usize;
    let rows: usize = lines.iter().map(|l| l.width().max(1).div_ceil(inner_w)).sum();
    let area = popup(f.area(), 74, rows as u16 + 2);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(
            popup_block(if typed.is_some() { "Critical process" } else { "Confirm" }, red, pal)
                .padding(ratatui::widgets::Padding::horizontal(1)),
        ),
        area,
    );
}

fn draw_help(f: &mut Frame, app: &App, pal: &Palette) {
    let containers = !app.choices.is_empty();
    let mut rows = vec![
        ("↑ ↓ PgUp PgDn Home End", "move"),
        ("Enter", "details (or collapse a group)"),
        ("Space", "collapse / expand group or tree branch"),
        ("/", "search name, command, service or PID; Esc clears"),
        if containers {
            ("v or 1–6", "view: list, by application, by service, by parent, tree, by container")
        } else {
            ("v or 1–5", "view: list, by application, by service, by parent, tree")
        },
        ("s", "sort: CPU, memory, disk I/O, PID, name"),
        ("i", "show / hide disk I/O columns"),
        ("a", "active processes only"),
        ("K", "show / hide kernel threads"),
        ("+ / -", "sampling interval"),
        ("p", "pause / resume"),
        ("t", "terminate (SIGTERM)"),
        ("x", "force kill (SIGKILL)"),
        ("l (in details)", "journal logs (o: this process only)"),
        ("q, Ctrl+C", "quit"),
    ];
    if containers {
        rows.insert(5, ("c", "containers: limit the view to one, or a project or pod; Esc: all"));
    }
    let mut lines: Vec<Line> = rows
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(format!("{k:<24}"), Style::new().fg(pal.fix(ACCENT)).add_modifier(Modifier::BOLD)),
                Span::raw(*d),
            ])
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "CPU colors: blue (idle) → yellow → red (100% = one full core)",
        Style::new().fg(pal.fix(DIM)),
    ));
    let area = popup(f.area(), 72, lines.len() as u16 + 4);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(popup_block("Keys", pal.fix(ACCENT), pal).padding(ratatui::widgets::Padding::horizontal(1))),
        area,
    );
}

/// The containers picker: every entry, groups with their containers indented.
fn draw_picker(f: &mut Frame, app: &App, pal: &Palette, at: usize) {
    let notes = app.snap.as_ref().map(|s| s.containers.notes()).unwrap_or_default();
    let full = f.area();
    // At least 6 rows when the terminal has them; never more than it has
    // (a clamp with min > max panics below 10 rows).
    let want = app.choices.len() as u16 + notes.len() as u16 * 2 + 4;
    let height = want.max(6).min(full.height.saturating_sub(4));
    let area = popup(full, 64, height);
    f.render_widget(Clear, area);
    let block = popup_block("Containers", pal.fix(kind_color(ServiceKind::Container)), pal)
        .title_bottom(Line::from(Span::styled(" ↑↓ choose · Enter show · Esc close ", Style::new().fg(pal.fix(DIM)))))
        .padding(ratatui::widgets::Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    if app.choices.is_empty() {
        lines.push(Line::styled("No containers are running.  Enter: all processes", Style::new().fg(pal.fix(DIM))));
    }
    let list_h = (inner.height as usize).saturating_sub(if notes.is_empty() { 0 } else { notes.len() * 2 + 1 }).max(1);
    let at = at.min(app.choices.len().saturating_sub(1));
    let start = at.saturating_sub(list_h.saturating_sub(1));
    for (i, c) in app.choices.iter().enumerate().skip(start).take(list_h) {
        let current = c.filter == app.filter;
        let mut spans = vec![Span::raw(if current { "● " } else { "  " }), Span::raw("   ".repeat(c.depth as usize))];
        let style = match &c.filter {
            Filter::All | Filter::Containers => Style::new().add_modifier(Modifier::BOLD),
            Filter::Group(_) => Style::new().fg(pal.fix(MANAGER)).add_modifier(Modifier::BOLD),
            Filter::Container { .. } => Style::new(),
        };
        spans.push(Span::styled(c.label.clone(), style));
        let n = c.procs;
        spans.push(Span::styled(
            format!("  {n} process{}", if n == 1 { "" } else { "es" }),
            Style::new().fg(pal.fix(DIM)),
        ));
        let mut line = Line::from(spans);
        if i == at {
            line = line.style(Style::new().bg(pal.fix(SELECT_BG)).add_modifier(Modifier::BOLD));
        }
        lines.push(line);
    }
    if !notes.is_empty() {
        lines.push(Line::raw(""));
        for n in notes {
            lines.push(Line::styled(n, Style::new().fg(pal.fix(DIM))));
        }
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{container_procs, snap_with_containers};
    use crate::app::{App, Mode};
    use ratatui::{Terminal, backend::TestBackend};

    fn render(app: &mut App, w: u16, h: u16) {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app, &Palette { truecolor: true })).unwrap();
    }

    #[test]
    fn containers_picker_fits_small_terminals() {
        let (procs, inv) = container_procs();
        let mut app = App::new(piddle_core::sampler::Control::new(1000));
        app.apply(snap_with_containers(procs, inv));
        assert!(!app.choices.is_empty());
        // Opened at every size, then the terminal shrinks (a resize) with it open.
        for at in [0, app.choices.len() - 1] {
            for w in [1, 10, 30, 80] {
                for h in (0..=12).rev() {
                    app.mode = Mode::Containers(at);
                    render(&mut app, w, h);
                }
            }
        }
    }

    #[test]
    fn small_terminals_render_every_mode() {
        let (procs, inv) = container_procs();
        let mut app = App::new(piddle_core::sampler::Control::new(1000));
        app.apply(snap_with_containers(procs, inv));
        let modes: [fn() -> Mode; 4] = [|| Mode::Normal, || Mode::Search, || Mode::Help, || Mode::Containers(0)];
        for mode in modes {
            for (w, h) in [(1, 1), (20, 3), (40, 6), (80, 9), (80, 10)] {
                app.mode = mode();
                render(&mut app, w, h);
            }
        }
    }
}
