//! Rendering. Colors follow the GUI: CPU load runs from cool blue through
//! yellow to hot red, where 100% is one fully used core.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use piddle_core::actions::{exe_path, fmt_age, state_name};
use piddle_core::encode::{cpu_color, fmt_bytes, fmt_cpu};
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
        Mode::Help => draw_help(f, pal),
        _ => {}
    }
}

fn draw_header(f: &mut Frame, app: &App, pal: &Palette, area: Rect) {
    let Some(s) = &app.snap else {
        f.render_widget(Paragraph::new("Sampling /proc…").style(Style::new().fg(pal.fix(DIM))), area);
        return;
    };
    let [l1, l2, l3] = Layout::vertical([Constraint::Length(1); 3]).areas(area);
    let mem_frac = if s.mem_total > 0 { s.mem_used as f64 / s.mem_total as f64 } else { 0.0 };
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
    f.render_widget(
        meter(
            " CPU ",
            s.total_cpu as f64 / 100.0,
            &format!("{:.1}% of all cores", s.total_cpu),
            pal.cpu(s.total_cpu as f64),
            pal,
        ),
        l2,
    );
    f.render_widget(
        meter(
            " MEM ",
            mem_frac,
            &format!(
                "{} / {}   ·   PIDdle {} CPU, {:.1} ms per sample",
                fmt_bytes(s.mem_used),
                fmt_bytes(s.mem_total),
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
    let header = TRow::new(["PID", "NAME", "USER", "CPU", "", "MEM", "", "THR", "S"])
        .style(Style::new().fg(pal.fix(DIM)).add_modifier(Modifier::BOLD));
    let max_rss = app.max_rss as f64;
    let rows: Vec<TRow> = app
        .rows
        .iter()
        .map(|r| match r {
            Row::Group { label, count, cpu, rss, collapsed, .. } => {
                let c = pal.cpu(*cpu as f64);
                TRow::new(vec![
                    Cell::from(if *collapsed { "▸" } else { "▾" }),
                    Cell::from(Line::from(vec![
                        Span::styled(label.clone(), Style::new().add_modifier(Modifier::BOLD)),
                        Span::styled(format!("  ×{count}"), Style::new().fg(pal.fix(DIM))),
                    ])),
                    Cell::from(""),
                    Cell::from(Span::styled(fmt_cpu(*cpu as f64), Style::new().fg(c).add_modifier(Modifier::BOLD))),
                    Cell::from(Span::styled(cpu_bar(*cpu as f64, 10), Style::new().fg(c))),
                    Cell::from(fmt_bytes(*rss)),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(""),
                ])
                .style(Style::new().bg(pal.fix(Color::Rgb(22, 26, 34))))
            }
            Row::Proc { info: p, prefix } => {
                let c = pal.cpu(p.cpu as f64);
                let mem_frac = (p.rss as f64 / max_rss).sqrt();
                let mem_w = (mem_frac * 6.0).round() as usize;
                TRow::new(vec![
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
                    Cell::from(p.threads.to_string()),
                    Cell::from(Span::styled(p.state.to_string(), Style::new().fg(pal.fix(DIM)))),
                ])
            }
        })
        .collect();

    let title = if app.query.is_empty() {
        Line::from(format!(" {} rows ", app.rows.len()))
    } else {
        Line::from(vec![
            Span::raw(" search: "),
            Span::styled(app.query.clone(), Style::new().fg(pal.fix(ACCENT)).add_modifier(Modifier::BOLD)),
            Span::raw(format!(" · {} rows ", app.rows.len())),
        ])
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Fill(1),
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Length(4),
            Constraint::Length(1),
        ],
    )
    .header(header)
    .block(
        Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(pal.fix(BORDER))).title(title),
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
        Line::from(vec![
            key(" ↑↓"),
            txt(" move  "),
            key("Enter"),
            txt(" details  "),
            key("/"),
            txt(" search  "),
            key("v"),
            txt(" view  "),
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
        ])
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
    let area = popup(f.area(), 84, 24);
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
        .map(|since_boot| {
            let at = s.boot_time + since_boot;
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(at);
            format!("{} ago", fmt_age(now.saturating_sub(at)))
        })
        .unwrap_or_default();
    let parent = match app.name_of(p.ppid) {
        Some(n) => format!("{n} ({})", p.ppid),
        None => p.ppid.to_string(),
    };
    let mem_pct = if s.mem_total > 0 { p.rss as f64 / s.mem_total as f64 * 100.0 } else { 0.0 };
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
            Span::raw(format!("{} resident ({mem_pct:.1}% of RAM) · {} virtual", fmt_bytes(p.rss), fmt_bytes(p.vsize))),
        ]),
        Line::from(vec![label("Threads"), Span::raw(format!("{}   nice {}", p.threads, p.nice))]),
        Line::from(vec![label("Parent"), Span::raw(parent)]),
        Line::from(vec![label("Started"), Span::raw(started)]),
        Line::from(vec![label("Executable"), Span::raw(exe_path(p.pid).unwrap_or_else(|| "not accessible".into()))]),
        Line::from(vec![
            label("Command"),
            Span::raw(if p.cmdline.is_empty() { format!("[{}]", p.name) } else { p.cmdline.to_string() }),
        ]),
    ];
    if p.kthread {
        lines.push(Line::styled("Kernel thread", Style::new().fg(pal.fix(DIM))));
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
            key("Esc"),
            txt(" close"),
        ])),
        keys,
    );
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

fn draw_help(f: &mut Frame, pal: &Palette) {
    let rows = [
        ("↑ ↓ PgUp PgDn Home End", "move"),
        ("Enter", "details (or collapse a group)"),
        ("Space", "collapse / expand group or tree branch"),
        ("/", "search name, command or PID; Esc clears"),
        ("v or 1–4", "view: list, by application, by parent, tree"),
        ("s", "sort: CPU, memory, PID, name"),
        ("a", "active processes only"),
        ("K", "show / hide kernel threads"),
        ("+ / -", "sampling interval"),
        ("p", "pause / resume"),
        ("t", "terminate (SIGTERM)"),
        ("x", "force kill (SIGKILL)"),
        ("q, Ctrl+C", "quit"),
    ];
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
