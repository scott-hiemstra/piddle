//! Which systemd service, scope or container a process belongs to, read from
//! `/proc/<pid>/cgroup`.
//!
//! Membership is not part of a process's identity: processes can be moved
//! between cgroups (`systemd-run --scope`, container runtimes), so the sampler
//! re-reads it periodically rather than caching it for the process lifetime.

use std::collections::HashMap;
use std::sync::Arc;

use crate::encode::sanitize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ServiceKind {
    /// A unit of the system manager (`/system.slice/sshd.service`).
    System,
    /// A unit of a per-user manager (`user@1000.service/.../pipewire.service`).
    User,
    /// A transient scope: login sessions, launched apps, `systemd-run --scope`.
    Scope,
    /// Docker, Podman, Kubernetes, LXC, systemd-nspawn or a libvirt machine.
    Container,
    /// Kernel threads.
    Kernel,
    /// Root cgroup, unrecognized layout, or cgroup unreadable.
    Other,
}

impl ServiceKind {
    pub fn label(self) -> &'static str {
        match self {
            ServiceKind::System => "system",
            ServiceKind::User => "user",
            ServiceKind::Scope => "scope",
            ServiceKind::Container => "container",
            ServiceKind::Kernel => "kernel",
            ServiceKind::Other => "other",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Service {
    pub kind: ServiceKind,
    /// Grouping key: the cgroup path up to and including the unit (or
    /// container), so equal unit names under different users stay apart.
    pub key: Arc<str>,
    /// Short display name.
    pub label: Arc<str>,
    /// systemd unit name as journalctl expects it (still `\x2d`-escaped).
    pub unit: Option<Arc<str>>,
    /// `unit` belongs to a user manager (`journalctl --user-unit`).
    pub user_unit: bool,
    /// Owner from `user-<uid>.slice`, when the path has one.
    pub uid: Option<u32>,
    /// Full cgroup path, sanitized, for the details view.
    pub path: Arc<str>,
    /// Lowercased label + unit, for search.
    pub search: Box<str>,
    /// For containers: which one, as the cgroup names it.
    pub container: Option<ContainerRef>,
}

/// A container as its cgroup identifies it, for matching against the
/// runtime's own list (see [`crate::containers`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerRef {
    /// "docker", "podman", "containerd", "cri-o", "k8s", "lxc", "nspawn",
    /// "machine".
    pub runtime: &'static str,
    /// The full ID (Docker, Podman, CRI runtimes) or the name (LXC, nspawn,
    /// libvirt), lowercase and sanitized.
    pub id: Arc<str>,
    /// Kubernetes pod UID, from the `pod<uid>` cgroup, with dashes.
    pub pod: Option<Arc<str>>,
}

impl Service {
    pub fn kernel() -> Arc<Service> {
        Arc::new(Service::simple(ServiceKind::Kernel, "kernel", "kernel threads", ""))
    }

    /// Placeholder until a process's cgroup has been read.
    pub fn unknown() -> Arc<Service> {
        Arc::new(Service::simple(ServiceKind::Other, "?", "unknown", ""))
    }

    /// "sshd.service  (system)", "docker 4f1c0e2a9b7d  (container)".
    pub fn describe(&self) -> String {
        let name = match (&self.unit, self.kind) {
            (_, ServiceKind::Container) | (None, _) => self.label.to_string(),
            (Some(u), _) => u.to_string(),
        };
        let kind = match self.kind {
            ServiceKind::Scope if self.user_unit => "user scope".into(),
            ServiceKind::Container if self.user_unit => "rootless container".into(),
            k => k.label().to_string(),
        };
        format!("{name}  ({kind})")
    }

    fn simple(kind: ServiceKind, key: &str, label: &str, path: &str) -> Service {
        Service {
            kind,
            key: key.into(),
            label: label.into(),
            unit: None,
            user_unit: false,
            uid: None,
            path: path.into(),
            search: label.to_lowercase().into(),
            container: None,
        }
    }
}

/// Pick the cgroup path out of `/proc/<pid>/cgroup`: the unified (v2) entry,
/// else systemd's v1 named hierarchy, else the first line.
pub fn cgroup_path(contents: &str) -> Option<&str> {
    let mut first = None;
    let mut named = None;
    for line in contents.lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(id), Some(ctrl), Some(path)) = (parts.next(), parts.next(), parts.next()) else { continue };
        if id == "0" && ctrl.is_empty() {
            return Some(path);
        }
        if ctrl == "name=systemd" || ctrl == "name=openrc" {
            named = Some(path);
        }
        first.get_or_insert(path);
    }
    named.or(first)
}

