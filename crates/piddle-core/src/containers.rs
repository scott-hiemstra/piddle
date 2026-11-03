//! Names for the containers the cgroups reveal, from the runtimes' own APIs.
//!
//! [`crate::service`] finds containers in `/proc/<pid>/cgroup`, but knows
//! Docker and Podman ones only by ID. Their names, Compose project and
//! service, and Podman pod come from the runtime's API socket. Only read-only
//! list calls are made (`GET /containers/json`), never anything that changes a
//! container, and only for runtimes that have containers running, so a
//! machine without containers never touches a socket.
//!
//! Docker's socket is readable by root and the `docker` group only. That group
//! is root-equivalent, so without access PIDdle says why the containers keep
//! their short IDs and doesn't suggest joining it. Rootless Podman's socket
//! belongs to the user.
//!
//! [`Watcher`] queries on a background thread so a slow or hung daemon never
//! delays a sample; [`place`] and [`choices`] turn the result into what the
//! interfaces show.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::encode::sanitize;
use crate::sampler::ProcInfo;
use crate::service::{ContainerRef, Service};

/// A container as its runtime describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Container {
    pub runtime: Runtime,
    /// Full ID, lowercase.
    pub id: Arc<str>,
    pub name: Arc<str>,
    pub image: Arc<str>,
    pub manager: Manager,
}

/// What a container was started by, beyond the runtime itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Manager {
    /// Docker Compose or podman-compose (`com.docker.compose.*` labels).
    Compose {
        project: Arc<str>,
        service: Arc<str>,
    },
    /// A Podman pod; `infra` is the pod's placeholder container.
    PodmanPod {
        pod: Arc<str>,
        infra: bool,
    },
    None,
}

/// The runtimes whose APIs PIDdle reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Runtime {
    Docker,
    Podman,
}

impl Runtime {
    /// The [`ContainerRef::runtime`] name this API covers.
    pub fn cgroup_name(self) -> &'static str {
        match self {
            Runtime::Docker => "docker",
            Runtime::Podman => "podman",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Runtime::Docker => "Docker",
            Runtime::Podman => "Podman",
        }
    }

    fn of(c: &ContainerRef) -> Option<Runtime> {
        match c.runtime {
            "docker" => Some(Runtime::Docker),
            "podman" => Some(Runtime::Podman),
            _ => None,
        }
    }
}

/// How the last query of one runtime went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Access {
    /// Listed; this many running containers.
    Listed(usize),
    /// No API socket found.
    NoSocket,
    /// A socket exists but isn't readable by this user.
    Denied(PathBuf),
    /// Anything else (refused, timed out, unexpected reply).
    Failed(String),
}

/// What the runtimes reported, keyed by full container ID.
#[derive(Clone, Debug, Default)]
pub struct Inventory {
    pub containers: HashMap<Arc<str>, Container>,
    /// One entry per runtime queried so far.
    pub access: BTreeMap<Runtime, Access>,
}

impl Inventory {
    pub fn get(&self, c: &ContainerRef) -> Option<&Container> {
        let rt = Runtime::of(c)?;
        self.containers.get(&c.id).filter(|k| k.runtime == rt)
    }

