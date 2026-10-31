//! TUI state: rows for the current view, selection, search and the
//! confirm / details flows. Rendering lives in `ui.rs`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use piddle_core::actions::{Assessment, Signal, assess, send_signal};
use piddle_core::net::{Sockets, sockets_of};
use piddle_core::sampler::{Control, ProcInfo, Snapshot};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::TableState;

/// A process counts as "active" if it used at least this much CPU recently.
const ACTIVE_CPU: f32 = 0.5;
const ACTIVE_HOLD: Duration = Duration::from_secs(4);
/// CPU history kept for the details popup (one point per sample).
pub const HISTORY: usize = 120;
pub const INTERVALS: [u64; 4] = [250, 500, 1000, 2000];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    List,
    App,
    Parent,
    Tree,
}

impl View {
    pub fn name(self) -> &'static str {
        match self {
            View::List => "list",
            View::App => "by application",
            View::Parent => "by parent",
            View::Tree => "tree",
        }
    }
    fn next(self) -> Self {
        match self {
            View::List => View::App,
            View::App => View::Parent,
            View::Parent => View::Tree,
            View::Tree => View::List,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sort {
    Cpu,
    Mem,
    Io,
    Pid,
    Name,
}

impl Sort {
    pub fn name(self) -> &'static str {
        match self {
            Sort::Cpu => "CPU",
            Sort::Mem => "memory",
            Sort::Io => "disk I/O",
            Sort::Pid => "PID",
            Sort::Name => "name",
        }
    }
    fn next(self) -> Self {
        match self {
            Sort::Cpu => Sort::Mem,
            Sort::Mem => Sort::Io,
            Sort::Io => Sort::Pid,
            Sort::Pid => Sort::Name,
            Sort::Name => Sort::Cpu,
        }
    }
}

pub enum Row {
    Group { key: String, label: String, count: usize, cpu: f32, rss: u64, io: (f64, f64), collapsed: bool },
    Proc { info: ProcInfo, prefix: String },
}

pub enum Mode {
    Normal,
    Search,
    Details,
    /// Simple yes/no confirmation (default No).
    Confirm(Pending),
    /// Critical process: the name must be typed.
    ConfirmTyped(Pending, String),
    Help,
}

pub struct Pending {
    pub sig: Signal,
    pub info: ProcInfo,
    pub assessment: Assessment,
}

pub struct App {
    pub control: Arc<Control>,
    pub snap: Option<Snapshot>,
    pub rows: Vec<Row>,
    pub view: View,
    pub sort: Sort,
    pub query: String,
    pub show_kernel: bool,
    pub active_only: bool,
    pub paused: bool,
    pub table: TableState,
    pub mode: Mode,
    pub status: Option<(String, Instant)>,
    pub history: VecDeque<f32>,
    pub max_rss: u64,
    selected_pid: Option<i32>,
    history_pid: Option<i32>,
    /// Sockets of the process in the details popup, with when they were read.
    pub detail_net: Option<(Instant, Option<Sockets>)>,
    /// Show the disk I/O columns (and collect I/O in the sampler).
    pub show_io: bool,
    activity: HashMap<i32, Instant>,
    collapsed: HashSet<String>,
    names: HashMap<i32, Arc<str>>,
    pub quit: bool,
}

impl App {
    pub fn new(control: Arc<Control>) -> Self {
        Self {
            control,
            snap: None,
            rows: Vec::new(),
            view: View::List,
            sort: Sort::Cpu,
            query: String::new(),
            show_kernel: false,
            active_only: false,
            paused: false,
            table: TableState::default().with_selected(Some(0)),
            mode: Mode::Normal,
            status: None,
            history: VecDeque::with_capacity(HISTORY),
            max_rss: 1,
            selected_pid: None,
            history_pid: None,
            detail_net: None,
            show_io: true,
            activity: HashMap::new(),
            collapsed: HashSet::new(),
            names: HashMap::new(),
            quit: false,
        }
    }

    pub fn interval_ms(&self) -> u64 {
        self.control.interval_ms.load(Ordering::Relaxed)
    }

    pub fn name_of(&self, pid: i32) -> Option<&Arc<str>> {
        self.names.get(&pid)
    }

    pub fn selected(&self) -> Option<&ProcInfo> {
        match self.rows.get(self.table.selected()?)? {
            Row::Proc { info, .. } => Some(info),
            Row::Group { .. } => None,
        }
    }

    /// Latest data for the process shown in the details popup.
    pub fn detail_proc(&self) -> Option<&ProcInfo> {
        let pid = self.history_pid?;
        self.snap.as_ref()?.procs.iter().find(|p| p.pid == pid)
    }

    pub fn set_status(&mut self, s: impl Into<String>) {
        self.status = Some((s.into(), Instant::now()));
    }

    pub fn apply(&mut self, snap: Snapshot) {
        let now = Instant::now();
        for p in &snap.procs {
            if p.cpu >= ACTIVE_CPU {
                self.activity.insert(p.pid, now);
            }
        }
        self.activity.retain(|_, t| now.duration_since(*t) < ACTIVE_HOLD);
        self.names = snap.procs.iter().map(|p| (p.pid, p.name.clone())).collect();
        self.max_rss = snap.procs.iter().map(|p| p.rss).max().unwrap_or(1).max(1);
        if let Some(pid) = self.history_pid {
            match snap.procs.iter().find(|p| p.pid == pid) {
                Some(p) => {
                    if self.history.len() == HISTORY {
                        self.history.pop_front();
                    }
                    self.history.push_back(p.cpu);
                }
                None => self.history_pid = None,
            }
        }
        self.snap = Some(snap);
        self.refresh_net(false);
        self.rebuild();
    }

    fn visible(&self, p: &ProcInfo) -> bool {
        if p.kthread && !self.show_kernel {
            return false;
        }
        if self.active_only && !self.activity.contains_key(&p.pid) {
            return false;
        }
        if self.query.is_empty() {
            return true;
        }
        let q = self.query.to_lowercase();
        let numeric = q.bytes().all(|c| c.is_ascii_digit());
        (numeric && p.pid.to_string().starts_with(&q)) || p.search_text.contains(&q)
    }

    fn cmp(&self, a: &ProcInfo, b: &ProcInfo) -> std::cmp::Ordering {
        match self.sort {
            Sort::Cpu => b.cpu.total_cmp(&a.cpu).then(b.rss.cmp(&a.rss)),
            Sort::Mem => b.rss.cmp(&a.rss),
            Sort::Io => io_rate(b).total_cmp(&io_rate(a)).then(b.cpu.total_cmp(&a.cpu)),
            Sort::Pid => a.pid.cmp(&b.pid),
            Sort::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.pid.cmp(&b.pid)),
        }
    }

    /// Recompute rows for the current view, keeping the selection on the same process.
    pub fn rebuild(&mut self) {
        let Some(snap) = &self.snap else { return };
        let mut procs: Vec<&ProcInfo> = snap.procs.iter().filter(|p| self.visible(p)).collect();
        procs.sort_by(|a, b| self.cmp(a, b));
        let mut rows = Vec::with_capacity(procs.len() + 16);

        match self.view {
            View::List => {
                rows.extend(procs.iter().map(|p| Row::Proc { info: (*p).clone(), prefix: String::new() }));
            }
            View::App | View::Parent => {
                let mut groups: Vec<(String, String, Vec<&ProcInfo>)> = Vec::new();
                let mut index: HashMap<String, usize> = HashMap::new();
                for p in &procs {
                    let (key, label) = match self.view {
                        View::App => (p.app.to_string(), p.app.to_string()),
                        _ => {
                            let label = match self.names.get(&p.ppid) {
                                Some(n) => format!("{n} ({})", p.ppid),
                                None if p.ppid == 0 => "kernel".to_string(),
                                None => format!("PID {}", p.ppid),
                            };
                            (format!("p{}", p.ppid), label)
                        }
                    };
                    let i = *index.entry(key.clone()).or_insert_with(|| {
                        groups.push((key, label, Vec::new()));
                        groups.len() - 1
                    });
                    groups[i].2.push(p);
                }
                let sum =
                    |g: &Vec<&ProcInfo>| (g.iter().map(|p| p.cpu).sum::<f32>(), g.iter().map(|p| p.rss).sum::<u64>());
                let io_sum = |g: &Vec<&ProcInfo>| {
                    g.iter().filter_map(|p| p.io).fold((0.0, 0.0), |(r, w), io| (r + io.read_rate, w + io.write_rate))
                };
                groups.sort_by(|a, b| {
                    let ((ca, ma), (cb, mb)) = (sum(&a.2), sum(&b.2));
                    match self.sort {
                        Sort::Cpu => cb.total_cmp(&ca).then(mb.cmp(&ma)),
                        Sort::Mem => mb.cmp(&ma),
                        Sort::Io => {
                            let (ia, ib) = (io_sum(&a.2), io_sum(&b.2));
                            (ib.0 + ib.1).total_cmp(&(ia.0 + ia.1))
                        }
                        Sort::Pid => a.2[0].pid.cmp(&b.2[0].pid),
                        Sort::Name => a.1.to_lowercase().cmp(&b.1.to_lowercase()),
                    }
                });
                for (key, label, members) in groups {
                    let (cpu, rss) = sum(&members);
                    let io = io_sum(&members);
                    let collapsed = self.collapsed.contains(&key);
                    if members.len() == 1 && self.view == View::App {
                        rows.push(Row::Proc { info: members[0].clone(), prefix: String::new() });
                        continue;
                    }
                    rows.push(Row::Group { key, label, count: members.len(), cpu, rss, io, collapsed });
                    if !collapsed {
                        let n = members.len();
                        for (i, p) in members.into_iter().enumerate() {
                            let prefix = if i + 1 == n { "└─ " } else { "├─ " };
                            rows.push(Row::Proc { info: p.clone(), prefix: prefix.into() });
                        }
                    }
                }
            }
            View::Tree => {
                let present: HashSet<i32> = procs.iter().map(|p| p.pid).collect();
                let mut children: HashMap<i32, Vec<&ProcInfo>> = HashMap::new();
                let mut roots = Vec::new();
                for p in &procs {
                    if p.ppid != p.pid && present.contains(&p.ppid) {
                        children.entry(p.ppid).or_default().push(p);
                    } else {
                        roots.push(*p);
                    }
                }
                // procs is already sorted, so children lists are too.
                let mut stack: Vec<(&ProcInfo, String, bool, bool)> =
                    roots.iter().rev().map(|p| (*p, String::new(), true, true)).collect();
                while let Some((p, indent, last, root)) = stack.pop() {
                    let collapsed = self.collapsed.contains(&format!("t{}", p.pid));
                    let kids = children.get(&p.pid).map(Vec::as_slice).unwrap_or(&[]);
                    let branch = if root {
                        String::new()
                    } else {
                        format!("{indent}{}", if last { "└─ " } else { "├─ " })
                    };
                    let marker = if kids.is_empty() {
                        ""
                    } else if collapsed {
                        "▸ "
                    } else {
                        "▾ "
                    };
                    rows.push(Row::Proc { info: p.clone(), prefix: format!("{branch}{marker}") });
                    if collapsed {
                        continue;
                    }
                    let child_indent =
                        if root { String::new() } else { format!("{indent}{}", if last { "   " } else { "│  " }) };
                    for (i, c) in kids.iter().enumerate().rev() {
                        stack.push((c, child_indent.clone(), i + 1 == kids.len(), false));
                    }
                }
            }
        }

        self.rows = rows;
        // Restore selection by PID; otherwise clamp.
        let idx = self
            .selected_pid
            .and_then(|pid| self.rows.iter().position(|r| matches!(r, Row::Proc { info, .. } if info.pid == pid)))
            .or(self.table.selected())
            .unwrap_or(0)
            .min(self.rows.len().saturating_sub(1));
        self.table.select(Some(idx));
    }

    fn remember_selection(&mut self) {
        self.selected_pid = self.selected().map(|p| p.pid);
    }

    fn move_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let cur = self.table.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, self.rows.len() as isize - 1) as usize;
        self.table.select(Some(next));
        self.remember_selection();
    }

    fn toggle_collapse(&mut self) {
        let key = match self.table.selected().and_then(|i| self.rows.get(i)) {
            Some(Row::Group { key, .. }) => key.clone(),
            Some(Row::Proc { info, .. }) if self.view == View::Tree => format!("t{}", info.pid),
            _ => return,
        };
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        self.rebuild();
    }

    fn open_details(&mut self) {
        if let Some(p) = self.selected() {
            let (pid, cpu) = (p.pid, p.cpu);
            if self.history_pid != Some(pid) {
                self.history.clear();
                self.history.push_back(cpu);
                self.history_pid = Some(pid);
            }
            self.mode = Mode::Details;
            self.refresh_net(true);
        }
    }

    /// Re-read the details process's sockets every couple of seconds while
    /// the popup is open (this walks its file descriptors, so not every sample).
    fn refresh_net(&mut self, force: bool) {
        if !matches!(self.mode, Mode::Details) {
            self.detail_net = None;
            return;
        }
        let stale = self.detail_net.as_ref().is_none_or(|(at, _)| at.elapsed() >= Duration::from_secs(2));
        if let Some(pid) = self.history_pid
            && (force || stale)
        {
            self.detail_net = Some((Instant::now(), sockets_of(pid)));
        }
    }

    fn request_signal(&mut self, sig: Signal) {
        let info = match self.mode {
            Mode::Details => self.detail_proc().cloned(),
            _ => self.selected().cloned(),
        };
        let Some(info) = info else { return };
        let Some(snap) = &self.snap else { return };
        let assessment = assess(&info, &snap.procs, std::process::id() as i32);
        if let Some(why) = &assessment.blocked {
            self.set_status(format!("Not allowed: {why}"));
            return;
        }
        let pending = Pending { sig, info, assessment };
        self.mode = if pending.assessment.is_critical() {
            Mode::ConfirmTyped(pending, String::new())
        } else {
            Mode::Confirm(pending)
        };
    }

    fn execute(&mut self, p: Pending) {
        let msg = match send_signal(p.info.pid, p.info.start_ticks, p.sig) {
            Ok(()) => format!("Sent {} to {} ({}).", p.sig.name(), p.info.name, p.info.pid),
            Err(e) => e,
        };
        self.set_status(msg);
        self.mode = Mode::Normal;
    }

    fn set_interval(&mut self, step: isize) {
        let cur = INTERVALS.iter().position(|&ms| ms == self.interval_ms()).unwrap_or(1) as isize;
        let next = INTERVALS[(cur + step).clamp(0, INTERVALS.len() as isize - 1) as usize];
        self.control.interval_ms.store(next, Ordering::Relaxed);
        self.set_status(format!("Sampling every {next} ms"));
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Search => match key.code {
                KeyCode::Esc => {
                    self.query.clear();
                    self.rebuild();
                }
                KeyCode::Enter => {}
                KeyCode::Backspace => {
                    self.query.pop();
                    self.rebuild();
                    self.mode = Mode::Search;
                }
                KeyCode::Char(c) => {
                    self.query.push(c);
                    self.rebuild();
                    self.mode = Mode::Search;
                }
                KeyCode::Up | KeyCode::Down => {
                    self.move_by(if key.code == KeyCode::Up { -1 } else { 1 });
                    self.mode = Mode::Search;
                }
                _ => self.mode = Mode::Search,
            },
            Mode::Details => match key.code {
                KeyCode::Char('t') => self.request_signal(Signal::Terminate),
                KeyCode::Char('x') => self.request_signal(Signal::Kill),
                KeyCode::Char('q') => self.quit = true,
                KeyCode::Esc | KeyCode::Enter => {}
                _ => self.mode = Mode::Details,
            },
            Mode::Confirm(p) => {
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.execute(p);
                } else {
                    self.set_status("Cancelled.");
                }
            }
            Mode::ConfirmTyped(p, mut typed) => match key.code {
                KeyCode::Esc => self.set_status("Cancelled."),
                KeyCode::Enter if typed.trim() == &*p.info.name => self.execute(p),
                KeyCode::Enter => self.mode = Mode::ConfirmTyped(p, typed),
                KeyCode::Backspace => {
                    typed.pop();
                    self.mode = Mode::ConfirmTyped(p, typed);
                }
                KeyCode::Char(c) => {
                    typed.push(c);
                    self.mode = Mode::ConfirmTyped(p, typed);
                }
                _ => self.mode = Mode::ConfirmTyped(p, typed),
            },
            Mode::Help => {}
            Mode::Normal => self.on_normal_key(key),
        }
    }

    fn on_normal_key(&mut self, key: KeyEvent) {
        let page = 15;
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc if !self.query.is_empty() => {
                self.query.clear();
                self.rebuild();
            }
            KeyCode::Char('/') => self.mode = Mode::Search,
            KeyCode::Char('?') | KeyCode::F(1) => self.mode = Mode::Help,
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::PageUp => self.move_by(-page),
            KeyCode::PageDown => self.move_by(page),
            KeyCode::Home => self.move_by(-(self.rows.len() as isize)),
            KeyCode::End => self.move_by(self.rows.len() as isize),
            KeyCode::Enter => match self.table.selected().and_then(|i| self.rows.get(i)) {
                Some(Row::Group { .. }) => self.toggle_collapse(),
                Some(Row::Proc { .. }) => self.open_details(),
                None => {}
            },
            KeyCode::Char(' ') => self.toggle_collapse(),
            KeyCode::Char('v') => {
                self.view = self.view.next();
                self.rebuild();
                self.set_status(format!("View: {}", self.view.name()));
            }
            KeyCode::Char('1') => self.switch_view(View::List),
            KeyCode::Char('2') => self.switch_view(View::App),
            KeyCode::Char('3') => self.switch_view(View::Parent),
            KeyCode::Char('4') => self.switch_view(View::Tree),
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.rebuild();
                self.set_status(format!("Sorted by {}", self.sort.name()));
            }
            KeyCode::Char('a') => {
                self.active_only = !self.active_only;
                self.rebuild();
                self.set_status(if self.active_only {
                    "Showing active processes only"
                } else {
                    "Showing all processes"
                });
            }
            KeyCode::Char('i') => {
                self.show_io = !self.show_io;
                self.control.collect_io.store(self.show_io, Ordering::Relaxed);
                self.set_status(if self.show_io { "Disk I/O columns shown" } else { "Disk I/O columns hidden" });
            }
            KeyCode::Char('K') => {
                self.show_kernel = !self.show_kernel;
                self.rebuild();
                self.set_status(if self.show_kernel { "Kernel threads shown" } else { "Kernel threads hidden" });
            }
            KeyCode::Char('p') => {
                self.paused = !self.paused;
                self.control.paused.store(self.paused, Ordering::Relaxed);
                self.set_status(if self.paused { "Paused" } else { "Resumed" });
            }
            KeyCode::Char('+') | KeyCode::Char('=') => self.set_interval(1),
            KeyCode::Char('-') => self.set_interval(-1),
            KeyCode::Char('t') => self.request_signal(Signal::Terminate),
            KeyCode::Char('x') => self.request_signal(Signal::Kill),
            _ => {}
        }
    }

    fn switch_view(&mut self, v: View) {
        self.view = v;
        self.rebuild();
        self.set_status(format!("View: {}", v.name()));
    }
}