/// Classify a cgroup path. `viewer_uid` decides whether user units get their
/// owner's name appended (only for units of *other* users).
pub fn classify(path: &str, users: &HashMap<u32, Arc<str>>, viewer_uid: u32) -> Service {
    let path_clean = sanitize(path);
    if path == "/" || path.is_empty() {
        return Service::simple(ServiceKind::Other, "/", "root cgroup", &path_clean);
    }
    let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let prefix = |i: usize| -> String { format!("/{}", comps[..=i].join("/")) };

    let mut uid = None;
    let mut user_manager: Option<usize> = None;
    let mut last_unit: Option<usize> = None;
    for (i, c) in comps.iter().enumerate() {
        if let Some(u) = c.strip_prefix("user-").and_then(|r| r.strip_suffix(".slice")) {
            uid = u.parse().ok();
        }
        if let Some((runtime, id, name)) = container(&comps, i) {
            let label = sanitize(&format!("{runtime} {name}"));
            let pod = kube_pod(&comps[..i]).map(|p| Arc::from(sanitize(&p)));
            let unit = (c.ends_with(".scope") || c.ends_with(".service")).then(|| Arc::from(sanitize(c)));
            return Service {
                kind: ServiceKind::Container,
                key: sanitize(&prefix(i)).into(),
                search: format!("{} {}", label, c).to_lowercase().into(),
                label: label.into(),
                user_unit: unit.is_some() && user_manager.is_some(),
                unit,
                uid,
                path: path_clean.into(),
                container: Some(ContainerRef { runtime, id: sanitize(&id).into(), pod }),
            };
        }
        if c.starts_with("user@") && c.ends_with(".service") {
            user_manager = Some(i);
        }
        if c.ends_with(".service") || c.ends_with(".scope") {
            last_unit = Some(i);
        }
    }

    // OpenRC (Alpine, Gentoo, Artix) puts each service in /openrc.<name>.
    if let Some(name) = comps.first().and_then(|c| c.strip_prefix("openrc.")) {
        let label = sanitize(name);
        return Service {
            kind: ServiceKind::System,
            key: sanitize(&prefix(0)).into(),
            search: label.to_lowercase().into(),
            label: label.into(),
            unit: None,
            user_unit: false,
            uid,
            path: path_clean.into(),
            container: None,
        };
    }

    let Some(i) = last_unit else {
        // Only slices (or something non-systemd): name it after the leaf.
        let leaf = comps.last().copied().unwrap_or("/");
        let label = unescape(leaf.strip_suffix(".slice").unwrap_or(leaf));
        return Service { uid, ..Service::simple(ServiceKind::Other, &sanitize(path), &sanitize(&label), &path_clean) };
    };

    let c = comps[i];
    let (kind, unit_idx, user_unit) = match user_manager {
        // The user manager process itself lives in user@N.service/init.scope.
        Some(m) if i == m || (i == m + 1 && c == "init.scope") => (ServiceKind::User, m, false),
        Some(_) if c.ends_with(".scope") => (ServiceKind::Scope, i, true),
        Some(_) => (ServiceKind::User, i, true),
        None if c.ends_with(".scope") && c != "init.scope" => (ServiceKind::Scope, i, false),
        None => (ServiceKind::System, i, false),
    };
    let unit = comps[unit_idx];
    let mut label = pretty(unit);
    if kind != ServiceKind::System
        && let Some(u) = uid
        && u != viewer_uid
    {
        let who = users.get(&u).map(|s| s.to_string()).unwrap_or_else(|| u.to_string());
        label = format!("{label} ({who})");
    }
    let label = sanitize(&label);
    let unit = sanitize(unit);
    Service {
        kind,
        key: sanitize(&prefix(unit_idx)).into(),
        search: format!("{label} {unit}").to_lowercase().into(),
        label: label.into(),
        unit: Some(unit.into()),
        user_unit,
        uid,
        path: path_clean.into(),
        container: None,
    }
}

