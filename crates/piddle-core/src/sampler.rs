//! Low-overhead /proc sampler.
//!
//! Each tick reads only `/proc/<pid>/stat` per process (one small read).
//! Anything that does not change over a process's lifetime (command line,
//! owner, application name) is read once per process and cached, keyed by
//! (pid, start time) so PID reuse is detected.
//!
//! Service membership (`/proc/<pid>/cgroup`) can change while a process
//! runs, so it is re-read on a rotation: each process every
//! [`CGROUP_EVERY`] samples, a quarter of the processes per tick.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::encode::sanitize;
use crate::service::{self, Service};

/// Kernel thread flag from `include/linux/sched.h`.
const PF_KTHREAD: u64 = 0x0020_0000;

/// Re-read each process's cgroup every this many samples.
pub const CGROUP_EVERY: u64 = 4;

#[derive(Clone, Debug)]
pub struct ProcInfo {
    pub pid: i32,
    pub ppid: i32,
    /// Best human-readable name (comm, or the untruncated argv[0] basename).
    pub name: Arc<str>,
    /// Grouping key for "by application".
    pub app: Arc<str>,
    pub cmdline: Arc<str>,
    /// Lowercased name + app + cmdline, for search.
    pub search_text: Arc<str>,
    pub user: Arc<str>,
    pub uid: u32,
    pub state: char,
    /// CPU usage in percent of ONE core (100.0 == one fully busy core).
    pub cpu: f32,
    pub rss: u64,
    pub vsize: u64,
    pub threads: u32,
    pub nice: i32,
    /// Start time in clock ticks since boot.
    pub start_ticks: u64,
    pub kthread: bool,
    /// Disk I/O, when collection is enabled and `/proc/<pid>/io` is readable
    /// (own processes, or all of them when running as root).
    pub io: Option<IoStats>,
    /// The systemd service, scope or container the process is in. Shared
    /// between all members, and may change between samples.
    pub service: Arc<Service>,
}

/// Storage I/O of one process: bytes actually read from / written to block
/// devices (what iotop shows), as totals and per-second rates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct IoStats {
    pub read_total: u64,
    pub write_total: u64,
    pub read_rate: f64,
    pub write_rate: f64,
}

#[derive(Debug)]
pub struct Snapshot {
    pub procs: Vec<ProcInfo>,
    pub ncpu: usize,
    pub clk_tck: u64,
    pub boot_time: u64,
    /// Whole-machine busy fraction, 0..=100.
    pub total_cpu: f32,
    pub mem_total: u64,
    pub mem_used: u64,
    pub sample_cost: Duration,
    pub self_pid: i32,
}

/// Shared knobs for a background sampler.
pub struct Control {
    pub interval_ms: AtomicU64,
    pub paused: AtomicBool,
    /// Also read `/proc/<pid>/io` each sample (one more small read per process).
    pub collect_io: AtomicBool,
}

impl Control {
    pub fn new(interval_ms: u64) -> Arc<Self> {
        Arc::new(Self {
            interval_ms: AtomicU64::new(interval_ms),
            paused: AtomicBool::new(false),
            collect_io: AtomicBool::new(false),
        })
    }
}

struct Cached {
    start_ticks: u64,
    name: Arc<str>,
    app: Arc<str>,
    cmdline: Arc<str>,
    search_text: Arc<str>,
    user: Arc<str>,
    uid: u32,
    prev_ticks: u64,
    seen: bool,
    /// Previous (read, write) byte totals; None until first read.
    prev_io: Option<(u64, u64)>,
    /// `/proc/<pid>/io` was not readable; permissions don't change, so skip it.
    io_denied: bool,
    /// Raw cgroup path last read, to reclassify only when it changes.
    cgroup: Box<str>,
    service: Arc<Service>,
}

/// Synchronous sampler: call [`Sampler::sample`] at a steady cadence.
/// CPU percentages are computed over the time since the previous call.
pub struct Sampler {
    cache: HashMap<i32, Cached>,
    users: HashMap<u32, Arc<str>>,
    buf: String,
    io_buf: String,
    cg_buf: String,
    /// Interned classifications, keyed by raw cgroup path.
    services: HashMap<Box<str>, Arc<Service>>,
    kernel: Arc<Service>,
    unknown: Arc<Service>,
    tick: u64,
    uid: u32,
    collect_io: bool,
    clk_tck: u64,
    page_size: u64,
    ncpu: usize,
    boot_time: u64,
    last: Option<Instant>,
    prev_cpu: (u64, u64),
    self_pid: i32,
}

