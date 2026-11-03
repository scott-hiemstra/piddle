//! TUI state: rows for the current view, selection, search and the
//! confirm / details flows. Rendering lives in `ui.rs`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use piddle_core::actions::{Assessment, Signal, assess, send_signal};
use piddle_core::containers::{self, Choice, Filter, GroupKind, Placement};
use piddle_core::logs::{self, Entry, Query, Stream};
use piddle_core::net::{Sockets, sockets_of};
use piddle_core::sampler::{Control, ProcInfo, Snapshot};
use piddle_core::service::ServiceKind;
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
    Service,
    Parent,
    Tree,
    /// By Compose project, pod and container; offered only while containers run.
    Containers,
}

impl View {
    pub fn name(self) -> &'static str {
        match self {
            View::List => "list",
            View::App => "by application",
            View::Service => "by service",
            View::Parent => "by parent",
            View::Tree => "tree",
            View::Containers => "by container",
        }
    }
    /// The next view; `containers` says whether "by container" is offered.
    fn next(self, containers: bool) -> Self {
        match self {
            View::List => View::App,
            View::App => View::Service,
            View::Service => View::Parent,
            View::Parent => View::Tree,
            View::Tree if containers => View::Containers,
            View::Tree | View::Containers => View::List,
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
    Group {
        key: String,
        label: String,
        /// Set in the service view, and for containers in the containers view.
        kind: Option<ServiceKind>,
        /// Compose project or pod rows in the containers view.
        manager: Option<GroupKind>,
        /// 1 for containers inside a project or pod.
        depth: u8,
        count: usize,
        cpu: f32,
        rss: u64,
        io: (f64, f64),
        collapsed: bool,
    },
    Proc {
        info: ProcInfo,
        prefix: String,
    },
}

pub enum Mode {
    Normal,
    Search,
    Details,
    /// Simple yes/no confirmation (default No).
    Confirm(Pending),
    /// Critical process: the name must be typed.
    ConfirmTyped(Pending, String),
    /// Journal of the details process's service (opened from Details).
    Logs,
    Help,
    /// The containers picker, with the highlighted entry.
    Containers(usize),
}

/// Lines kept in the logs view.
const MAX_LOG_LINES: usize = 5000;

pub struct LogView {
    pub name: String,
    service: Query,
    pub per_pid: Option<Query>,
    pub pid_only: bool,
    pub command: String,
    pub lines: VecDeque<Entry>,
    /// Shown above the lines: missing permissions, journalctl errors.
    pub notice: Option<String>,
    /// Lines scrolled up from the bottom; 0 follows new entries.
    pub scroll: usize,
    stream: Option<Stream>,
}

impl LogView {
    fn start(&mut self) {
        self.stream = None;
        self.lines.clear();
        self.scroll = 0;
        let q = match (&self.per_pid, self.pid_only) {
            (Some(p), true) => p,
            _ => &self.service,
        };
        self.command = q.command_line();
        self.notice = logs::hidden_by_permissions(q).then(|| logs::NO_SYSTEM_JOURNAL.to_string());
        match logs::follow(q) {
            Ok(s) => self.stream = Some(s),
            Err(why) => self.notice = Some(why),
        }
    }

    /// Take queued entries; true if anything changed.
    fn poll(&mut self) -> bool {
        let Some(s) = &self.stream else { return false };
        let batch = logs::take_queued(&s.rx, None);
        let changed = !batch.entries.is_empty() || batch.ended.is_some();
        for e in batch.entries {
            if self.lines.len() == MAX_LOG_LINES {
                self.lines.pop_front();
            }
            self.lines.push_back(e);
            // Keep the view still while scrolled up.
            if self.scroll > 0 {
                self.scroll += 1;
            }
        }
        if let Some(why) = batch.ended {
            self.notice = Some(format!("journalctl stopped: {why}"));
        }
        changed
    }
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
    pub logs: Option<LogView>,
    selected_pid: Option<i32>,
    history_pid: Option<i32>,
    /// Start time of the details process: with the PID, its identity, so a
    /// reused PID is never mistaken for it.
    history_start: u64,
    /// Sockets of the process in the details popup, with when they were read.
    pub detail_net: Option<(Instant, Option<Sockets>)>,
    /// Show the disk I/O columns (and collect I/O in the sampler).
    pub show_io: bool,
    activity: HashMap<i32, Instant>,
    collapsed: HashSet<String>,
    names: HashMap<i32, Arc<str>>,
    /// What the containers picker limits the view to.
    pub filter: Filter,
    /// The picker's entries; empty when no process is in a container.
    pub choices: Vec<Choice>,
    /// The filter's container or project is gone; the view says so.
    pub filter_gone: bool,
    /// Each service's container placement, for this snapshot.
    placed: HashMap<usize, Option<Placement>>,
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
            logs: None,
            mode: Mode::Normal,
            status: None,
            history: VecDeque::with_capacity(HISTORY),
            max_rss: 1,
            selected_pid: None,
            history_pid: None,
            history_start: 0,
            detail_net: None,
            show_io: true,
            activity: HashMap::new(),
            collapsed: HashSet::new(),
            names: HashMap::new(),
            filter: Filter::All,
            choices: Vec::new(),
            filter_gone: false,
            placed: HashMap::new(),
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
        self.snap.as_ref()?.procs.iter().find(|p| p.pid == pid && p.start_ticks == self.history_start)
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
            match snap.procs.iter().find(|p| p.pid == pid && p.start_ticks == self.history_start) {
                Some(p) => {
                    if self.history.len() == HISTORY {
                        self.history.pop_front();
                    }
                    self.history.push_back(p.cpu);
                }
                None => self.history_pid = None,
            }
        }
        self.placed.clear();
        for p in &snap.procs {
            let k = Arc::as_ptr(&p.service) as usize;
            self.placed.entry(k).or_insert_with(|| containers::place(&p.service, &snap.containers));
        }
        self.choices = containers::choices(&snap.procs, &snap.containers);
        match containers::resolve(&self.filter, &self.choices) {
            Some(f) => {
                self.filter = f;
                self.filter_gone = false;
            }
            None => self.filter_gone = true,
        }
        if self.choices.is_empty() && self.view == View::Containers && self.filter.is_all() {
            self.view = View::List;
        }
        self.snap = Some(snap);
        self.refresh_net(false);
        self.rebuild();
    }

    /// Where a process's container sits; `None` outside containers.
    pub fn placement(&self, p: &ProcInfo) -> Option<&Placement> {
        self.placed.get(&(Arc::as_ptr(&p.service) as usize))?.as_ref()
    }

    /// The containers line for the header: what the view is limited to.
    pub fn filter_text(&self) -> Option<String> {
        if self.filter.is_all() {
            return None;
        }
        let name = containers::filter_label(&self.filter, &self.choices);
        Some(if self.filter_gone {
            format!("{name} is no longer running · c: choose another · Esc: all processes")
        } else if self.filter == Filter::Containers {
            "containers only · c: change · Esc: all processes".to_string()
        } else {
            format!("only {name} · c: change · Esc: all processes")
        })
    }

    fn visible(&self, p: &ProcInfo) -> bool {
        if p.kthread && !self.show_kernel {
            return false;
        }
        if !self.filter.is_all() && (self.filter_gone || !self.filter.shows(self.placement(p))) {
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
        (numeric && p.pid.to_string().starts_with(&q)) || p.search_text.contains(&q) || p.service.search.contains(&q)
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
            View::App | View::Service | View::Parent => {
                let mut groups: Vec<(String, String, Option<ServiceKind>, Vec<&ProcInfo>)> = Vec::new();
                let mut index: HashMap<String, usize> = HashMap::new();
                for p in &procs {
                    let (key, label, kind) = match self.view {
                        View::App => (p.app.to_string(), p.app.to_string(), None),
                        View::Service => {
                            (format!("s{}", p.service.key), p.service.label.to_string(), Some(p.service.kind))
                        }
                        _ => {
                            let label = match self.names.get(&p.ppid) {
                                Some(n) => format!("{n} ({})", p.ppid),
                                None if p.ppid == 0 => "kernel".to_string(),
                                None => format!("PID {}", p.ppid),
                            };
                            (format!("p{}", p.ppid), label, None)
                        }
                    };
                    let i = *index.entry(key.clone()).or_insert_with(|| {
                        groups.push((key, label, kind, Vec::new()));
                        groups.len() - 1
                    });
                    groups[i].3.push(p);
                }
                let sum =
                    |g: &Vec<&ProcInfo>| (g.iter().map(|p| p.cpu).sum::<f32>(), g.iter().map(|p| p.rss).sum::<u64>());
                let io_sum = |g: &Vec<&ProcInfo>| {
                    g.iter().filter_map(|p| p.io).fold((0.0, 0.0), |(r, w), io| (r + io.read_rate, w + io.write_rate))
                };
                groups.sort_by(|a, b| {
                    let ((ca, ma), (cb, mb)) = (sum(&a.3), sum(&b.3));
                    match self.sort {
                        Sort::Cpu => cb.total_cmp(&ca).then(mb.cmp(&ma)),
                        Sort::Mem => mb.cmp(&ma),
                        Sort::Io => {
                            let (ia, ib) = (io_sum(&a.3), io_sum(&b.3));
                            (ib.0 + ib.1).total_cmp(&(ia.0 + ia.1))
                        }
                        Sort::Pid => a.3[0].pid.cmp(&b.3[0].pid),
                        Sort::Name => a.1.to_lowercase().cmp(&b.1.to_lowercase()),
                    }
                });
                for (key, label, kind, members) in groups {
                    let (cpu, rss) = sum(&members);
                    let io = io_sum(&members);
                    let collapsed = self.collapsed.contains(&key);
                    if members.len() == 1 && self.view == View::App {
                        rows.push(Row::Proc { info: members[0].clone(), prefix: String::new() });
                        continue;
                    }
                    rows.push(Row::Group {
                        key,
                        label,
                        kind,
                        manager: None,
                        depth: 0,
                        count: members.len(),
                        cpu,
                        rss,
                        io,
                        collapsed,
                    });
                    if !collapsed {
                        let n = members.len();
                        for (i, p) in members.into_iter().enumerate() {
                            let prefix = if i + 1 == n { "└─ " } else { "├─ " };
                            rows.push(Row::Proc { info: p.clone(), prefix: prefix.into() });
                        }
                    }
                }
            }
            View::Containers => self.container_rows(&procs, &mut rows),
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

    /// Rows for the containers view: each Compose project or pod with its
    /// totals, its containers inside it (a service's replicas side by side),
    /// then standalone containers, then everything outside containers.
    fn container_rows(&self, procs: &[&ProcInfo], rows: &mut Vec<Row>) {
        struct Agg<'a> {
            key: String,
            label: String,
            order: Arc<str>,
            procs: Vec<&'a ProcInfo>,
        }
        type Top<'a> = (Option<(String, String, GroupKind)>, Vec<Agg<'a>>);
        let mut tops: Vec<Top> = Vec::new();
        let mut top_index: HashMap<String, usize> = HashMap::new();
        let mut host: Vec<&ProcInfo> = Vec::new();
        for p in procs {
            let Some(pl) = self.placement(p) else {
                host.push(p);
                continue;
            };
            let tkey = pl.group.as_ref().map_or_else(|| format!("c{}", pl.key), |g| format!("g{}", g.key));
            let ti = *top_index.entry(tkey.clone()).or_insert_with(|| {
                let g = pl.group.as_ref().map(|g| (tkey.clone(), g.label.to_string(), g.kind));
                tops.push((g, Vec::new()));
                tops.len() - 1
            });
            let members = &mut tops[ti].1;
            let ckey = format!("c{}", pl.key);
            match members.iter_mut().find(|a| a.key == ckey) {
                Some(a) => a.procs.push(p),
                None => {
                    members.push(Agg { key: ckey, label: pl.name.to_string(), order: pl.order.clone(), procs: vec![p] })
                }
            }
        }
        let totals = |ps: &mut dyn Iterator<Item = &&ProcInfo>| {
            ps.fold((0usize, 0.0f32, 0u64, (0.0, 0.0)), |(n, c, m, io), p| {
                let pio = p.io.map_or((0.0, 0.0), |x| (x.read_rate, x.write_rate));
                (n + 1, c + p.cpu, m + p.rss, (io.0 + pio.0, io.1 + pio.1))
            })
        };
        let rank = |t: &(usize, f32, u64, (f64, f64)), label: &str| -> (f64, String) {
            match self.sort {
                Sort::Cpu => (-(t.1 as f64), label.to_lowercase()),
                Sort::Mem => (-(t.2 as f64), label.to_lowercase()),
                Sort::Io => (-(t.3.0 + t.3.1), label.to_lowercase()),
                Sort::Pid | Sort::Name => (0.0, label.to_lowercase()),
            }
        };
        let mut tops: Vec<_> = tops
            .into_iter()
            .map(|(g, mut members)| {
                members.sort_by(|a, b| a.order.cmp(&b.order));
                let t = totals(&mut members.iter().flat_map(|a| a.procs.iter()));
                (g, members, t)
            })
            .collect();
        tops.sort_by(|a, b| {
            let la = a.0.as_ref().map_or(a.1[0].label.as_str(), |g| g.1.as_str());
            let lb = b.0.as_ref().map_or(b.1[0].label.as_str(), |g| g.1.as_str());
            rank(&a.2, la).partial_cmp(&rank(&b.2, lb)).unwrap_or(std::cmp::Ordering::Equal)
        });
        let push_procs = |rows: &mut Vec<Row>, ps: &[&ProcInfo], indent: &str| {
            for (i, p) in ps.iter().enumerate() {
                let branch = if i + 1 == ps.len() { "└─ " } else { "├─ " };
                rows.push(Row::Proc { info: (*p).clone(), prefix: format!("{indent}{branch}") });
            }
        };
        let container = |rows: &mut Vec<Row>, a: &Agg, depth: u8| {
            let (count, cpu, rss, io) = totals(&mut a.procs.iter());
            let collapsed = self.collapsed.contains(&a.key);
            rows.push(Row::Group {
                key: a.key.clone(),
                label: a.label.clone(),
                kind: Some(ServiceKind::Container),
                manager: None,
                depth,
                count,
                cpu,
                rss,
                io,
                collapsed,
            });
            if !collapsed {
                push_procs(rows, &a.procs, if depth > 0 { "   " } else { "" });
            }
        };
        for (g, members, (count, cpu, rss, io)) in &tops {
            match g {
                Some((key, label, kind)) => {
                    let collapsed = self.collapsed.contains(key);
                    rows.push(Row::Group {
                        key: key.clone(),
                        label: label.clone(),
                        kind: None,
                        manager: Some(*kind),
                        depth: 0,
                        count: *count,
                        cpu: *cpu,
                        rss: *rss,
                        io: *io,
                        collapsed,
                    });
                    if !collapsed {
                        for a in members {
                            container(rows, a, 1);
                        }
                    }
                }
                None => container(rows, &members[0], 0),
            }
        }
        if !host.is_empty() {
            let key = "host".to_string();
            let (count, cpu, rss, io) = totals(&mut host.iter());
            let collapsed = self.collapsed.contains(&key);
            rows.push(Row::Group {
                key,
                label: "not in a container".into(),
                kind: None,
                manager: None,
                depth: 0,
                count,
                cpu,
                rss,
                io,
                collapsed,
            });
            if !collapsed {
                push_procs(rows, &host, "");
            }
        }
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
            let (pid, start, cpu) = (p.pid, p.start_ticks, p.cpu);
            if self.history_pid != Some(pid) || self.history_start != start {
                self.history.clear();
                self.history.push_back(cpu);
                self.history_pid = Some(pid);
                self.history_start = start;
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

    fn open_logs(&mut self) {
        self.mode = Mode::Details;
        let Some(p) = self.detail_proc() else { return };
        let Some(service) = Query::for_process(p, false) else {
            self.set_status("Kernel threads don't write to the journal (see journalctl -k).");
            return;
        };
        let per_pid = Query::for_process(p, true).filter(|_| service.is_service());
        let mut v = LogView {
            name: format!("{} ({})", p.name, p.pid),
            service,
            per_pid,
            pid_only: false,
            command: String::new(),
            lines: VecDeque::new(),
            notice: None,
            scroll: 0,
            stream: None,
        };
        v.start();
        self.logs = Some(v);
        self.mode = Mode::Logs;
    }

    /// Called from the main loop; true when the logs view needs a redraw.
    pub fn poll_logs(&mut self) -> bool {
        self.logs.as_mut().is_some_and(LogView::poll)
    }

    fn on_logs_key(&mut self, key: KeyEvent) {
        self.mode = Mode::Logs;
        let Some(v) = self.logs.as_mut() else { return };
        let max = v.lines.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('l') => {
                // Dropping the view stops journalctl.
                self.logs = None;
                self.mode = Mode::Details;
            }
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('o') if v.per_pid.is_some() => {
                v.pid_only = !v.pid_only;
                v.start();
            }
            KeyCode::Up | KeyCode::Char('k') => v.scroll = (v.scroll + 1).min(max),
            KeyCode::Down | KeyCode::Char('j') => v.scroll = v.scroll.saturating_sub(1),
            KeyCode::PageUp => v.scroll = (v.scroll + 20).min(max),
            KeyCode::PageDown => v.scroll = v.scroll.saturating_sub(20),
            KeyCode::Home | KeyCode::Char('g') => v.scroll = max,
            KeyCode::End | KeyCode::Char('G') => v.scroll = 0,
            _ => {}
        }
    }

    /// Ask to signal `target`: the process shown in Details, or the selected
    /// row in the list. The caller picks it, because the key handler has
    /// already reset `self.mode` and the list selection can differ from the
    /// Details process (filters, exits).
    fn request_signal(&mut self, sig: Signal, target: Option<ProcInfo>) {
        // None: nothing selected, or the Details process exited (the popup says so).
        let Some(info) = target else { return };
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
                KeyCode::Char(c @ ('t' | 'x')) => {
                    let sig = if c == 't' { Signal::Terminate } else { Signal::Kill };
                    match self.detail_proc().cloned() {
                        Some(p) => self.request_signal(sig, Some(p)),
                        // Exited: keep showing the popup that says so.
                        None => self.mode = Mode::Details,
                    }
                }
                KeyCode::Char('l') => self.open_logs(),
                KeyCode::Char('q') => self.quit = true,
                KeyCode::Esc | KeyCode::Enter => {}
                _ => self.mode = Mode::Details,
            },
            Mode::Logs => self.on_logs_key(key),
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
            Mode::Containers(i) => self.on_picker_key(key, i),
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
            KeyCode::Esc if !self.filter.is_all() => self.choose(Filter::All),
            KeyCode::Char('c') if !self.choices.is_empty() || !self.filter.is_all() => {
                let at = self.choices.iter().position(|c| c.filter == self.filter).unwrap_or(0);
                self.mode = Mode::Containers(at);
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
                self.view = self.view.next(!self.choices.is_empty());
                self.rebuild();
                self.set_status(format!("View: {}", self.view.name()));
            }
            KeyCode::Char('1') => self.switch_view(View::List),
            KeyCode::Char('2') => self.switch_view(View::App),
            KeyCode::Char('3') => self.switch_view(View::Service),
            KeyCode::Char('4') => self.switch_view(View::Parent),
            KeyCode::Char('5') => self.switch_view(View::Tree),
            KeyCode::Char('6') if !self.choices.is_empty() => self.switch_view(View::Containers),
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
            KeyCode::Char('t') => self.request_signal(Signal::Terminate, self.selected().cloned()),
            KeyCode::Char('x') => self.request_signal(Signal::Kill, self.selected().cloned()),
            _ => {}
        }
    }

    fn on_picker_key(&mut self, key: KeyEvent, at: usize) {
        let last = self.choices.len().saturating_sub(1);
        let at = at.min(last);
        match key.code {
            KeyCode::Esc | KeyCode::Char('c') => {}
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.mode = Mode::Containers(at.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.mode = Mode::Containers((at + 1).min(last)),
            KeyCode::Home | KeyCode::Char('g') => self.mode = Mode::Containers(0),
            KeyCode::End | KeyCode::Char('G') => self.mode = Mode::Containers(last),
            KeyCode::Enter => match self.choices.get(at) {
                Some(c) => self.choose(c.filter.clone()),
                // No containers left: back to everything.
                None => self.choose(Filter::All),
            },
            _ => self.mode = Mode::Containers(at),
        }
    }

    /// Limit the view to a picker entry. All containers and a project or pod
    /// switch to the containers view, where their structure shows; one
    /// container keeps the current view.
    pub fn choose(&mut self, f: Filter) {
        if matches!(f, Filter::Containers | Filter::Group(_)) {
            self.view = View::Containers;
        }
        self.filter = f;
        self.filter_gone = false;
        if self.choices.is_empty() && self.view == View::Containers && self.filter.is_all() {
            self.view = View::List;
        }
        self.table.select(Some(0));
        self.rebuild();
        let what = containers::filter_label(&self.filter, &self.choices);
        self.set_status(format!("Showing {what}"));
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
            service: piddle_core::service::Service::unknown(),
        }
    }

    fn snap(procs: Vec<ProcInfo>) -> Snapshot {
        Snapshot {
            procs,
            ncpu: 4,
            clk_tck: 100,
            boot_time: Some(0),
            total_cpu: Some(0.0),
            memory: Some(piddle_core::sampler::Memory { total: 1 << 30, used: 0 }),
            sample_cost: Duration::ZERO,
            self_pid: 99_999,
            containers: Default::default(),
        }
    }

    fn snap_with_containers(procs: Vec<ProcInfo>, inv: containers::Inventory) -> Snapshot {
        Snapshot { containers: Arc::new(inv), ..snap(procs) }
    }

    fn app_with(procs: Vec<ProcInfo>) -> App {
        let mut app = App::new(Control::new(1000));
        app.apply(snap(procs));
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

    fn in_service(mut p: ProcInfo, path: &str) -> ProcInfo {
        p.service = Arc::new(piddle_core::service::classify(path, &HashMap::new(), 1000));
        p
    }

    fn docker_id(c: char) -> String {
        c.to_string().repeat(64)
    }

    fn container_procs() -> (Vec<ProcInfo>, containers::Inventory) {
        use piddle_core::containers::{Access, Container, Inventory, Manager, Runtime};
        let d = |c| format!("/system.slice/docker-{}.scope", docker_id(c));
        let procs = vec![
            in_service(proc(1, 0, "init", 0.0), "/init.scope"),
            in_service(proc(10, 1, "nginx", 5.0), &d('a')),
            in_service(proc(11, 10, "nginx-worker", 1.0), &d('a')),
            in_service(proc(20, 1, "postgres", 2.0), &d('b')),
            in_service(proc(30, 1, "redis", 0.0), &d('c')),
        ];
        let mut inv = Inventory::default();
        let compose = |s: &str| Manager::Compose { project: "shop".into(), service: s.into() };
        for (c, name, manager) in
            [('a', "shop-web-1", compose("web")), ('b', "shop-db-1", compose("db")), ('c', "cache", Manager::None)]
        {
            let id: Arc<str> = docker_id(c).into();
            inv.containers.insert(
                id.clone(),
                Container { runtime: Runtime::Docker, id, name: name.into(), image: "i".into(), manager },
            );
        }
        inv.access.insert(Runtime::Docker, Access::Listed(3));
        (procs, inv)
    }

    #[test]
    fn no_containers_no_picker() {
        let mut app = app_with(vec![proc(1, 0, "init", 0.0)]);
        assert!(app.choices.is_empty());
        app.on_key(key(KeyCode::Char('c')));
        assert!(matches!(app.mode, Mode::Normal), "c does nothing without containers");
        app.on_key(key(KeyCode::Char('6')));
        assert_eq!(app.view, View::List);
        for _ in 0..6 {
            app.on_key(key(KeyCode::Char('v')));
            assert_ne!(app.view, View::Containers);
        }
    }

    #[test]
    fn containers_view_nests_projects_and_filters() {
        let (procs, inv) = container_procs();
        let mut app = App::new(Control::new(1000));
        app.apply(snap_with_containers(procs.clone(), inv.clone()));
        app.switch_view(View::Containers);
        // Project (with totals), its containers, the standalone one, then the host.
        let head: Vec<String> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Group { label, depth, count, .. } => {
                    Some(format!("{}{label} ×{count}", "  ".repeat(*depth as usize)))
                }
                Row::Proc { .. } => None,
            })
            .collect();
        assert_eq!(head, ["shop ×3", "  shop-db-1 ×1", "  shop-web-1 ×2", "cache ×1", "not in a container ×1"]);

        // The picker: choosing the project limits the view to it.
        app.on_key(key(KeyCode::Char('c')));
        assert!(matches!(app.mode, Mode::Containers(0)));
        app.on_key(KeyEvent::from(KeyCode::Down));
        app.on_key(KeyEvent::from(KeyCode::Down));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(matches!(app.filter, Filter::Group(_)));
        assert_eq!(
            names(&app),
            ["[shop ×3]", "[shop-db-1 ×1]", "   └─ postgres", "[shop-web-1 ×2]", "   ├─ nginx", "   └─ nginx-worker"]
        );

        // One container keeps the current view; in the list, just its processes.
        app.switch_view(View::List);
        let web = app.choices.iter().find(|c| c.label == "shop-web-1").unwrap().filter.clone();
        app.choose(web);
        assert_eq!(names(&app), ["nginx", "nginx-worker"]);

        // It stops: the view says so, and Esc goes back to everything.
        app.apply(snap_with_containers(vec![procs[0].clone(), procs[3].clone()], inv.clone()));
        assert!(app.filter_gone && app.rows.is_empty());
        assert!(app.filter_text().unwrap().contains("shop-web-1 is no longer running"));
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.filter.is_all() && !app.filter_gone);
        assert_eq!(app.rows.len(), 2);

        // The last container goes: the containers view falls back to the list.
        app.switch_view(View::Containers);
        app.apply(snap_with_containers(vec![procs[0].clone()], inv));
        assert_eq!(app.view, View::List);
    }

    #[test]
    fn service_view_groups_and_follows_moves() {
        let user = "/user.slice/user-1000.slice/user@1000.service";
        let procs = |vim_cgroup: &str| {
            vec![
                in_service(proc(10, 1, "sshd", 1.0), "/system.slice/ssh.service"),
                in_service(proc(11, 10, "bash", 0.0), "/user.slice/user-1000.slice/session-3.scope"),
                in_service(proc(12, 11, "vim", 50.0), vim_cgroup),
            ]
        };
        let mut app = app_with(procs("/user.slice/user-1000.slice/session-3.scope"));
        app.on_key(key(KeyCode::Char('3')));
        assert_eq!(app.view, View::Service);
        assert_eq!(names(&app), ["[session-3 ×2]", "├─ vim", "└─ bash", "[ssh ×1]", "└─ sshd"]);
        // The same process (pid + start time) moved into another unit: it follows.
        app.apply(snap(procs(&format!("{user}/app.slice/run-u42.scope"))));
        assert_eq!(names(&app), ["[run-u42 ×1]", "└─ vim", "[ssh ×1]", "└─ sshd", "[session-3 ×1]", "└─ bash"]);
        // Search matches the service name too.
        app.query = "ssh".into();
        app.rebuild();
        assert_eq!(names(&app), ["[ssh ×1]", "└─ sshd"]);
    }

    #[test]
    fn logs_for_kernel_threads_explain_instead_of_opening() {
        let mut k = proc(2, 0, "kthreadd", 0.0);
        k.kthread = true;
        k.service = piddle_core::service::Service::kernel();
        let mut app = app_with(vec![k]);
        app.show_kernel = true;
        app.rebuild();
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::Details));
        app.on_key(key(KeyCode::Char('l')));
        assert!(matches!(app.mode, Mode::Details) && app.logs.is_none());
        assert!(app.status.as_ref().is_some_and(|(s, _)| s.contains("journalctl -k")));
    }

    fn pending_target(app: &App) -> Option<i32> {
        match &app.mode {
            Mode::Confirm(p) | Mode::ConfirmTyped(p, _) => Some(p.info.pid),
            _ => None,
        }
    }

    #[test]
    fn details_actions_target_the_details_process() {
        // Nothing here sends a signal: only the pending confirmation is checked.
        let a = || proc(10, 1, "alpha", 50.0);
        let b = || proc(20, 1, "beta", 40.0);
        let mut app = app_with(vec![a(), b()]);
        app.active_only = true;
        app.rebuild();
        assert_eq!(app.selected().map(|p| p.pid), Some(10));
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::Details));

        // alpha goes idle: the active-only list hides it and the selection
        // moves to beta, while Details still shows alpha.
        app.activity.remove(&10);
        let mut idle = a();
        idle.cpu = 0.0;
        app.apply(snap(vec![idle, b()]));
        assert_eq!(app.selected().map(|p| p.pid), Some(20));
        assert_eq!(app.detail_proc().map(|p| p.pid), Some(10));

        app.on_key(key(KeyCode::Char('t')));
        assert_eq!(pending_target(&app), Some(10));
        app.on_key(key(KeyCode::Esc)); // cancel
        assert!(matches!(app.mode, Mode::Normal));

        // In the list, t still means the selected row.
        app.on_key(key(KeyCode::Char('x')));
        assert_eq!(pending_target(&app), Some(20));
    }

    #[test]
    fn details_never_follow_a_reused_pid() {
        let mut app = app_with(vec![proc(10, 1, "alpha", 5.0)]);
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.detail_proc().map(|p| p.name.to_string()).as_deref(), Some("alpha"));
        // alpha exits and an unrelated process gets PID 10.
        let mut reused = proc(10, 1, "intruder", 5.0);
        reused.start_ticks = 999;
        app.apply(snap(vec![reused]));
        assert!(app.detail_proc().is_none());
        app.on_key(key(KeyCode::Char('x')));
        assert_eq!(pending_target(&app), None);
        assert!(matches!(app.mode, Mode::Details));
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