/// Recognize a container cgroup at component `i`; returns (runtime, full
/// ID or name, short display name).
fn container(comps: &[&str], i: usize) -> Option<(&'static str, String, String)> {
    let c = comps[i];
    let prev = if i > 0 { comps[i - 1] } else { "" };
    let kube = comps[..i].iter().any(|p| p.starts_with("kubepods"));
    let scope_id = |prefix: &str| {
        c.strip_prefix(prefix).and_then(|r| r.strip_suffix(".scope")).map(|id| (id.to_ascii_lowercase(), short_id(id)))
    };
    let named = |name: String| Some((name.clone(), name));

    let found = if let Some(id) = scope_id("docker-") {
        Some((if kube { "k8s" } else { "docker" }, id))
    } else if !c.starts_with("libpod-conmon-")
        && let Some(id) = scope_id("libpod-")
    {
        Some(("podman", id))
    } else if let Some(id) = scope_id("cri-containerd-") {
        Some((if kube { "k8s" } else { "containerd" }, id))
    } else if !c.starts_with("crio-conmon-")
        && let Some(id) = scope_id("crio-")
    {
        Some((if kube { "k8s" } else { "cri-o" }, id))
    } else if is_hex_id(c) && (prev == "docker" || (kube && prev.starts_with("pod"))) {
        // cgroupfs drivers: /docker/<id>, /kubepods/<qos>/pod<uid>/<id>
        Some((if prev == "docker" { "docker" } else { "k8s" }, (c.to_ascii_lowercase(), short_id(c))))
    } else if let Some(name) = c.strip_prefix("lxc.payload.") {
        Some(("lxc", named(unescape(name))?))
    } else if prev == "lxc" && i == 1 {
        Some(("lxc", named(unescape(c))?))
    } else if let Some(name) = c.strip_prefix("systemd-nspawn@").and_then(|r| r.strip_suffix(".service")) {
        Some(("nspawn", named(unescape(name))?))
    } else if prev == "machine.slice"
        && let Some(name) = c.strip_prefix("machine-").and_then(|r| r.strip_suffix(".scope"))
    {
        Some(("machine", named(unescape(name))?))
    } else {
        None
    };
    found.map(|(runtime, (id, short))| (runtime, id, short))
}

/// The Kubernetes pod UID in the cgroups above a container: `pod<uid>`
/// (cgroupfs driver) or `kubepods-<qos>-pod<uid>.slice` (systemd driver,
/// which writes the UID's dashes as underscores).
fn kube_pod(above: &[&str]) -> Option<String> {
    if !above.iter().any(|p| p.starts_with("kubepods")) {
        return None;
    }
    above.iter().rev().find_map(|c| {
        let stem = c.strip_suffix(".slice").unwrap_or(c);
        let uid = stem.rsplit_once("-pod").map(|(_, u)| u).or_else(|| stem.strip_prefix("pod"))?;
        let uid = uid.replace('_', "-");
        (!uid.is_empty() && uid.bytes().all(|b| b == b'-' || b.is_ascii_hexdigit())).then_some(uid)
    })
}

