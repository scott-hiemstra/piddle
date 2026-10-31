//! Process actions and descriptions shared by all front ends.

use crate::sampler::{ProcInfo, start_ticks_of};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// SIGTERM: ask the process to exit cleanly.
    Terminate,
    /// SIGKILL: stop the process immediately.
    Kill,
}

impl Signal {
    pub fn name(self) -> &'static str {
        match self {
            Signal::Terminate => "SIGTERM",
            Signal::Kill => "SIGKILL",
        }
    }

    fn raw(self) -> i32 {
        match self {
            Signal::Terminate => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        }
    }
}

/// Send `sig` to `pid`, but only if it is still the same process that was
/// observed (same start time).
///
/// On Linux 5.3+ this is race-free: a pidfd is opened first, which pins that
/// exact process, then its identity is checked, then the signal goes through
/// the pidfd. If the PID was recycled before the pidfd was opened, the check
/// fails; if it is recycled afterwards, the pidfd still refers to the original
/// process, so a stranger can never be signalled. Older kernels fall back to
/// check-then-kill(2).
pub fn send_signal(pid: i32, start_ticks: u64, sig: Signal) -> Result<(), String> {
    if pid <= 1 {
        return Err(format!("Refusing to signal PID {pid}."));
    }
    let exited = || format!("PID {pid} has already exited.");

    // SAFETY: pidfd_open takes a pid and flags, returns an fd or -1.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    let rc = if fd >= 0 {
        let fd = fd as libc::c_int;
        let same = start_ticks_of(pid) == Some(start_ticks);
        // SAFETY: fd is a pidfd we own; a null siginfo means "like kill(2)".
        let rc = if same {
            unsafe { libc::syscall(libc::SYS_pidfd_send_signal, fd, sig.raw(), std::ptr::null::<libc::siginfo_t>(), 0) }
        } else {
            0
        };
        let err = std::io::Error::last_os_error();
        // SAFETY: closing the fd we opened above.
        unsafe { libc::close(fd) };
        if !same {
            return Err(exited());
        }
        if rc == 0 { Ok(()) } else { Err(err) }
    } else {
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ESRCH) => return Err(exited()),
            // No pidfd support: check, then kill(2).
            Some(libc::ENOSYS) => {
                if start_ticks_of(pid) != Some(start_ticks) {
                    return Err(exited());
                }
                // SAFETY: kill(2) with a positive pid only targets that single process.
                if unsafe { libc::kill(pid, sig.raw()) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
            }
            _ => Err(err),
        }
    };
    rc.map_err(|err| match err.raw_os_error() {
        Some(libc::EPERM) => {
            format!("Permission denied: PID {pid} belongs to another user. Run PIDdle with sufficient privileges to signal it.")
        }
        Some(libc::ESRCH) => exited(),
        _ => format!("Signalling PID {pid} failed: {err}"),
    })
}

/// Processes whose loss takes down the desktop session or core system
/// services. Matched against the process name and its application name.
const CRITICAL: &[&str] = &[
    "systemd",
    "init",
    "systemd-logind",
    "systemd-journald",
    "systemd-udevd",
    "dbus-daemon",
    "dbus-broker",
    "dbus-broker-launch",
    "polkitd",
    "login",
    "agetty",
    "sshd",
    "gnome-shell",
    "gnome-session-binary",
    "gnome-session",
    "gnome-session-service",
    "mutter",
    "gdm",
    "gdm3",
    "gdm-wayland-session",
    "gdm-x-session",
    "gdm-session-worker",
    "plasmashell",
    "kwin_wayland",
    "kwin_x11",
    "ksmserver",
    "startplasma-wayland",
    "startplasma-x11",
    "sddm",
    "lightdm",
    "Xwayland",
    "Xorg",
    "X",
    "NetworkManager",
    "wpa_supplicant",
    "pipewire",
    "wireplumber",
    "pulseaudio",
    "udisksd",
    "containerd",
    "dockerd",
];

/// What a front end must know before letting the user signal a process.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Assessment {
    /// Signalling is refused outright, with the reason.
    pub blocked: Option<String>,
    /// Reasons this is risky. Non-empty means the UI should ask for a
    /// stronger confirmation (for example typing the process name).
    pub warnings: Vec<String>,
    /// Number of descendant processes (informational).
    pub descendants: usize,
}

impl Assessment {
    pub fn is_critical(&self) -> bool {
        !self.warnings.is_empty()
    }
}