    /// Why some containers have no names, for the picker and the details:
    /// one sentence per runtime that couldn't be listed.
    pub fn notes(&self) -> Vec<String> {
        self.access
            .iter()
            .filter_map(|(rt, a)| {
                let t = rt.title();
                Some(match a {
                    Access::Listed(_) => return None,
                    Access::Denied(p) => format!(
                        "{t} containers are shown by ID: {} is readable only by root and the {} group.",
                        sanitize(&p.display().to_string()),
                        rt.cgroup_name()
                    ),
                    Access::NoSocket if *rt == Runtime::Podman => {
                        "Podman containers are shown by ID: its API socket isn't running \
                         (systemctl --user start podman.socket starts it for your own containers)."
                            .to_string()
                    }
                    Access::NoSocket => format!("{t} containers are shown by ID: no {t} API socket was found."),
                    Access::Failed(e) => format!("{t} containers are shown by ID: {}", sanitize(e)),
                })
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Querying

/// API sockets to try for a runtime, most specific first.
fn sockets(rt: Runtime) -> Vec<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let mut v = Vec::new();
    match rt {
        Runtime::Docker => {
            if let Some(p) = std::env::var("DOCKER_HOST").ok().as_deref().and_then(|h| h.strip_prefix("unix://")) {
                v.push(PathBuf::from(p));
            }
            v.push("/var/run/docker.sock".into());
            if let Some(d) = &runtime_dir {
                v.push(d.join("docker.sock")); // rootless Docker
            }
        }
        Runtime::Podman => {
            if let Some(d) = &runtime_dir {
                v.push(d.join("podman/podman.sock"));
            }
            v.push("/run/podman/podman.sock".into());
        }
    }
    v
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Raw {
    id: String,
    #[serde(default)]
    names: Vec<String>,
    #[serde(default)]
    image: String,
    #[serde(default)]
    labels: Option<HashMap<String, String>>,
    /// Podman's own endpoint only.
    #[serde(default)]
    pod_name: Option<String>,
    #[serde(default)]
    is_infra: bool,
}

/// Parse a `GET /containers/json` reply (Docker's, or Podman's own
/// `/libpod/containers/json`).
pub fn parse_list(rt: Runtime, body: &[u8]) -> Result<Vec<Container>, String> {
    let raw: Vec<Raw> = serde_json::from_slice(body).map_err(|e| format!("unexpected reply ({e})"))?;
    Ok(raw.into_iter().map(|r| Container::from_raw(rt, r)).collect())
}

impl Container {
    fn from_raw(runtime: Runtime, r: Raw) -> Container {
        let labels = r.labels.unwrap_or_default();
        let label = |keys: &[&str]| keys.iter().find_map(|k| labels.get(*k)).filter(|v| !v.is_empty());
        let project = label(&["com.docker.compose.project", "io.podman.compose.project"]);
        let service = label(&["com.docker.compose.service", "io.podman.compose.service"]);
        let name = r.names.first().map(|n| n.trim_start_matches('/')).unwrap_or_default();
        let id = r.id.to_ascii_lowercase();
        let name = if name.is_empty() { id.chars().take(12).collect() } else { name.to_string() };
        let manager = match (project, service, r.pod_name.as_deref().filter(|p| !p.is_empty())) {
            (Some(p), Some(s), _) => Manager::Compose { project: sanitize(p).into(), service: sanitize(s).into() },
            (_, _, Some(pod)) => Manager::PodmanPod { pod: sanitize(pod).into(), infra: r.is_infra },
            _ => Manager::None,
        };
        Container {
            runtime,
            id: sanitize(&id).into(),
            name: sanitize(&name).into(),
            image: sanitize(&r.image).into(),
            manager,
        }
    }
}

const TIMEOUT: Duration = Duration::from_secs(2);
const MAX_REPLY: u64 = 32 << 20;

/// One read-only request over a Unix socket. HTTP/1.0, so the daemon closes
/// the connection after replying and the body ends there.
fn get(socket: &Path, path: &str) -> std::io::Result<(u16, Vec<u8>)> {
    let mut s = UnixStream::connect(socket)?;
    s.set_read_timeout(Some(TIMEOUT))?;
    s.set_write_timeout(Some(TIMEOUT))?;
    let request =
        format!("GET {path} HTTP/1.0\r\nHost: localhost\r\nUser-Agent: piddle\r\nAccept: application/json\r\n\r\n");
    s.write_all(request.as_bytes())?;
    let mut reply = Vec::new();
    s.take(MAX_REPLY).read_to_end(&mut reply)?;
    parse_reply(&reply).ok_or_else(|| std::io::Error::other("unexpected reply"))
}

/// Split an HTTP reply into status and body, undoing chunked encoding.
fn parse_reply(reply: &[u8]) -> Option<(u16, Vec<u8>)> {
    let end = reply.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&reply[..end]).ok()?;
    let body = &reply[end + 4..];
    let status = head.split(' ').nth(1)?.parse().ok()?;
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    if !chunked {
        return Some((status, body.to_vec()));
    }
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let nl = rest.windows(2).position(|w| w == b"\r\n")?;
        let size = std::str::from_utf8(&rest[..nl]).ok()?.split(';').next()?.trim();
        let n = usize::from_str_radix(size, 16).ok()?;
        rest = &rest[nl + 2..];
        if n == 0 {
            return Some((status, out));
        }
        out.extend_from_slice(rest.get(..n)?);
        rest = rest.get(n + 2..)?;
    }
}

/// List one runtime's running containers through the first socket that
/// answers.
pub fn query(rt: Runtime) -> (Access, Vec<Container>) {
    query_sockets(rt, &sockets(rt))
}

fn query_sockets(rt: Runtime, socks: &[PathBuf]) -> (Access, Vec<Container>) {
    let mut result = Access::NoSocket;
    for sock in socks {
        // Podman's own endpoint adds pods; its Docker-compatible one is the
        // fallback (and what a Docker socket answers).
        let paths: &[&str] = match rt {
            Runtime::Podman => &["/v4.0.0/libpod/containers/json", "/containers/json"],
            Runtime::Docker => &["/containers/json"],
        };
        for path in paths {
            match get(sock, path) {
                Ok((200, body)) => {
                    return match parse_list(rt, &body) {
                        Ok(list) => (Access::Listed(list.len()), list),
                        Err(e) => (Access::Failed(e), Vec::new()),
                    };
                }
                Ok((404, _)) => continue,
                Ok((code, _)) => {
                    result = Access::Failed(format!("{} answered HTTP {code}", sock.display()));
                    break;
                }
                Err(e) => {
                    use std::io::ErrorKind::*;
                    match e.kind() {
                        NotFound => {}
                        PermissionDenied => result = Access::Denied(sock.clone()),
                        // A leftover socket file with nothing listening.
                        ConnectionRefused if matches!(result, Access::NoSocket) => {}
                        _ => result = Access::Failed(format!("{}: {e}", sock.display())),
                    }
                    break;
                }
            }
        }
    }
    (result, Vec::new())
}

// ---------------------------------------------------------------------------
// Background watcher

/// Re-list a runtime this often while it has containers running.
const REFRESH: Duration = Duration::from_secs(15);
/// But sooner when a container appears that the last list didn't have.
const UNKNOWN_REFRESH: Duration = Duration::from_secs(2);

/// Keeps an [`Inventory`] current on a background thread.
pub struct Watcher {
    current: Arc<Mutex<Arc<Inventory>>>,
    busy: Arc<AtomicBool>,
    tx: mpsc::Sender<Vec<Runtime>>,
    last: HashMap<Runtime, Instant>,
}

impl Default for Watcher {
    fn default() -> Self {
        Self::spawn()
    }
}

impl Watcher {
    pub fn spawn() -> Self {
        let current = Arc::new(Mutex::new(Arc::new(Inventory::default())));
        let busy = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Vec<Runtime>>();
        let (cur, b) = (current.clone(), busy.clone());
        std::thread::Builder::new()
            .name("piddle-containers".into())
            .spawn(move || {
                while let Ok(runtimes) = rx.recv() {
                    let mut inv = Inventory::clone(&cur.lock().unwrap());
                    for rt in runtimes {
                        let (access, list) = query(rt);
                        // Replace this runtime's entries, keep the others'.
                        inv.containers.retain(|_, c| c.runtime != rt);
                        if matches!(access, Access::Listed(_)) {
                            inv.containers.extend(list.into_iter().map(|c| (c.id.clone(), c)));
                        }
                        inv.access.insert(rt, access);
                    }
                    *cur.lock().unwrap() = Arc::new(inv);
                    b.store(false, Ordering::Release);
                }
            })
            .expect("spawn container watcher thread");
        Watcher { current, busy, tx, last: HashMap::new() }
    }

    pub fn current(&self) -> Arc<Inventory> {
        self.current.lock().unwrap().clone()
    }

    /// Ask for a refresh if the processes show containers the inventory
    /// lacks, or it is due. Cheap; call once per sample.
    pub fn update(&mut self, procs: &[ProcInfo]) {
        if self.busy.load(Ordering::Acquire) {
            return;
        }
        let inv = self.current();
        let mut seen: HashMap<Runtime, bool> = HashMap::new(); // runtime -> has unknown
        let mut checked = HashSet::new();
        for p in procs {
            let Some(c) = &p.service.container else { continue };
            let Some(rt) = Runtime::of(c) else { continue };
            if !checked.insert(Arc::as_ptr(&p.service)) {
                continue;
            }
            let unknown = !inv.containers.contains_key(&c.id);
            *seen.entry(rt).or_default() |= unknown;
        }
        let now = Instant::now();
        let due: Vec<Runtime> = seen
            .into_iter()
            .filter(|&(rt, unknown)| {
                // Unknown IDs can't be named without access; don't retry fast.
                let listed = matches!(inv.access.get(&rt), Some(Access::Listed(_)));
                let wait = if unknown && listed { UNKNOWN_REFRESH } else { REFRESH };
                self.last.get(&rt).is_none_or(|t| now.duration_since(*t) >= wait)
            })
            .map(|(rt, _)| rt)
            .collect();
        if due.is_empty() {
            return;
        }
        for rt in &due {
            self.last.insert(*rt, now);
        }
        self.busy.store(true, Ordering::Release);
        if self.tx.send(due).is_err() {
            self.busy.store(false, Ordering::Release);
        }
    }
}

// ---------------------------------------------------------------------------
// What the interfaces show

/// What groups containers in the containers view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GroupKind {
    Compose,
    PodmanPod,
    KubePod,
}

impl GroupKind {
    pub fn label(self) -> &'static str {
        match self {
            GroupKind::Compose => "compose",
            GroupKind::PodmanPod => "pod",
            GroupKind::KubePod => "k8s pod",
        }
    }
}

