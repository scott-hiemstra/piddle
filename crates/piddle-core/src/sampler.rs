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
    /// Raw bytes of `/proc/<pid>/stat` and `/proc/<pid>/cgroup`: process
    /// names and cgroup paths needn't be UTF-8, so they're decoded lossily.
    raw: Vec<u8>,
    io_buf: String,
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

/// Check that `/proc` is mounted. PIDdle reads every process from it, so
/// without it the view would be empty rather than wrong; say why instead.
/// The error is a complete message for the user.
pub fn check_proc() -> Result<(), String> {
    check_proc_at(std::path::Path::new("/proc"))
}

fn check_proc_at(proc: &std::path::Path) -> Result<(), String> {
    // A mounted procfs always has /proc/stat and /proc/self; an empty
    // mount point (a chroot or rescue shell) has neither.
    if proc.join("stat").is_file() && proc.join("self").exists() {
        return Ok(());
    }
    Err(format!(
        "{} is not mounted, so there are no processes to show. PIDdle reads them from there.\n\
         In a chroot or rescue shell, mount it first:  mount -t proc proc /proc",
        proc.display()
    ))
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
            raw: Vec::with_capacity(1024),
            io_buf: String::with_capacity(512),
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
        let st = parse_stat(&read_lossy(&format!("/proc/{pid}/stat"), &mut self.raw)?)?;

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
            let text = read_lossy(&format!("/proc/{pid}/cgroup"), &mut self.raw);
            if let Some(path) = text.as_deref().and_then(service::cgroup_path)
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

        let io_buf = &mut self.io_buf;
        let io =
            io_step(&mut cached.prev_io, &mut cached.io_denied, self.collect_io, dt, first, || read_io(pid, io_buf));

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

/// Read a small /proc file whose text may contain non-UTF-8 bytes (process
/// names, cgroup paths): invalid sequences become U+FFFD instead of the whole
/// read failing, which would hide the process.
fn read_lossy<'a>(path: &str, buf: &'a mut Vec<u8>) -> Option<std::borrow::Cow<'a, str>> {
    buf.clear();
    File::open(path).ok()?.read_to_end(buf).ok()?;
    Some(String::from_utf8_lossy(buf))
}

/// One process's disk I/O for this sample.
///
/// `prev` is the counter baseline for rates. While collection is off the
/// baseline is dropped, so re-enabling starts a fresh baseline instead of
/// dividing everything accumulated meanwhile by one sampling interval.
fn io_step(
    prev: &mut Option<(u64, u64)>,
    denied: &mut bool,
    collect: bool,
    dt: f64,
    first: bool,
    read: impl FnOnce() -> Option<(u64, u64)>,
) -> Option<IoStats> {
    if !collect {
        *prev = None;
        return None;
    }
    if *denied {
        return None;
    }
    let Some((r, w)) = read() else {
        // Permissions don't change for a process's lifetime: stop trying.
        *denied = true;
        return None;
    };
    let rate = |now: u64, before: u64| if dt > 0.0 { now.saturating_sub(before) as f64 / dt } else { 0.0 };
    let (read_rate, write_rate) = match *prev {
        Some((pr, pw)) if !first => (rate(r, pr), rate(w, pw)),
        _ => (0.0, 0.0),
    };
    *prev = Some((r, w));
    Some(IoStats { read_total: r, write_total: w, read_rate, write_rate })
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
    parse_stat(&read_lossy(&format!("/proc/{pid}/stat"), &mut Vec::new())?).map(|st| st.start_ticks)
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
    fn missing_proc_is_reported() {
        let dir = std::env::temp_dir().join(format!("piddle-proc-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let err = check_proc_at(&dir).unwrap_err();
        assert!(err.contains("is not mounted") && err.contains("mount -t proc"), "{err}");
        // A procfs-like layout passes.
        std::fs::write(dir.join("stat"), "cpu  1 2 3 4\n").unwrap();
        std::fs::create_dir_all(dir.join("self")).unwrap();
        assert_eq!(check_proc_at(&dir), Ok(()));
        std::fs::remove_dir_all(&dir).unwrap();
        // And the real one, on the machine running the tests.
        assert_eq!(check_proc(), Ok(()));
    }

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
    fn non_utf8_stat_keeps_the_process() {
        // Kernel comm is raw bytes; one invalid byte must not drop the record.
        let dir = std::env::temp_dir().join(format!("piddle-stat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("stat");
        let mut line = b"4321 (bad\xff name) R 1 4321 4321 0 -1 4194304 0 0 0 0 7 3 0 0 20 0 1 0 555 2048 9 0".to_vec();
        line.push(b'\n');
        std::fs::write(&f, &line).unwrap();
        let mut raw = Vec::new();
        let text = read_lossy(f.to_str().unwrap(), &mut raw).unwrap();
        let st = parse_stat(&text).unwrap();
        assert_eq!(st.comm, "bad\u{FFFD} name");
        assert_eq!((st.ppid, st.utime, st.stime, st.start_ticks, st.rss_pages), (1, 7, 3, 555, 9));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn process_with_non_utf8_name_is_sampled_and_signalable() {
        use std::os::unix::ffi::OsStrExt;
        // Run sleep under a name containing 0xff; comm is taken from it.
        let dir = std::env::temp_dir().join(format!("piddle-name-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(std::ffi::OsStr::from_bytes(b"zz\xffsleep"));
        std::fs::copy("/bin/sleep", &exe).unwrap();
        let mut child = std::process::Command::new(&exe).arg("30").spawn().unwrap();
        let pid = child.id() as i32;
        std::thread::sleep(Duration::from_millis(50));
        let snap = Sampler::new().sample();
        let found = snap.procs.iter().find(|p| p.pid == pid).map(|p| p.name.to_string());
        let ticks = start_ticks_of(pid);
        child.kill().unwrap();
        child.wait().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found.as_deref(), Some("zz\u{FFFD}sleep"));
        // The PID-reuse guard must be able to read it too, or signals fail.
        assert!(ticks.is_some());
    }

    #[test]
    fn reenabling_io_does_not_spike() {
        let (mut prev, mut denied) = (None, false);
        let step = |prev: &mut _, denied: &mut _, collect, counter: u64| {
            io_step(prev, denied, collect, 1.0, false, || Some((counter, counter)))
        };
        assert_eq!(step(&mut prev, &mut denied, true, 0).unwrap().read_rate, 0.0);
        assert_eq!(step(&mut prev, &mut denied, true, 1_000).unwrap().read_rate, 1_000.0);
        // Off for three samples while the process keeps reading.
        for _ in 0..3 {
            assert!(step(&mut prev, &mut denied, false, 0).is_none());
        }
        // First sample back: a new baseline, not 999 kB over one second.
        assert_eq!(step(&mut prev, &mut denied, true, 1_000_000).unwrap().read_rate, 0.0);
        assert_eq!(step(&mut prev, &mut denied, true, 1_002_000).unwrap().read_rate, 2_000.0);
        // Unreadable: stop trying for this process.
        assert!(io_step(&mut prev, &mut denied, true, 1.0, false, || None).is_none());
        assert!(denied);
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