/// Judge how dangerous it is to signal `target`, given the current process
/// list and the PID of the front end itself.
pub fn assess(target: &ProcInfo, procs: &[ProcInfo], self_pid: i32) -> Assessment {
    let mut a = Assessment::default();
    if target.pid <= 1 {
        a.blocked = Some("PID 1 is the init system. Stopping it would crash or reboot the machine.".into());
        return a;
    }
    if target.kthread {
        a.blocked = Some("This is a kernel thread; it cannot be stopped from user space.".into());
        return a;
    }

    let parent: std::collections::HashMap<i32, i32> = procs.iter().map(|p| (p.pid, p.ppid)).collect();
    let is_ancestor_of = |anc: i32, mut pid: i32| {
        for _ in 0..256 {
            match parent.get(&pid) {
                Some(&pp) if pp == anc => return true,
                Some(&pp) if pp > 0 => pid = pp,
                _ => return false,
            }
        }
        false
    };

    if target.pid == self_pid {
        a.warnings.push("This is PIDdle itself; its window will close.".into());
    } else if is_ancestor_of(target.pid, self_pid) {
        a.warnings.push("PIDdle is running inside this process, so PIDdle will close too.".into());
    }
    if CRITICAL.iter().any(|&c| *target.name == *c || *target.app == *c) {
        a.warnings.push(format!(
            "“{}” is part of the system or your desktop session. Stopping it can log you out, freeze the desktop, or break networking or sound.",
            target.name
        ));
    }
    // SAFETY: geteuid has no preconditions.
    if target.uid == 0 && unsafe { libc::geteuid() } == 0 {
        a.warnings.push("It runs as root and PIDdle has root privileges, so nothing will stop the signal.".into());
    }
    a.descendants = procs.iter().filter(|p| is_ancestor_of(target.pid, p.pid)).count();
    a
}

pub fn state_name(c: char) -> &'static str {
    match c {
        'R' => "Running",
        'S' => "Sleeping",
        'D' => "Uninterruptible wait (disk)",
        'Z' => "Zombie",
        'T' => "Stopped",
        't' => "Traced",
        'I' => "Idle",
        'X' => "Dead",
        _ => "Unknown",
    }
}

pub fn fmt_age(s: u64) -> String {
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m {}s", s / 60, s % 60),
        3600..86400 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d {}h", s / 86400, s % 86400 / 3600),
    }
}

/// Executable path, if readable (other users' processes usually are not).
pub fn exe_path(pid: i32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok().map(|p| crate::encode::sanitize(&p.display().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn proc(pid: i32, ppid: i32, name: &str) -> ProcInfo {
        let n: Arc<str> = Arc::from(name);
        ProcInfo {
            pid,
            ppid,
            name: n.clone(),
            app: n.clone(),
            cmdline: n.clone(),
            search_text: n,
            user: Arc::from("u"),
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
        }
    }

    #[test]
    fn assessment_levels() {
        let procs = vec![
            proc(1, 0, "systemd"),
            proc(10, 1, "gnome-shell"),
            proc(20, 1, "gnome-terminal-server"),
            proc(21, 20, "bash"),
            proc(22, 21, "piddle"),
            proc(30, 1, "firefox"),
            proc(31, 30, "firefox"),
        ];
        let self_pid = 22;
        assert!(assess(&procs[0], &procs, self_pid).blocked.is_some());
        assert!(assess(&procs[1], &procs, self_pid).is_critical());
        assert!(assess(&procs[2], &procs, self_pid).is_critical()); // hosts PIDdle
        assert!(assess(&procs[4], &procs, self_pid).is_critical()); // PIDdle itself
        let ff = assess(&procs[5], &procs, self_pid);
        assert!(ff.blocked.is_none() && !ff.is_critical());
        assert_eq!(ff.descendants, 1);
        let mut k = proc(2, 0, "kthreadd");
        k.kthread = true;
        assert!(assess(&k, &procs, self_pid).blocked.is_some());
    }

    #[test]
    fn signals_the_verified_process_only() {
        // A real child we are allowed to signal.
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id() as i32;
        let ticks = start_ticks_of(pid).unwrap();
        // Wrong identity: refused, child untouched.
        assert!(send_signal(pid, ticks + 1, Signal::Terminate).is_err());
        assert!(child.try_wait().unwrap().is_none());
        // Right identity: delivered.
        send_signal(pid, ticks, Signal::Terminate).unwrap();
        let status = child.wait().unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGTERM));
        // Gone now: reported as exited, not an error about something else.
        assert!(send_signal(pid, ticks, Signal::Terminate).unwrap_err().contains("exited"));
    }

    #[test]
    fn pid_one_is_refused() {
        assert!(send_signal(1, 0, Signal::Terminate).is_err());
    }

    #[test]
    fn stale_start_time_is_refused() {
        let pid = std::process::id() as i32;
        let real = start_ticks_of(pid).unwrap();
        assert!(send_signal(pid, real + 1, Signal::Terminate).is_err());
    }
}
