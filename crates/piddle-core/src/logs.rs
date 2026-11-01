//! Journal entries for a process's service, streamed from `journalctl`.
//!
//! `journalctl` is spawned rather than linking libsystemd, so the static
//! (musl) binaries stay static and systems without systemd simply report
//! that no journal is available. Log text is arbitrary bytes written by the
//! process, so every line is sanitized before it reaches a label or terminal.

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};

use crate::encode::sanitize;
use crate::sampler::ProcInfo;
use crate::service::ServiceKind;

/// Entries loaded when a stream starts, before following new ones.
pub const BACKLOG: usize = 200;

/// What to ask the journal for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub unit: Option<String>,
    /// `unit` belongs to a user manager (`--user-unit`).
    pub user_unit: bool,
    /// Restrict to one user's entries (user units of a given user).
    pub uid: Option<u32>,
    /// Restrict to one process.
    pub pid: Option<i32>,
}

impl Query {
    /// The selected process's whole service, or just the process when
    /// `pid_only` is set or it has no unit. None for kernel threads.
    pub fn for_process(p: &ProcInfo, pid_only: bool) -> Option<Query> {
        if p.kthread || p.service.kind == ServiceKind::Kernel {
            return None;
        }
        let s = &p.service;
        let unit = if s.kind == ServiceKind::Other { None } else { s.unit.as_deref().map(str::to_string) };
        Some(Query {
            user_unit: unit.is_some() && s.user_unit,
            uid: if s.user_unit { s.uid } else { None },
            pid: (pid_only || unit.is_none()).then_some(p.pid),
            unit,
        })
    }

    /// Whole-service query (as opposed to one process).
    pub fn is_service(&self) -> bool {
        self.unit.is_some() && self.pid.is_none()
    }

    /// Needs the system journal, which ordinary users usually can't read.
    pub fn needs_system_journal(&self, viewer_uid: u32) -> bool {
        !(self.user_unit && self.uid == Some(viewer_uid))
    }

    fn args(&self) -> Vec<String> {
        let mut a = Vec::new();
        // `--opt=value` keeps a unit name from ever being parsed as an option.
        match (&self.unit, self.user_unit) {
            (Some(u), true) => a.push(format!("--user-unit={u}")),
            (Some(u), false) => a.push(format!("--unit={u}")),
            (None, _) => {}
        }
        if let Some(uid) = self.uid {
            a.push(format!("_UID={uid}"));
        }
        if let Some(pid) = self.pid {
            a.push(format!("_PID={pid}"));
        }
        a
    }

    /// The equivalent command, for people who want to run it themselves.
    pub fn command_line(&self) -> String {
        let mut s = String::from("journalctl");
        for a in self.args() {
            s.push(' ');
            s.push_str(&a);
        }
        s
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Microseconds since the epoch.
    pub time_us: u64,
    /// syslog priority, 0 (emergency) .. 7 (debug); 6 when absent.
    pub priority: u8,
    pub pid: Option<i32>,
    pub ident: String,
    /// One display line (multi-line messages become several entries).
    pub message: String,
}

#[derive(Debug)]
pub enum Event {
    Entry(Entry),
    /// journalctl stopped (or could not start); the text says why.
    Ended(String),
}

/// Everything that was queued at one moment, in order.
#[derive(Debug, Default, PartialEq)]
pub struct Batch {
    pub entries: Vec<Entry>,
    /// journalctl stopped; why.
    pub ended: Option<String>,
}

/// Take `first` (an event already received, e.g. by an async `recv`) plus
/// everything else queued, without blocking. Both kinds of event are kept:
/// dropping the end-of-stream event would leave a dead stream looking live.
pub fn take_queued(rx: &async_channel::Receiver<Event>, first: Option<Event>) -> Batch {
    let mut b = Batch::default();
    for ev in first.into_iter().chain(std::iter::from_fn(|| rx.try_recv().ok())) {
        match ev {
            Event::Entry(e) => b.entries.push(e),
            Event::Ended(why) => b.ended = Some(why),
        }
    }
    b
}

/// A running `journalctl -f`; dropping it stops the process.
pub struct Stream {
    child: Child,
    pub rx: async_channel::Receiver<Event>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start following the journal for `q`, beginning with the last [`BACKLOG`]
/// entries. Errors (journalctl missing) are returned as text for display.
pub fn follow(q: &Query) -> Result<Stream, String> {
    let mut child = Command::new("journalctl")
        .args(q.args())
        .args([
            "--follow",
            "--quiet",
            "--no-pager",
            "--output=json",
            "--output-fields=MESSAGE,PRIORITY,_PID,SYSLOG_IDENTIFIER,_COMM",
        ])
        .arg(format!("--lines={BACKLOG}"))
        .env("SYSTEMD_COLORS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "journalctl not found: this system has no systemd journal".to_string(),
            _ => format!("could not start journalctl: {e}"),
        })?;
    let (tx, rx) = async_channel::bounded(4096);
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    std::thread::Builder::new()
        .name("piddle-journal".into())
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                for e in parse_line(&line) {
                    if tx.send_blocking(Event::Entry(e)).is_err() {
                        return;
                    }
                }
            }
            let mut err = String::new();
            let _ = stderr.read_to_string(&mut err);
            let why = err.lines().map(str::trim).find(|l| !l.is_empty()).map(sanitize);
            let _ = tx.send_blocking(Event::Ended(why.unwrap_or_else(|| "journalctl exited".into())));
        })
        .map_err(|e| format!("could not start reader: {e}"))?;
    Ok(Stream { child, rx })
}