/// Run a [`Sampler`] on a background thread. The newest snapshot replaces any
/// that has not been consumed yet, so slow consumers never see stale data.
pub fn spawn(control: Arc<Control>) -> async_channel::Receiver<Snapshot> {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::Builder::new()
        .name("piddle-sampler".into())
        .spawn(move || {
            let mut s = Sampler::new();
            loop {
                if !control.paused.load(Ordering::Relaxed) {
                    s.set_collect_io(control.collect_io.load(Ordering::Relaxed));
                    let snap = s.sample();
                    // Replace an unconsumed snapshot rather than queueing stale data.
                    if tx.force_send(snap).is_err() {
                        break;
                    }
                }
                let ms = control.interval_ms.load(Ordering::Relaxed).clamp(100, 5000);
                std::thread::sleep(Duration::from_millis(ms));
            }
        })
        .expect("spawn sampler thread");
    rx
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Self {
        // SAFETY: sysconf is always safe to call.
        let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
        let ncpu = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        let stat = std::fs::read_to_string("/proc/stat").unwrap_or_default();
        let boot_time =
            stat.lines().find_map(|l| l.strip_prefix("btime ")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        Self {
            cache: HashMap::new(),
            users: load_users(),
            buf: String::with_capacity(1024),
            io_buf: String::with_capacity(512),
            cg_buf: String::with_capacity(256),
            services: HashMap::new(),
            kernel: Service::kernel(),
            unknown: Service::unknown(),
            tick: 0,
            // SAFETY: getuid cannot fail.
            uid: unsafe { libc::getuid() },
            collect_io: false,
            clk_tck,
            page_size,
            ncpu,
            boot_time,
            last: None,
            prev_cpu: (0, 0),
            self_pid: std::process::id() as i32,
        }
    }

    /// Enable or disable per-process disk I/O collection.
    pub fn set_collect_io(&mut self, on: bool) {
        self.collect_io = on;
    }

    pub fn sample(&mut self) -> Snapshot {
        let t0 = Instant::now();
        let dt = self.last.map(|l| t0.duration_since(l).as_secs_f64()).unwrap_or(0.0);
        let first = self.last.is_none();
        self.last = Some(t0);

        for c in self.cache.values_mut() {
            c.seen = false;
        }

        let mut procs = Vec::with_capacity(self.cache.len().max(256));
        if let Ok(dir) = std::fs::read_dir("/proc") {
            for entry in dir.flatten() {
                let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
                    continue;
                };
                if let Some(p) = self.read_proc(pid, dt, first) {
                    procs.push(p);
                }
            }
        }
        self.cache.retain(|_, c| c.seen);
        self.tick += 1;
        if self.tick.is_multiple_of(64) {
            self.services.retain(|_, s| Arc::strong_count(s) > 1);
        }

        let total_cpu = self.read_total_cpu();
        let (mem_total, mem_used) = read_meminfo(&mut self.buf);

        Snapshot {
            procs,
            ncpu: self.ncpu,
            clk_tck: self.clk_tck,
            boot_time: self.boot_time,
            total_cpu,
            mem_total,
            mem_used,
            sample_cost: t0.elapsed(),
            self_pid: self.self_pid,
        }
    }

    fn read_proc(&mut self, pid: i32, dt: f64, first: bool) -> Option<ProcInfo> {
        let path = format!("/proc/{pid}/stat");
        self.buf.clear();
        File::open(&path).ok()?.read_to_string(&mut self.buf).ok()?;
        let st = parse_stat(&self.buf)?;

        let total_ticks = st.utime + st.stime;
        let cached = match self.cache.get_mut(&pid) {
            Some(c) if c.start_ticks == st.start_ticks => c,
            _ => {
                let c = self.load_static(pid, &st);
                self.cache.insert(pid, c);
                self.cache.get_mut(&pid).unwrap()
            }
        };
        let fresh = !cached.seen && cached.prev_ticks == u64::MAX;
        let kthread = st.flags & PF_KTHREAD != 0;
        if kthread {
            cached.service = self.kernel.clone();
        } else if fresh || (pid as u64).wrapping_add(self.tick).is_multiple_of(CGROUP_EVERY) {
            self.cg_buf.clear();
            let ok = File::open(format!("/proc/{pid}/cgroup")).and_then(|mut f| f.read_to_string(&mut self.cg_buf));
            if let Some(path) = ok.ok().and_then(|_| service::cgroup_path(&self.cg_buf))
                && *cached.cgroup != *path
            {
                cached.service = match self.services.get(path) {
                    Some(s) => s.clone(),
                    None => {
                        let s = Arc::new(service::classify(path, &self.users, self.uid));
                        self.services.insert(path.into(), s.clone());
                        s
                    }
                };
                cached.cgroup = path.into();
            }
        }
        let cpu = if first || fresh || dt <= 0.0 {
            0.0
        } else {
            let d = total_ticks.saturating_sub(cached.prev_ticks) as f64;
            (d / self.clk_tck as f64 / dt * 100.0) as f32
        };
        cached.prev_ticks = total_ticks;
        cached.seen = true;

        let io = if self.collect_io && !cached.io_denied {
            match read_io(pid, &mut self.io_buf) {
                Some((r, w)) => {
                    let rate = |now: u64, prev: u64| if dt > 0.0 { now.saturating_sub(prev) as f64 / dt } else { 0.0 };
                    let (read_rate, write_rate) = match cached.prev_io {
                        Some((pr, pw)) if !first => (rate(r, pr), rate(w, pw)),
                        _ => (0.0, 0.0),
                    };
                    cached.prev_io = Some((r, w));
                    Some(IoStats { read_total: r, write_total: w, read_rate, write_rate })
                }
                None => {
                    cached.io_denied = true;
                    None
                }
            }
        } else {
            None
        };

        Some(ProcInfo {
            pid,
            ppid: st.ppid,
            name: cached.name.clone(),
            app: cached.app.clone(),
            cmdline: cached.cmdline.clone(),
            search_text: cached.search_text.clone(),
            user: cached.user.clone(),
            uid: cached.uid,
            state: st.state,
            cpu,
            rss: st.rss_pages.max(0) as u64 * self.page_size,
            vsize: st.vsize,
            threads: st.threads,
            nice: st.nice,
            start_ticks: st.start_ticks,
            kthread,
            io,
            service: cached.service.clone(),
        })
    }

    fn load_static(&mut self, pid: i32, st: &Stat) -> Cached {
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let args: Vec<&[u8]> = raw.split(|&b| b == 0).filter(|a| !a.is_empty()).collect();
        // Everything here is chosen by whoever started the process: sanitize
        // before it can reach a terminal or a label.
        let cmdline: String = sanitize(&args.iter().map(|a| String::from_utf8_lossy(a)).collect::<Vec<_>>().join(" "));

        // argv[0] basename. Some programs (Chromium, sshd) rewrite argv into a
        // single space-separated string, so cut at the first space in that case.
        let argv0 = args.first().map(|a| String::from_utf8_lossy(a).into_owned()).unwrap_or_default();
        let argv0 = if args.len() == 1 { argv0.split(' ').next().unwrap_or("").to_string() } else { argv0 };
        let base = sanitize(argv0.rsplit('/').next().unwrap_or("").trim_end_matches(':'));

        let comm = &sanitize(&st.comm);
        // comm is truncated to 15 bytes; prefer the full argv[0] name when it extends comm.
        let name = if comm.len() >= 15 && base.starts_with(comm.as_str()) { base.clone() } else { comm.clone() };
        let app = if base.is_empty() || st.flags & PF_KTHREAD != 0 { comm.clone() } else { base };

        let uid = std::fs::metadata(format!("/proc/{pid}")).map(|m| m.uid()).unwrap_or(u32::MAX);
        let user = self.users.get(&uid).cloned().unwrap_or_else(|| Arc::from(uid.to_string()));
        let search_text = format!("{name}\n{app}\n{cmdline}").to_lowercase();

        Cached {
            start_ticks: st.start_ticks,
            name: name.into(),
            app: app.into(),
            cmdline: cmdline.into(),
            search_text: search_text.into(),
            user,
            uid,
            prev_ticks: u64::MAX,
            seen: false,
            prev_io: None,
            io_denied: false,
            cgroup: "".into(),
            service: self.unknown.clone(),
        }
    }

    fn read_total_cpu(&mut self) -> f32 {
        self.buf.clear();
        if File::open("/proc/stat").and_then(|mut f| f.read_to_string(&mut self.buf)).is_err() {
            return 0.0;
        }
        let Some(line) = self.buf.lines().next() else { return 0.0 };
        let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
        if v.len() < 5 {
            return 0.0;
        }
        // guest time is already included in user/nice.
        let total: u64 = v.iter().take(8).sum();
        let idle = v[3] + v[4];
        let (pt, pi) = self.prev_cpu;
        self.prev_cpu = (total, idle);
        let dt = total.saturating_sub(pt);
        if pt == 0 || dt == 0 {
            return 0.0;
        }
        let di = idle.saturating_sub(pi);
        (dt.saturating_sub(di) as f64 / dt as f64 * 100.0) as f32
    }
}