/// A Compose project, Podman pod or Kubernetes pod.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Group {
    pub key: Arc<str>,
    pub label: Arc<str>,
    pub kind: GroupKind,
}

/// Where a process's container sits in the containers view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub group: Option<Group>,
    /// The container: its service key (cgroup path), stable while it runs.
    pub key: Arc<str>,
    /// Its name, or "docker 4f1c0e2a9b7d" when the runtime couldn't be asked.
    pub name: Arc<str>,
    /// Orders containers within a group: Compose service, then name, so a
    /// service's replicas sit side by side.
    pub order: Arc<str>,
    /// One line for the details: what runs it and from which image.
    pub detail: String,
}

/// Place a process's service in the containers view; `None` when it isn't a
/// container.
pub fn place(svc: &Service, inv: &Inventory) -> Option<Placement> {
    let c = svc.container.as_ref()?;
    let known = inv.get(c);
    let name: Arc<str> = known.map(|k| k.name.clone()).unwrap_or_else(|| svc.label.clone());
    let image = known.map(|k| format!(", image {}", k.image)).filter(|_| known.is_some_and(|k| !k.image.is_empty()));
    let image = image.unwrap_or_default();
    let rt = c.runtime;
    let (group, order, detail) = match known.map(|k| &k.manager) {
        Some(Manager::Compose { project, service }) => (
            Some(Group {
                key: format!("compose:{rt}:{project}").into(),
                label: project.clone(),
                kind: GroupKind::Compose,
            }),
            format!("{service}\0{name}"),
            format!("{rt} container, service {service} of Compose project {project}{image}"),
        ),
        Some(Manager::PodmanPod { pod, infra }) => (
            Some(Group { key: format!("pod:{rt}:{pod}").into(), label: pod.clone(), kind: GroupKind::PodmanPod }),
            format!("{}\0{name}", if *infra { "0" } else { "1" }),
            format!("{} of Podman pod {pod}{image}", if *infra { "infra container" } else { "container" }),
        ),
        _ => match &c.pod {
            Some(uid) => {
                let short: String = uid.chars().take(8).collect();
                (
                    Some(Group {
                        key: format!("k8s:{uid}").into(),
                        label: format!("pod {short}").into(),
                        kind: GroupKind::KubePod,
                    }),
                    name.to_string(),
                    format!("container of Kubernetes pod {uid} (kpiddle shows the cluster by name)"),
                )
            }
            None => (None, name.to_string(), format!("{rt} container{image}")),
        },
    };
    Some(Placement { group, key: svc.key.clone(), name, order: order.into(), detail })
}