/// Whether the system journal is readable by us: Some(false) when journal
/// files exist but are not readable (not root, not in systemd-journal/adm).
pub fn system_journal_readable() -> Option<bool> {
    for dir in ["/var/log/journal", "/run/log/journal"] {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        for machine in rd.flatten() {
            let f = machine.path().join("system.journal");
            if f.exists() {
                let c = std::ffi::CString::new(f.as_os_str().as_encoded_bytes()).ok()?;
                // SAFETY: c is a valid NUL-terminated path.
                return Some(unsafe { libc::access(c.as_ptr(), libc::R_OK) } == 0);
            }
        }
    }
    None
}

/// True when `q` needs the system journal and we can't read it: the
/// stream would come back empty with no error, so the UI should say why.
pub fn hidden_by_permissions(q: &Query) -> bool {
    // SAFETY: getuid cannot fail.
    q.needs_system_journal(unsafe { libc::getuid() }) && system_journal_readable() == Some(false)
}

/// Advice shown when the system journal is out of reach.
pub const NO_SYSTEM_JOURNAL: &str = "You can't read the system journal, so system services' logs are hidden. \
     Add yourself to the systemd-journal group (sudo usermod -aG systemd-journal $USER, then log in again) \
     or run PIDdle as root.";

/// Parse one `journalctl -o json` line into display entries.
pub fn parse_line(line: &str) -> Vec<Entry> {
    let Some(fields) = parse_flat_object(line) else { return Vec::new() };
    let get = |k: &str| fields.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let time_us = get("__REALTIME_TIMESTAMP").and_then(|v| v.parse().ok()).unwrap_or(0);
    let priority = get("PRIORITY").and_then(|v| v.parse().ok()).unwrap_or(6u8).min(7);
    let pid = get("_PID").and_then(|v| v.parse().ok());
    let ident = sanitize(get("SYSLOG_IDENTIFIER").or(get("_COMM")).unwrap_or(""));
    let message = get("MESSAGE").unwrap_or("");
    message
        .trim_end_matches('\n')
        .split('\n')
        .map(|l| Entry { time_us, priority, pid, ident: ident.clone(), message: sanitize(&l.replace('\t', "    ")) })
        .collect()
}