/// Combined read + write bytes per second (0 when unknown).
pub fn io_rate(p: &ProcInfo) -> f64 {
    p.io.map(|io| io.read_rate + io.write_rate).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEvent;

    fn proc(pid: i32, ppid: i32, name: &str, cpu: f32) -> ProcInfo {
        let n: Arc<str> = Arc::from(name);
        ProcInfo {
            pid,
            ppid,
            name: n.clone(),
            app: n.clone(),
            cmdline: n.clone(),
            search_text: Arc::from(name.to_lowercase()),
            user: Arc::from("u"),
            uid: 1000,
            state: 'S',
            cpu,
            rss: 1 << 20,
            vsize: 0,
            threads: 1,
            nice: 0,
            start_ticks: 0,
            kthread: false,
            io: None,
        }
    }

    fn app_with(procs: Vec<ProcInfo>) -> App {
        let mut app = App::new(Control::new(1000));
        app.apply(Snapshot {
            procs,
            ncpu: 4,
            clk_tck: 100,
            boot_time: 0,
            total_cpu: 0.0,
            mem_total: 1 << 30,
            mem_used: 0,
            sample_cost: Duration::ZERO,
            self_pid: 99_999,
        });
        app
    }

    fn sample() -> Vec<ProcInfo> {
        vec![
            proc(1, 0, "systemd", 0.0),
            proc(10, 1, "sshd", 1.0),
            proc(11, 10, "bash", 0.0),
            proc(12, 11, "vim", 50.0),
            proc(20, 1, "cron", 0.0),
        ]
    }

    fn names(app: &App) -> Vec<String> {
        app.rows
            .iter()
            .map(|r| match r {
                Row::Proc { info, prefix } => format!("{prefix}{}", info.name),
                Row::Group { label, count, .. } => format!("[{label} ×{count}]"),
            })
            .collect()
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn tree_prefixes_and_collapse() {
        let mut app = app_with(sample());
        app.switch_view(View::Tree);
        assert_eq!(names(&app), ["▾ systemd", "├─ ▾ sshd", "│  └─ ▾ bash", "│     └─ vim", "└─ cron"]);
        // Collapse sshd: its subtree disappears.
        app.table.select(Some(1));
        app.on_key(key(KeyCode::Char(' ')));
        assert_eq!(names(&app), ["▾ systemd", "├─ ▸ sshd", "└─ cron"]);
    }

    #[test]
    fn search_by_name_and_pid() {
        let mut app = app_with(sample());
        app.on_key(key(KeyCode::Char('/')));
        for c in "vi".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(names(&app), ["vim"]);
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.rows.len(), 5);
        app.query = "2".into();
        app.rebuild();
        assert_eq!(names(&app), ["cron"]); // PID prefix "2" matches 20 only
    }

    #[test]
    fn selection_follows_process_across_resort() {
        let mut app = app_with(sample());
        // Sorted by CPU: vim first, sshd second.
        app.table.select(Some(1));
        app.remember_selection();
        assert_eq!(app.selected().unwrap().name.as_ref(), "sshd");
        app.on_key(key(KeyCode::Char('s'))); // sort by memory (all equal)
        app.on_key(key(KeyCode::Char('s'))); // sort by disk I/O
        app.on_key(key(KeyCode::Char('s'))); // sort by PID
        assert_eq!(app.selected().unwrap().name.as_ref(), "sshd");
    }

    #[test]
    fn critical_kill_needs_exact_name() {
        let mut procs = sample();
        procs.push(proc(30, 1, "gnome-shell", 5.0));
        let mut app = app_with(procs);
        let row = app.rows.iter().position(|r| matches!(r, Row::Proc { info, .. } if info.pid == 30)).unwrap();
        app.table.select(Some(row));
        app.on_key(key(KeyCode::Char('x')));
        assert!(matches!(app.mode, Mode::ConfirmTyped(..)));
        for c in "gnome".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter)); // wrong name: still waiting
        assert!(matches!(app.mode, Mode::ConfirmTyped(..)));
        app.on_key(key(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn pid_one_is_blocked() {
        let mut app = app_with(sample());
        app.switch_view(View::Tree);
        app.table.select(Some(0)); // systemd, PID 1
        app.on_key(key(KeyCode::Char('t')));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.status.as_ref().unwrap().0.starts_with("Not allowed"));
    }
}