/// What the containers picker limits the view to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Filter {
    /// Every process: no limit.
    #[default]
    All,
    /// Every process in any container.
    Containers,
    /// One Compose project or pod, by [`Group::key`].
    Group(Arc<str>),
    /// One container, by [`Placement::key`]; `name` finds it again if it is
    /// recreated (a Compose `up` gives it a new ID).
    Container { key: Arc<str>, name: Arc<str> },
}

impl Filter {
    pub fn is_all(&self) -> bool {
        *self == Filter::All
    }

    /// Whether a process with this placement passes the filter.
    pub fn shows(&self, placed: Option<&Placement>) -> bool {
        match (self, placed) {
            (Filter::All, _) => true,
            (_, None) => false,
            (Filter::Containers, Some(_)) => true,
            (Filter::Group(g), Some(p)) => p.group.as_ref().is_some_and(|pg| pg.key == *g),
            (Filter::Container { key, .. }, Some(p)) => p.key == *key,
        }
    }
}

/// One picker entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub filter: Filter,
    pub label: String,
    /// 0 for the top entries, groups and standalone containers; 1 for the
    /// containers inside a group.
    pub depth: u8,
    /// Processes it would show.
    pub procs: usize,
}

/// The picker's entries for the current processes: "All processes", "All
/// containers", then each group followed by its containers, then the
/// standalone containers. Empty when no process is in a container, so the
/// interfaces show no picker at all.
pub fn choices(procs: &[ProcInfo], inv: &Inventory) -> Vec<Choice> {
    let mut placed: HashMap<*const Service, Option<Placement>> = HashMap::new();
    // Containers in display order, each with its process count.
    type Members = BTreeMap<(Arc<str>, Arc<str>), (Placement, usize)>;
    let mut groups: BTreeMap<Arc<str>, (Group, Members)> = BTreeMap::new();
    let mut alone = Members::new();
    let mut total = 0;
    for p in procs {
        let pl = placed.entry(Arc::as_ptr(&p.service)).or_insert_with(|| place(&p.service, inv));
        let Some(pl) = pl else { continue };
        total += 1;
        let slot = match &pl.group {
            Some(g) => &mut groups.entry(g.key.clone()).or_insert_with(|| (g.clone(), BTreeMap::new())).1,
            None => &mut alone,
        };
        slot.entry((pl.order.clone(), pl.key.clone())).or_insert_with(|| (pl.clone(), 0)).1 += 1;
    }
    if total == 0 {
        return Vec::new();
    }
    let mut v = vec![
        Choice { filter: Filter::All, label: "All processes".into(), depth: 0, procs: procs.len() },
        Choice { filter: Filter::Containers, label: "All containers".into(), depth: 0, procs: total },
    ];
    let container = |(pl, n): (Placement, usize), depth| Choice {
        filter: Filter::Container { key: pl.key, name: pl.name.clone() },
        label: pl.name.to_string(),
        depth,
        procs: n,
    };
    let mut sorted: Vec<_> = groups.into_values().collect();
    sorted.sort_by(|a, b| (a.0.kind, &a.0.label).cmp(&(b.0.kind, &b.0.label)));
    for (g, members) in sorted {
        v.push(Choice {
            label: format!("{}  ({})", g.label, g.kind.label()),
            filter: Filter::Group(g.key),
            depth: 0,
            procs: members.values().map(|m| m.1).sum(),
        });
        v.extend(members.into_values().map(|m| container(m, 1)));
    }
    v.extend(alone.into_values().map(|m| container(m, 0)));
    v
}