/// Minimal JSON reader for journalctl's flat objects: string values,
/// byte arrays (non-UTF-8 fields), and anything else skipped.
fn parse_flat_object(s: &str) -> Option<Vec<(String, String)>> {
    let mut p = Parser { b: s.as_bytes(), i: 0 };
    p.ws();
    p.eat(b'{')?;
    let mut out = Vec::new();
    loop {
        p.ws();
        if p.eat(b'}').is_some() {
            return Some(out);
        }
        let key = p.string()?;
        p.ws();
        p.eat(b':')?;
        p.ws();
        match p.peek()? {
            b'"' => out.push((key, p.string()?)),
            b'[' => {
                // Either bytes, or (repeated field) an array of strings: keep the first.
                if let Some(v) = p.array()? {
                    out.push((key, v));
                }
            }
            _ => p.scalar(),
        }
        p.ws();
        if p.eat(b',').is_none() {
            p.ws();
            p.eat(b'}')?;
            return Some(out);
        }
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn eat(&mut self, c: u8) -> Option<()> {
        (self.peek()? == c).then(|| self.i += 1)
    }
    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.i += 1;
        }
    }
    fn scalar(&mut self) {
        while !matches!(self.peek(), None | Some(b',' | b'}' | b']')) {
            self.i += 1;
        }
    }
    fn hex4(&mut self) -> Option<u32> {
        let h = std::str::from_utf8(self.b.get(self.i..self.i + 4)?).ok()?;
        self.i += 4;
        u32::from_str_radix(h, 16).ok()
    }
    fn string(&mut self) -> Option<String> {
        self.eat(b'"')?;
        let mut out = Vec::new();
        loop {
            let c = self.peek()?;
            self.i += 1;
            match c {
                b'"' => return Some(String::from_utf8_lossy(&out).into_owned()),
                b'\\' => {
                    let e = self.peek()?;
                    self.i += 1;
                    let ch = match e {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xD800..0xDC00).contains(&hi) && self.b.get(self.i..self.i + 2) == Some(b"\\u")
                            {
                                self.i += 2;
                                let lo = self.hex4()?;
                                0x10000 + ((hi - 0xD800) << 10) + (lo.wrapping_sub(0xDC00) & 0x3FF)
                            } else {
                                hi
                            };
                            char::from_u32(cp).unwrap_or('\u{FFFD}')
                        }
                        other => other as char,
                    };
                    let mut buf = [0; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                _ => out.push(c),
            }
        }
    }
    /// A byte array becomes a lossy string; an array of strings yields its first.
    fn array(&mut self) -> Option<Option<String>> {
        self.eat(b'[')?;
        let mut bytes = Vec::new();
        let mut first = None;
        loop {
            self.ws();
            match self.peek()? {
                b']' => {
                    self.i += 1;
                    break;
                }
                b',' => self.i += 1,
                b'"' => {
                    let s = self.string()?;
                    first.get_or_insert(s);
                }
                _ => {
                    let start = self.i;
                    self.scalar();
                    if let Ok(v) = std::str::from_utf8(&self.b[start..self.i]).unwrap_or("").trim().parse::<u8>() {
                        bytes.push(v);
                    }
                }
            }
        }
        Some(first.or_else(|| (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())))
    }
}

/// Moved to `piddle-encode` (piddle-common) so kpiddle can share it.
pub use piddle_encode::fmt_clock;