struct Stat {
    comm: String,
    state: char,
    ppid: i32,
    flags: u64,
    utime: u64,
    stime: u64,
    nice: i32,
    threads: u32,
    start_ticks: u64,
    vsize: u64,
    rss_pages: i64,
}

fn parse_stat(s: &str) -> Option<Stat> {
    // comm may itself contain spaces and parentheses; it ends at the LAST ')'.
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let comm = s.get(open + 1..close)?.to_string();
    // Fields after comm, starting at field 3 (state).
    let f: Vec<&str> = s.get(close + 2..)?.split_ascii_whitespace().collect();
    let n = |i: usize| -> Option<&str> { f.get(i - 3).copied() };
    Some(Stat {
        comm,
        state: n(3)?.chars().next()?,
        ppid: n(4)?.parse().ok()?,
        flags: n(9)?.parse().ok()?,
        utime: n(14)?.parse().ok()?,
        stime: n(15)?.parse().ok()?,
        nice: n(19)?.parse().ok()?,
        threads: n(20)?.parse().ok()?,
        start_ticks: n(22)?.parse().ok()?,
        vsize: n(23)?.parse().ok()?,
        rss_pages: n(24)?.parse().ok()?,
    })
}

/// Disk (read_bytes, write_bytes) totals of one process, or None if not
/// readable. For front ends that need a single process without enabling
/// collection for all of them.
pub fn io_totals(pid: i32) -> Option<(u64, u64)> {
    read_io(pid, &mut String::with_capacity(512))
}