/// Check the current filter against fresh choices. A container that was
/// recreated under the same name is followed to its new key; `None` means
/// what the filter named is gone (stopped, or the last process left).
pub fn resolve(filter: &Filter, choices: &[Choice]) -> Option<Filter> {
    match filter {
        Filter::All => Some(Filter::All),
        _ if choices.iter().any(|c| c.filter == *filter) => Some(filter.clone()),
        Filter::Container { name, .. } => choices
            .iter()
            .find(|c| matches!(&c.filter, Filter::Container { name: n, .. } if n == name))
            .map(|c| c.filter.clone()),
        _ => None,
    }
}

/// How to name a filter in a status line: "open-webui", "all containers".
pub fn filter_label(filter: &Filter, choices: &[Choice]) -> String {
    match filter {
        Filter::All => "all processes".into(),
        Filter::Containers => "all containers".into(),
        Filter::Container { name, .. } => name.to_string(),
        Filter::Group(_) => {
            choices.iter().find(|c| c.filter == *filter).map(|c| c.label.clone()).unwrap_or_else(|| {
                // Gone: the key is "kind:runtime:name".
                match filter {
                    Filter::Group(k) => k.rsplit(':').next().unwrap_or(k).to_string(),
                    _ => unreachable!(),
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::classify;

    const DOCKER_LIST: &str = r#"[
      {"Id":"B7ECD6BACEBB00000000000000000000000000000000000000000000000000aa","Names":["/open-webui"],
       "Image":"ghcr.io/open-webui/open-webui:main","State":"running",
       "Labels":{"com.docker.compose.project":"open-web-ui","com.docker.compose.service":"open-webui"}},
      {"Id":"da02c68f5d9c00000000000000000000000000000000000000000000000000bb","Names":["/kind\u001b[2J-node"],
       "Image":"kindest/node","Labels":{}},
      {"Id":"cc","Names":[],"Image":"","Labels":null}
    ]"#;

    #[test]
    fn parses_docker_lists() {
        let l = parse_list(Runtime::Docker, DOCKER_LIST.as_bytes()).unwrap();
        assert_eq!(&*l[0].id, "b7ecd6bacebb00000000000000000000000000000000000000000000000000aa");
        assert_eq!(&*l[0].name, "open-webui");
        assert_eq!(l[0].manager, Manager::Compose { project: "open-web-ui".into(), service: "open-webui".into() });
        assert!(!l[1].name.contains('\x1b'), "names are sanitized");
        assert_eq!(l[1].manager, Manager::None);
        assert_eq!(&*l[2].name, "cc", "no name: the short ID");
        assert!(parse_list(Runtime::Docker, b"{\"message\":\"nope\"}").is_err());
    }

    #[test]
    fn parses_podman_pods() {
        let body = r#"[{"Id":"aa","Names":["web"],"Image":"nginx","Labels":null,"Pod":"p1","PodName":"shop","IsInfra":false},
                       {"Id":"bb","Names":["shop-infra"],"Image":"","Pod":"p1","PodName":"shop","IsInfra":true},
                       {"Id":"cc","Names":["db"],"Image":"pg","PodName":"",
                        "Labels":{"io.podman.compose.project":"app","io.podman.compose.service":"db"}}]"#;
        let l = parse_list(Runtime::Podman, body.as_bytes()).unwrap();
        assert_eq!(l[0].manager, Manager::PodmanPod { pod: "shop".into(), infra: false });
        assert_eq!(l[1].manager, Manager::PodmanPod { pod: "shop".into(), infra: true });
        assert_eq!(l[2].manager, Manager::Compose { project: "app".into(), service: "db".into() });
    }

    #[test]
    fn http_replies() {
        let plain = b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n[]";
        assert_eq!(parse_reply(plain), Some((200, b"[]".to_vec())));
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[{\r\n3;x=y\r\n}, \r\n2\r\n{}\r\n1\r\n]\r\n0\r\n\r\n";
        assert_eq!(parse_reply(chunked), Some((200, b"[{}, {}]".to_vec())));
        assert_eq!(parse_reply(b"HTTP/1.0 404 Not Found\r\n\r\n"), Some((404, vec![])));
        assert_eq!(parse_reply(b"garbage"), None);
        assert_eq!(parse_reply(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n"), None);
    }

    /// A one-shot fake daemon on a Unix socket in a temporary directory.
    fn serve(replies: Vec<&'static str>) -> (PathBuf, std::thread::JoinHandle<Vec<String>>) {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("piddle-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("api.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let h = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for reply in replies {
                let (mut s, _) = listener.accept().unwrap();
                let mut req = Vec::new();
                let mut buf = [0u8; 256];
                while !req.ends_with(b"\r\n\r\n") {
                    let n = s.read(&mut buf).unwrap();
                    assert!(n > 0, "request ended early");
                    req.extend_from_slice(&buf[..n]);
                }
                requests.push(String::from_utf8_lossy(&req).lines().next().unwrap_or("").to_string());
                s.write_all(reply.as_bytes()).unwrap();
            }
            let _ = std::fs::remove_dir_all(&dir);
            requests
        });
        (sock, h)
    }

    #[test]
    fn queries_over_a_socket_read_only() {
        let reply = "HTTP/1.0 200 OK\r\n\r\n[{\"Id\":\"ab\",\"Names\":[\"/x\"]}]";
        let (sock, h) = serve(vec![reply]);
        let (access, list) = query_sockets(Runtime::Docker, &[sock]);
        assert_eq!(access, Access::Listed(1));
        assert_eq!(&*list[0].name, "x");
        assert_eq!(h.join().unwrap(), ["GET /containers/json HTTP/1.0"]);

        // Podman: its own endpoint first, then the compatible one.
        let (sock, h) = serve(vec!["HTTP/1.0 404 Not Found\r\n\r\n", reply]);
        let (access, _) = query_sockets(Runtime::Podman, &[sock]);
        assert_eq!(access, Access::Listed(1));
        assert_eq!(h.join().unwrap(), ["GET /v4.0.0/libpod/containers/json HTTP/1.0", "GET /containers/json HTTP/1.0"]);

        let (sock, h) = serve(vec!["HTTP/1.0 500 Oops\r\n\r\n"]);
        assert!(matches!(query_sockets(Runtime::Docker, &[sock]).0, Access::Failed(_)));
        h.join().unwrap();

        // A socket this user may not open (as Docker's is without the group).
        // Root opens it anyway, so skip the check there.
        if unsafe { libc::geteuid() } != 0 {
            use std::os::unix::fs::PermissionsExt;
            let dir = std::env::temp_dir().join(format!("piddle-test-{}-denied", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let sock = dir.join("api.sock");
            let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
            std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o000)).unwrap();
            assert_eq!(query_sockets(Runtime::Docker, std::slice::from_ref(&sock)).0, Access::Denied(sock.clone()));
            let _ = std::fs::remove_dir_all(sock.parent().unwrap());
        }

        let missing = std::env::temp_dir().join("piddle-no-such-dir/docker.sock");
        assert_eq!(query_sockets(Runtime::Docker, &[missing]).0, Access::NoSocket);
    }

    #[test]
    fn notes_explain_missing_names_without_suggesting_the_docker_group() {
        let mut inv = Inventory::default();
        inv.access.insert(Runtime::Docker, Access::Denied("/var/run/docker.sock".into()));
        inv.access.insert(Runtime::Podman, Access::Listed(0));
        let notes = inv.notes();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("readable only by root and the docker group"));
        assert!(!notes[0].contains("usermod") && !notes[0].contains("join"));
    }

    fn proc_in(pid: i32, path: &str) -> ProcInfo {
        let svc = Arc::new(classify(path, &HashMap::new(), 1000));
        ProcInfo {
            pid,
            ppid: 1,
            name: "p".into(),
            app: "p".into(),
            cmdline: "".into(),
            search_text: "".into(),
            user: "u".into(),
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
            service: svc,
        }
    }

    fn id(c: char) -> String {
        c.to_string().repeat(64)
    }

    fn inventory() -> Inventory {
        let mut inv = Inventory::default();
        let mk = |c: char, name: &str, manager: Manager| Container {
            runtime: Runtime::Docker,
            id: id(c).into(),
            name: name.into(),
            image: "img".into(),
            manager,
        };
        let compose = |s: &str| Manager::Compose { project: "shop".into(), service: s.into() };
        for c in [
            mk('a', "shop-web-2", compose("web")),
            mk('b', "shop-web-1", compose("web")),
            mk('c', "shop-db-1", compose("db")),
            mk('d', "solo", Manager::None),
        ] {
            inv.containers.insert(c.id.clone(), c);
        }
        inv.access.insert(Runtime::Docker, Access::Listed(4));
        inv
    }

    fn procs() -> Vec<ProcInfo> {
        let d = |c| format!("/system.slice/docker-{}.scope", id(c));
        vec![
            proc_in(1, "/init.scope"),
            proc_in(10, &d('a')),
            proc_in(11, &d('a')),
            proc_in(20, &d('b')),
            proc_in(30, &d('c')),
            proc_in(40, &d('d')),
            proc_in(50, &d('e')), // not in the list (yet)
            proc_in(60, &format!("/kubepods.slice/kubepods-pod1234_ab.slice/cri-containerd-{}.scope", id('f'))),
        ]
    }

    #[test]
    fn places_containers_by_manager() {
        let inv = inventory();
        let p = procs();
        assert_eq!(place(&p[0].service, &inv), None);
        let web = place(&p[1].service, &inv).unwrap();
        assert_eq!(web.group.as_ref().map(|g| (&*g.label, g.kind)), Some(("shop", GroupKind::Compose)));
        assert_eq!(&*web.name, "shop-web-2");
        assert!(web.detail.contains("service web of Compose project shop"));
        let unknown = place(&p[6].service, &inv).unwrap();
        assert_eq!((unknown.group, &*unknown.name), (None, "docker eeeeeeeeeeee"));
        let k8s = place(&p[7].service, &inv).unwrap();
        assert_eq!(k8s.group.map(|g| (g.label.to_string(), g.kind)), Some(("pod 1234-ab".into(), GroupKind::KubePod)));
    }

    #[test]
    fn picker_lists_groups_then_their_containers() {
        let inv = inventory();
        let p = procs();
        let c = choices(&p, &inv);
        let rows: Vec<(String, u8, usize)> = c.iter().map(|c| (c.label.clone(), c.depth, c.procs)).collect();
        let row = |l: &str, d, n| (l.to_string(), d, n);
        assert_eq!(
            rows,
            [
                row("All processes", 0, 8),
                row("All containers", 0, 7),
                row("shop  (compose)", 0, 4),
                // Service order (db before web), replicas side by side.
                row("shop-db-1", 1, 1),
                row("shop-web-1", 1, 1),
                row("shop-web-2", 1, 2),
                row("pod 1234-ab  (k8s pod)", 0, 1),
                row(&format!("k8s {}", "f".repeat(12)), 1, 1),
                row(&format!("docker {}", "e".repeat(12)), 0, 1),
                row("solo", 0, 1),
            ]
        );
        // No containers: no picker.
        assert!(choices(&p[..1], &inv).is_empty());
    }

    #[test]
    fn filters_and_resolution() {
        let inv = inventory();
        let p = procs();
        let c = choices(&p, &inv);
        let count = |f: &Filter| p.iter().filter(|x| f.shows(place(&x.service, &inv).as_ref())).count();
        let shop = c[2].filter.clone();
        assert_eq!((count(&Filter::All), count(&Filter::Containers), count(&shop)), (8, 7, 4));
        let web2 = c[5].filter.clone();
        assert_eq!(count(&web2), 2);

        // Recreated under the same name (new ID): followed.
        let mut inv2 = inv.clone();
        let mut moved = inv2.containers.remove(&*id('a')).unwrap();
        moved.id = id('9').into();
        inv2.containers.insert(moved.id.clone(), moved);
        let mut p2 = p.clone();
        p2[1] = proc_in(10, &format!("/system.slice/docker-{}.scope", id('9')));
        p2.remove(2);
        let c2 = choices(&p2, &inv2);
        let followed = resolve(&web2, &c2).unwrap();
        assert_ne!(followed, web2);
        assert_eq!(filter_label(&followed, &c2), "shop-web-2");

        // Stopped: gone.
        let c3 = choices(&p[..2], &inv);
        assert_eq!(resolve(&c[9].filter, &c3), None);
        assert_eq!(resolve(&Filter::Group("compose:docker:gone".into()), &c3), None);
        assert_eq!(filter_label(&Filter::Group("compose:docker:gone".into()), &c3), "gone");
        assert_eq!(resolve(&Filter::All, &[]), Some(Filter::All));
    }
}