/// Short priority tag for display.
pub fn priority_name(p: u8) -> &'static str {
    match p {
        0 => "emerg",
        1 => "alert",
        2 => "crit",
        3 => "err",
        4 => "warn",
        5 => "notice",
        6 => "info",
        _ => "debug",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{Service, classify};
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn parses_journal_json() {
        let line = r#"{"__REALTIME_TIMESTAMP":"1792674958955518","PRIORITY":"3","_PID":"1217","SYSLOG_IDENTIFIER":"pipewire","MESSAGE":"spa.alsa: \"x\" \u00e9\ud83d\ude00 done"}"#;
        let e = parse_line(line);
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].time_us, e[0].priority, e[0].pid), (1792674958955518, 3, Some(1217)));
        assert_eq!(e[0].ident, "pipewire");
        assert_eq!(e[0].message, "spa.alsa: \"x\" é😀 done");
    }

    #[test]
    fn binary_multiline_and_hostile_messages() {
        // Non-UTF-8 MESSAGE arrives as a byte array.
        let e = parse_line(r#"{"MESSAGE":[104,105,255],"_COMM":"x"}"#);
        assert_eq!((e[0].message.as_str(), e[0].ident.as_str(), e[0].priority), ("hi\u{FFFD}", "x", 6));
        let e = parse_line(r#"{"MESSAGE":"one\ntwo\n"}"#);
        assert_eq!(e.iter().map(|e| e.message.as_str()).collect::<Vec<_>>(), ["one", "two"]);
        // Escape sequences must not survive into a terminal.
        let e = parse_line(r#"{"MESSAGE":"\u001b[2Jgone\u202e"}"#);
        assert!(!e[0].message.contains('\x1b') && !e[0].message.contains('\u{202e}'));
        assert!(parse_line("not json").is_empty());
        assert!(parse_line(r#"{"MESSAGE":null,"X":12}"#).iter().all(|e| e.message.is_empty()));
    }

    fn proc_in(path: &str, pid: i32) -> ProcInfo {
        let n: Arc<str> = Arc::from("p");
        ProcInfo {
            pid,
            ppid: 1,
            name: n.clone(),
            app: n.clone(),
            cmdline: n.clone(),
            search_text: n.clone(),
            user: n,
            uid: 1000,
            state: 'S',
            cpu: 0.0,
            rss: 0,
            vsize: 0,
            threads: 1,
            nice: 0,
            start_ticks: 0,
            kthread: false,
            io: None,
            service: if path.is_empty() { Service::kernel() } else { Arc::new(classify(path, &HashMap::new(), 1000)) },
        }
    }

    #[test]
    fn queries() {
        let q = Query::for_process(&proc_in("/system.slice/ssh.service", 5), false).unwrap();
        assert_eq!(q.command_line(), "journalctl --unit=ssh.service");
        assert!(q.is_service() && q.needs_system_journal(1000));
        let q = Query::for_process(&proc_in("/system.slice/ssh.service", 5), true).unwrap();
        assert_eq!(q.command_line(), "journalctl --unit=ssh.service _PID=5");

        let user = "/user.slice/user-1000.slice/user@1000.service/session.slice/pipewire.service";
        let q = Query::for_process(&proc_in(user, 7), false).unwrap();
        assert_eq!(q.command_line(), "journalctl --user-unit=pipewire.service _UID=1000");
        assert!(!q.needs_system_journal(1000) && q.needs_system_journal(0));

        // No unit: fall back to the process.
        let q = Query::for_process(&proc_in("/", 9), false).unwrap();
        assert_eq!(q.command_line(), "journalctl _PID=9");
        assert!(Query::for_process(&proc_in("", 2), false).is_none());

        // A unit name that looks like an option stays a value.
        let q = Query::for_process(&proc_in("/system.slice/--help.service", 3), false).unwrap();
        assert_eq!(q.args(), ["--unit=--help.service"]);
    }

    #[test]
    fn queued_end_of_stream_is_not_lost() {
        let (tx, rx) = async_channel::unbounded();
        let entry = |m: &str| Entry { time_us: 1, priority: 6, pid: None, ident: String::new(), message: m.into() };
        tx.send_blocking(Event::Entry(entry("two"))).unwrap();
        tx.send_blocking(Event::Ended("No journal files were found.".into())).unwrap();
        // "one" was already received by the caller's recv().
        let b = take_queued(&rx, Some(Event::Entry(entry("one"))));
        assert_eq!(b.entries.iter().map(|e| e.message.as_str()).collect::<Vec<_>>(), ["one", "two"]);
        assert_eq!(b.ended.as_deref(), Some("No journal files were found."));
        assert_eq!(take_queued(&rx, None), Batch::default());
        // An Ended received first is kept too.
        tx.send_blocking(Event::Entry(entry("late"))).unwrap();
        let b = take_queued(&rx, Some(Event::Ended("x".into())));
        assert_eq!((b.entries.len(), b.ended.as_deref()), (1, Some("x")));
    }

    #[test]
    fn clock_format() {
        assert_eq!(fmt_clock(0).len(), 8);
        assert_eq!(priority_name(3), "err");
    }
}