/// (read_bytes, write_bytes) from `/proc/<pid>/io`, or None if not readable.
fn read_io(pid: i32, buf: &mut String) -> Option<(u64, u64)> {
    buf.clear();
    File::open(format!("/proc/{pid}/io")).ok()?.read_to_string(buf).ok()?;
    parse_io(buf)
}

fn parse_io(s: &str) -> Option<(u64, u64)> {
    let field = |key: &str| s.lines().find_map(|l| l.strip_prefix(key)).and_then(|v| v.trim().parse::<u64>().ok());
    Some((field("read_bytes:")?, field("write_bytes:")?))
}

/// Re-read a process's start time, used to guard signals against PID reuse.
pub fn start_ticks_of(pid: i32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&s).map(|st| st.start_ticks)
}

fn read_meminfo(buf: &mut String) -> (u64, u64) {
    buf.clear();
    if File::open("/proc/meminfo").and_then(|mut f| f.read_to_string(buf)).is_err() {
        return (0, 0);
    }
    let mut total = 0;
    let mut avail = 0;
    for line in buf.lines() {
        let kb = |l: &str| l.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 1024;
        if line.starts_with("MemTotal:") {
            total = kb(line);
        } else if line.starts_with("MemAvailable:") {
            avail = kb(line);
        }
    }
    (total, total.saturating_sub(avail))
}

fn load_users() -> HashMap<u32, Arc<str>> {
    std::fs::read_to_string("/etc/passwd")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut it = l.split(':');
            let name = it.next()?;
            let uid = it.nth(1)?.parse().ok()?;
            Some((uid, Arc::from(name)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comm_with_parens_and_spaces() {
        let line = "1234 (my (weird) app) S 1 1234 1234 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 7 0 98765 1048576 300 18446744073709551615";
        let st = parse_stat(line).unwrap();
        assert_eq!(st.comm, "my (weird) app");
        assert_eq!(st.state, 'S');
        assert_eq!(st.ppid, 1);
        assert_eq!(st.utime, 250);
        assert_eq!(st.stime, 50);
        assert_eq!(st.threads, 7);
        assert_eq!(st.start_ticks, 98765);
        assert_eq!(st.rss_pages, 300);
    }

    #[test]
    fn parses_proc_io() {
        let s = "rchar: 100\nwchar: 200\nsyscr: 3\nsyscw: 4\nread_bytes: 4096\nwrite_bytes: 8192\ncancelled_write_bytes: 0\n";
        assert_eq!(parse_io(s), Some((4096, 8192)));
        assert_eq!(parse_io("rchar: 1\n"), None);
    }

    #[test]
    fn io_collected_for_own_process() {
        let mut s = Sampler::new();
        s.set_collect_io(true);
        s.sample();
        std::thread::sleep(Duration::from_millis(30));
        let snap = s.sample();
        let me = snap.procs.iter().find(|p| p.pid == snap.self_pid).unwrap();
        assert!(me.io.is_some(), "own /proc/self/io should be readable");
    }

    #[test]
    fn samples_real_proc() {
        let mut s = Sampler::new();
        s.sample();
        std::thread::sleep(Duration::from_millis(50));
        let snap = s.sample();
        assert!(snap.procs.iter().any(|p| p.pid == snap.self_pid));
        assert!(snap.mem_total > 0);
    }
}