fn is_hex_id(s: &str) -> bool {
    s.len() >= 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn short_id(id: &str) -> String {
    id.chars().take(12).collect()
}

/// Human-friendly unit name: drop the type suffix, decode `\x2d`, and turn
/// `app-<launcher>-<AppID>-<random>.scope` into the application ID.
fn pretty(unit: &str) -> String {
    if let Some(stem) = unit.strip_suffix(".service") {
        return unescape(stem);
    }
    let stem = unit.strip_suffix(".scope").unwrap_or(unit);
    if let Some(rest) = stem.strip_prefix("app-") {
        // Trailing instance number or random suffix.
        let rest = match rest.rsplit_once('-') {
            Some((head, tail)) if !head.is_empty() && tail.bytes().all(|b| b.is_ascii_alphanumeric()) => head,
            _ => rest,
        };
        // Real dashes separate launcher and ID; dashes inside the ID are escaped.
        let id = rest.split_once('-').map(|(_, id)| id).unwrap_or(rest);
        return unescape(id);
    }
    // Shorten trailing UUIDs: vte-spawn-d720a4fc-3656-...-9f3a38c337fd.
    let s = unescape(stem);
    // The cut is a byte offset: only split where it falls on a character
    // boundary (names can hold any Unicode after unescaping).
    if s.len() > 36 && s.is_char_boundary(s.len() - 36) {
        let (head, tail) = s.split_at(s.len() - 36);
        if tail.bytes().filter(|&b| b == b'-').count() == 4 && tail.bytes().all(|b| b == b'-' || b.is_ascii_hexdigit())
        {
            return format!("{head}{}", &tail[..8]);
        }
    }
    s
}

/// Decode systemd's `\xHH` escapes.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && b.get(i + 1) == Some(&b'x')
            && let Some(v) = s.get(i + 2..i + 4).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(path: &str) -> Service {
        let users = HashMap::from([(1000, Arc::from("alice")), (1001, Arc::from("bob"))]);
        classify(path, &users, 1000)
    }

    #[test]
    fn picks_unified_path() {
        assert_eq!(cgroup_path("0::/system.slice/sshd.service\n"), Some("/system.slice/sshd.service"));
        let v1 = "12:cpu,cpuacct:/foo\n1:name=systemd:/system.slice/cron.service\n";
        assert_eq!(cgroup_path(v1), Some("/system.slice/cron.service"));
    }

    #[test]
    fn system_services() {
        let s = c("/system.slice/sshd.service");
        assert_eq!(
            (s.kind, &*s.label, s.unit.as_deref(), s.user_unit),
            (ServiceKind::System, "sshd", Some("sshd.service"), false)
        );
        // Sub-cgroups inside a service still belong to it.
        let s = c("/system.slice/systemd-udevd.service/udev");
        assert_eq!((&*s.key, &*s.label), ("/system.slice/systemd-udevd.service", "systemd-udevd"));
        assert_eq!(c("/init.scope").kind, ServiceKind::System);
    }

    #[test]
    fn user_services_and_scopes() {
        let base = "/user.slice/user-1000.slice/user@1000.service";
        let s = c(&format!("{base}/session.slice/pipewire.service"));
        assert_eq!((s.kind, &*s.label, s.user_unit, s.uid), (ServiceKind::User, "pipewire", true, Some(1000)));
        let s = c(&format!("{base}/init.scope"));
        assert_eq!(
            (s.kind, &*s.label, s.unit.as_deref(), s.user_unit),
            (ServiceKind::User, "user@1000", Some("user@1000.service"), false)
        );
        let s = c(&format!("{base}/app.slice/app-gnome-chrome\\x2dabc\\x2dProfile_1-4056.scope"));
        assert_eq!((s.kind, &*s.label), (ServiceKind::Scope, "chrome-abc-Profile_1"));
        assert_eq!(&*c(&format!("{base}/app.slice/app-kiro-6360.scope")).label, "kiro");
        assert_eq!(&*c(&format!("{base}/app.slice/app-gnome-org.kde.kate-7160.scope")).label, "org.kde.kate");
        let s = c(&format!(
            "{base}/app.slice/app-org.gnome.Terminal.slice/vte-spawn-d720a4fc-3656-4d71-aeb4-9f3a38c337fd.scope"
        ));
        assert_eq!(&*s.label, "vte-spawn-d720a4fc");
        let s = c("/user.slice/user-1000.slice/session-3.scope");
        assert_eq!((s.kind, &*s.label, s.user_unit), (ServiceKind::Scope, "session-3", false));
    }

    #[test]
    fn other_users_are_named_and_kept_apart() {
        let a = c("/user.slice/user-1000.slice/user@1000.service/session.slice/pipewire.service");
        let b = c("/user.slice/user-1001.slice/user@1001.service/session.slice/pipewire.service");
        assert_ne!(a.key, b.key);
        assert_eq!(&*b.label, "pipewire (bob)");
    }

    #[test]
    fn containers() {
        let id = "4f1c0e2a9b7d".repeat(5) + "abcd";
        let s = c(&format!("/system.slice/docker-{id}.scope"));
        assert_eq!((s.kind, &*s.label), (ServiceKind::Container, "docker 4f1c0e2a9b7d"));
        // A container running its own systemd groups as the container.
        let s = c(&format!("/system.slice/docker-{id}.scope/system.slice/nginx.service"));
        assert_eq!((s.kind, &*s.key), (ServiceKind::Container, &*format!("/system.slice/docker-{id}.scope")));
        assert_eq!(&*c(&format!("/docker/{id}")).label, "docker 4f1c0e2a9b7d");
        let k = format!(
            "/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod1234.slice/cri-containerd-{id}.scope"
        );
        assert_eq!(&*c(&k).label, "k8s 4f1c0e2a9b7d");
        let r = c(&k).container.unwrap();
        assert_eq!((r.runtime, &*r.id, r.pod.as_deref()), ("k8s", &*id, Some("1234")));
        assert_eq!(&*c(&format!("/kubepods/burstable/pod1234/{id}")).label, "k8s 4f1c0e2a9b7d");
        let p = format!("/user.slice/user-1000.slice/user@1000.service/user.slice/libpod-{id}.scope");
        let s = c(&p);
        assert_eq!((s.kind, &*s.label, s.user_unit), (ServiceKind::Container, "podman 4f1c0e2a9b7d", true));
        let r = c(&p).container.unwrap();
        assert_eq!((r.runtime, &*r.id, r.pod), ("podman", &*id, None));
        assert_eq!(&*c("/lxc.payload.web1").label, "lxc web1");
        assert_eq!(&*c("/lxc.payload.web1").container.unwrap().id, "web1");
        assert_eq!(&*c("/machine.slice/systemd-nspawn@deb.service").label, "nspawn deb");
        assert_eq!(&*c("/machine.slice/machine-qemu\\x2d1\\x2dwin11.scope").label, "machine qemu-1-win11");
        // The daemons themselves are ordinary services.
        assert_eq!(c("/system.slice/docker.service").kind, ServiceKind::System);
        assert_eq!(c("/system.slice/docker.service").container, None);
    }

    #[test]
    fn kubernetes_pod_uids() {
        let id = "ab".repeat(32);
        let uid = "5f0c1a2b-3c4d-4e5f-8a9b-0c1d2e3f4a5b";
        let systemd = format!(
            "/kubepods.slice/kubepods-besteffort.slice/kubepods-besteffort-pod{}.slice/cri-containerd-{id}.scope",
            uid.replace('-', "_")
        );
        let guaranteed =
            format!("/kubepods.slice/kubepods-pod{}.slice/cri-containerd-{id}.scope", uid.replace('-', "_"));
        let cgroupfs = format!("/kubepods/burstable/pod{uid}/{id}");
        for path in [systemd, guaranteed, cgroupfs] {
            assert_eq!(c(&path).container.unwrap().pod.as_deref(), Some(uid), "{path}");
        }
    }

    #[test]
    fn openrc_services() {
        let s = c("/openrc.sshd");
        assert_eq!((s.kind, &*s.label, s.unit.as_deref()), (ServiceKind::System, "sshd", None));
        assert_eq!(cgroup_path("2:cpu:/x\n1:name=openrc:/openrc.crond\n"), Some("/openrc.crond"));
    }

    #[test]
    fn fallbacks() {
        assert_eq!(c("/").kind, ServiceKind::Other);
        let s = c("/user.slice/user-1000.slice");
        assert_eq!((s.kind, &*s.label, s.uid), (ServiceKind::Other, "user-1000", Some(1000)));
        let s = c("/custom/thing");
        assert_eq!((s.kind, &*s.label), (ServiceKind::Other, "thing"));
    }

    #[test]
    fn long_unicode_scope_names_do_not_panic() {
        // 19 × "é" (2 bytes each) + "a" = 39 bytes: byte 3 is mid-character.
        let stem = "é".repeat(19) + "a";
        assert_eq!(&*c(&format!("/system.slice/{stem}.scope")).label, stem);
        // The same name as systemd writes it, escaped.
        let escaped = "\\xc3\\xa9".repeat(19) + "a";
        assert_eq!(&*c(&format!("/system.slice/{escaped}.scope")).label, stem);
        // Mixed: Unicode prefix in front of a real UUID still shortens.
        let s = c("/system.slice/ünï-d720a4fc-3656-4d71-aeb4-9f3a38c337fd.scope");
        assert_eq!(&*s.label, "ünï-d720a4fc");
        // Many lengths, so every boundary position is exercised.
        for n in 30..45 {
            let _ = c(&format!("/user.slice/{}x.scope", "€".repeat(n)));
        }
    }

    #[test]
    fn hostile_names_are_sanitized() {
        let s = c("/system.slice/evil\x1b[2J.service");
        assert!(!s.label.contains('\x1b') && !s.unit.as_deref().unwrap().contains('\x1b'));
    }
}
