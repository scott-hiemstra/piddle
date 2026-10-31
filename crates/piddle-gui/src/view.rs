//! Bubble model, layout simulation and rendering.
//!
//! Positions are driven by a damped, position-based force simulation. Existing
//! bubbles are never re-placed: they only drift when neighbours grow, shrink or
//! appear, which keeps the picture stable and readable. Radius and color are
//! eased towards each new sample so changes animate smoothly between samples.

use std::collections::HashMap;
use std::f64::consts::{PI, TAU};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, graphene, gsk, pango};

use piddle_core::encode::{Rgb, cpu_color, fmt_bytes, fmt_cpu, mem_radius};
use piddle_core::sampler::{ProcInfo, Snapshot};
use piddle_core::service::ServiceKind;

/// How urgently the scene needs redrawing after a simulation step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Motion {
    /// Nothing visible is changing; stop the frame clock.
    Idle,
    /// Only gentle color easing or sub-pixel drift; a reduced frame rate is enough.
    Subtle,
    /// Bubbles are moving, growing, fading or the camera is zooming.
    Active,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Free,
    App,
    Service,
    Parent,
    Tree,
}

impl Layout {
    /// Layouts that pull bubbles into clusters with a shared hull.
    fn grouped(self) -> bool {
        matches!(self, Layout::App | Layout::Service | Layout::Parent)
    }
}

/// Hull tint per service kind, chosen away from the CPU color scale.
fn kind_tint(kind: Option<ServiceKind>) -> (f64, f64, f64) {
    match kind {
        Some(ServiceKind::User) => (0.45, 0.85, 0.55),
        Some(ServiceKind::Scope) => (0.75, 0.58, 0.98),
        Some(ServiceKind::Container) => (0.95, 0.5, 0.8),
        Some(ServiceKind::Kernel | ServiceKind::Other) => (0.6, 0.62, 0.66),
        Some(ServiceKind::System) | None => (0.6, 0.7, 0.9),
    }
}

/// A process counts as "active" if it used at least this much CPU recently.
const ACTIVE_CPU: f32 = 0.5;
const ACTIVE_HOLD: Duration = Duration::from_secs(4);

pub struct Bubble {
    pub info: ProcInfo,
    x: f64,
    y: f64,
    vx: f64,
    vy: f64,
    r: f64,
    target_r: f64,
    /// Displayed (eased) CPU percentage, drives color.
    cpu: f64,
    alpha: f64,
    dying: bool,
    matched: bool,
    generation: u64,
    group: usize,
    parent: Option<usize>,
    /// Number of visible children (Tree layout only).
    kids: usize,
    /// Radial-tree target position (Tree layout only).
    tx: f64,
    ty: f64,
    label: Option<LabelCache>,
}

struct LabelCache {
    key: (i32, i32),
    name: pango::Layout,
    pid: pango::Layout,
}

struct Group {
    key: Arc<str>,
    label: String,
    kind: Option<ServiceKind>,
    x: f64,
    y: f64,
    vx: f64,
    vy: f64,
    r: f64,
    /// Drawn outline: centered on the members' centroid.
    cx: f64,
    cy: f64,
    hull: f64,
    members: usize,
    cpu: f64,
    mem: u64,
}

#[derive(Clone, Copy, Default)]
pub struct Meta {
    pub ncpu: usize,
    pub clk_tck: u64,
    pub boot_time: u64,
    pub total_cpu: f32,
    pub mem_total: u64,
    pub mem_used: u64,
    pub sample_cost: Duration,
    pub self_cpu: f32,
    pub procs: usize,
    pub threads: u64,
}

pub struct View {
    pub bubbles: Vec<Bubble>,
    index: HashMap<i32, usize>,
    groups: Vec<Group>,
    activity: HashMap<i32, Instant>,
    names: HashMap<i32, Arc<str>>,
    pub layout: Layout,
    query: String,
    pub show_kernel: bool,
    pub active_only: bool,
    pub hover: Option<i32>,
    pub mouse: (f64, f64),
    pub size: (f64, f64),
    cam: (f64, f64, f64),
    cam_target: (f64, f64, f64),
    cam_init: bool,
    generation: u64,
    pub meta: Meta,
    last_procs: Vec<ProcInfo>,
    order: Vec<usize>,
    prev: Vec<(f64, f64)>,
    pub matches: usize,
    pub top: Vec<(Arc<str>, i32, f32)>,
}

impl View {
    pub fn new() -> Self {
        Self {
            bubbles: Vec::new(),
            index: HashMap::new(),
            groups: Vec::new(),
            activity: HashMap::new(),
            names: HashMap::new(),
            layout: Layout::Free,
            query: String::new(),
            show_kernel: false,
            active_only: false,
            hover: None,
            mouse: (0.0, 0.0),
            size: (800.0, 600.0),
            cam: (0.0, 0.0, 1.0),
            cam_target: (0.0, 0.0, 1.0),
            cam_init: false,
            generation: 0,
            meta: Meta::default(),
            last_procs: Vec::new(),
            order: Vec::new(),
            prev: Vec::new(),
            matches: 0,
            top: Vec::new(),
        }
    }

    /// Current world→screen scale, used by the legend to draw true-size references.
    pub fn scale(&self) -> f64 {
        self.cam.2
    }

    pub fn bubble(&self, pid: i32) -> Option<&Bubble> {
        self.index.get(&pid).map(|&i| &self.bubbles[i])
    }

    /// All processes from the latest sample, unfiltered.
    pub fn procs(&self) -> &[ProcInfo] {
        &self.last_procs
    }

    pub fn proc_info(&self, pid: i32) -> Option<&ProcInfo> {
        self.last_procs.iter().find(|p| p.pid == pid)
    }

    pub fn name_of(&self, pid: i32) -> Option<Arc<str>> {
        self.names.get(&pid).cloned()
    }

    pub fn apply(&mut self, snap: Snapshot) {
        let now = Instant::now();
        self.meta = Meta {
            ncpu: snap.ncpu,
            clk_tck: snap.clk_tck,
            boot_time: snap.boot_time,
            total_cpu: snap.total_cpu,
            mem_total: snap.mem_total,
            mem_used: snap.mem_used,
            sample_cost: snap.sample_cost,
            self_cpu: snap.procs.iter().find(|p| p.pid == snap.self_pid).map(|p| p.cpu).unwrap_or(0.0),
            procs: snap.procs.len(),
            threads: snap.procs.iter().map(|p| p.threads as u64).sum(),
        };
        self.names = snap.procs.iter().map(|p| (p.pid, p.name.clone())).collect();
        for p in &snap.procs {
            if p.cpu >= ACTIVE_CPU {
                self.activity.insert(p.pid, now);
            }
        }
        self.activity.retain(|_, t| now.duration_since(*t) < ACTIVE_HOLD);

        let mut top: Vec<_> = snap.procs.iter().map(|p| (p.name.clone(), p.pid, p.cpu)).collect();
        top.sort_by(|a, b| b.2.total_cmp(&a.2));
        top.truncate(6);
        self.top = top;

        self.last_procs = snap.procs;
        self.sync_bubbles();
    }

    /// Reconcile bubbles with the latest sample and current filters.
    fn sync_bubbles(&mut self) {
        self.generation += 1;
        let generation = self.generation;
        let startup = self.bubbles.is_empty();
        let procs = std::mem::take(&mut self.last_procs);
        let mut fresh = Vec::new();

        for p in &procs {
            if p.kthread && !self.show_kernel {
                continue;
            }
            if self.active_only && !self.activity.contains_key(&p.pid) {
                continue;
            }
            match self.index.get(&p.pid) {
                Some(&i) if self.bubbles[i].info.start_ticks == p.start_ticks => {
                    let b = &mut self.bubbles[i];
                    b.info = p.clone();
                    // Hysteresis: ignore small RSS wobble so the layout can rest.
                    let r = mem_radius(p.rss);
                    if (r - b.target_r).abs() > b.target_r * 0.02 {
                        b.target_r = r;
                    }
                    b.generation = generation;
                    b.dying = false;
                }
                other => {
                    if let Some(&i) = other {
                        self.bubbles[i].dying = true; // PID reused
                    }
                    fresh.push(p.clone());
                }
            }
        }
        self.last_procs = procs;

        for b in &mut self.bubbles {
            if b.generation != generation {
                b.dying = true;
            }
        }
        // Place new bubbles after parents are known so children spawn near them.
        let mut fresh_idx = Vec::with_capacity(fresh.len());
        for p in fresh {
            let r = mem_radius(p.rss);
            self.bubbles.push(Bubble {
                cpu: p.cpu as f64,
                info: p,
                x: 0.0,
                y: 0.0,
                vx: 0.0,
                vy: 0.0,
                r: 0.0,
                target_r: r,
                alpha: 0.0,
                dying: false,
                matched: false,
                generation,
                group: 0,
                parent: None,
                kids: 0,
                tx: 0.0,
                ty: 0.0,
                label: None,
            });
            fresh_idx.push(self.bubbles.len() - 1);
        }
        self.reindex();
        self.rebuild_groups();
        self.layout_tree();
        if startup {
            self.initial_placement();
        } else {
            for i in fresh_idx {
                self.place(i);
            }
        }
        self.refresh_matches();
    }

    fn reindex(&mut self) {
        self.index.clear();
        for (i, b) in self.bubbles.iter().enumerate() {
            if !b.dying {
                self.index.insert(b.info.pid, i);
            }
        }
        for i in 0..self.bubbles.len() {
            let pp = self.bubbles[i].info.ppid;
            self.bubbles[i].parent = self.index.get(&pp).copied().filter(|&p| p != i);
        }
    }

    fn group_key(&self, b: &Bubble) -> (Arc<str>, String, Option<ServiceKind>) {
        match self.layout {
            Layout::App => (b.info.app.clone(), b.info.app.to_string(), None),
            Layout::Service => {
                let s = &b.info.service;
                (s.key.clone(), s.label.to_string(), Some(s.kind))
            }
            Layout::Parent => {
                let pp = b.info.ppid;
                let label = match self.names.get(&pp) {
                    Some(n) => format!("{n} · {pp}"),
                    None if pp == 0 => "kernel".to_string(),
                    None => format!("PID {pp}"),
                };
                (Arc::from(pp.to_string()), label, None)
            }
            Layout::Free | Layout::Tree => (Arc::from(""), String::new(), None),
        }
    }

    fn rebuild_groups(&mut self) {
        let old: HashMap<Arc<str>, Group> = self.groups.drain(..).map(|g| (g.key.clone(), g)).collect();
        let mut old = old;
        let mut map: HashMap<Arc<str>, usize> = HashMap::new();
        let mut groups: Vec<Group> = Vec::new();
        let mut fresh_groups = Vec::new();
        for i in 0..self.bubbles.len() {
            let (key, label, kind) = self.group_key(&self.bubbles[i]);
            let gi = *map.entry(key.clone()).or_insert_with(|| {
                let g = match old.remove(&key) {
                    Some(mut g) => {
                        g.label = label;
                        g.kind = kind;
                        g
                    }
                    None => {
                        fresh_groups.push(groups.len());
                        Group {
                            key,
                            label,
                            kind,
                            x: 0.0,
                            y: 0.0,
                            vx: 0.0,
                            vy: 0.0,
                            r: 0.0,
                            cx: 0.0,
                            cy: 0.0,
                            hull: 0.0,
                            members: 0,
                            cpu: 0.0,
                            mem: 0,
                        }
                    }
                };
                groups.push(g);
                groups.len() - 1
            });
            self.bubbles[i].group = gi;
        }
        // New groups start at the centroid of their (already placed) members.
        for gi in fresh_groups {
            let (mut sx, mut sy, mut n) = (0.0f64, 0.0f64, 0.0f64);
            for b in self.bubbles.iter().filter(|b| b.group == gi && b.r > 0.0) {
                sx += b.x;
                sy += b.y;
                n += 1.0;
            }
            if n > 0.0 {
                groups[gi].x = sx / n;
                groups[gi].y = sy / n;
            } else {
                let (x, y) = spiral(gi, 60.0);
                groups[gi].x = x;
                groups[gi].y = y;
            }
        }
        self.groups = groups;
    }

    /// Lay out the very first sample on a sunflower spiral, sorted so that
    /// related processes start next to each other.
    fn initial_placement(&mut self) {
        let mut order: Vec<usize> = (0..self.bubbles.len()).collect();
        order.sort_by(|&a, &b| {
            let (pa, pb) = (&self.bubbles[a].info, &self.bubbles[b].info);
            pa.app.cmp(&pb.app).then(pa.pid.cmp(&pb.pid))
        });
        let spacing = 2.0 * self.bubbles.iter().map(|b| b.target_r).sum::<f64>() / self.bubbles.len().max(1) as f64;
        for (k, &i) in order.iter().enumerate() {
            let (x, y) = spiral(k, spacing * 0.75);
            let b = &mut self.bubbles[i];
            b.x = x * 1.4;
            b.y = y;
            b.r = b.target_r * 0.5;
        }
        for g in 0..self.groups.len() {
            let (mut sx, mut sy, mut n) = (0.0f64, 0.0f64, 0.0f64);
            for b in self.bubbles.iter().filter(|b| b.group == g) {
                sx += b.x;
                sy += b.y;
                n += 1.0;
            }
            self.groups[g].x = sx / n.max(1.0);
            self.groups[g].y = sy / n.max(1.0);
        }
    }

    /// Find a free spot for a new bubble near where it logically belongs.
    fn place(&mut self, i: usize) {
        let b = &self.bubbles[i];
        let (ax, ay) = match (self.layout, b.parent) {
            (Layout::Tree | Layout::Parent, Some(p)) => (self.bubbles[p].x, self.bubbles[p].y),
            (l, _) if l.grouped() => (self.groups[b.group].x, self.groups[b.group].y),
            (_, Some(p)) => (self.bubbles[p].x, self.bubbles[p].y),
            _ => (0.0, 0.0),
        };
        let r = b.target_r;
        let step = (r * 0.8).max(6.0);
        let mut best = (ax, ay);
        'search: for k in 0..120 {
            let ang = k as f64 * 2.399_963;
            let d = (k as f64).sqrt() * step;
            let (x, y) = (ax + ang.cos() * d, ay + ang.sin() * d);
            for (j, o) in self.bubbles.iter().enumerate() {
                if j == i || o.dying || o.r <= 0.0 {
                    continue;
                }
                let rr = o.r + r + 3.0;
                if (o.x - x).powi(2) + (o.y - y).powi(2) < rr * rr {
                    continue 'search;
                }
            }
            best = (x, y);
            break;
        }
        let b = &mut self.bubbles[i];
        b.x = best.0;
        b.y = best.1;
    }

    pub fn set_query(&mut self, q: &str) {
        self.query = q.trim().to_lowercase();
        self.refresh_matches();
    }

    pub fn has_query(&self) -> bool {
        !self.query.is_empty()
    }

    fn refresh_matches(&mut self) {
        let q = &self.query;
        let numeric = !q.is_empty() && q.bytes().all(|c| c.is_ascii_digit());
        let mut n = 0;
        for b in &mut self.bubbles {
            b.matched = !q.is_empty()
                && ((numeric && b.info.pid.to_string().starts_with(q.as_str()))
                    || b.info.search_text.contains(q.as_str())
                    || b.info.service.search.contains(q.as_str()));
            if b.matched && !b.dying {
                n += 1;
            }
        }
        self.matches = n;
    }

    pub fn set_layout(&mut self, l: Layout) {
        self.layout = l;
        self.rebuild_groups();
        self.layout_tree();
        for b in &mut self.bubbles {
            b.vx = 0.0;
            b.vy = 0.0;
        }
    }

    /// Re-apply filters (kernel threads / active-only) against the last sample.
    pub fn refilter(&mut self) {
        self.sync_bubbles();
    }

    // ---------------------------------------------------------------- simulation

    /// Advance animation by `dt` seconds and report how much is still changing.
    pub fn step(&mut self, dt: f64) -> Motion {
        let dt = dt.clamp(1.0 / 240.0, 0.1);
        let k = 1.0 - (-dt / 0.15).exp();
        let scale = self.cam.2;
        let mut subtle = false;
        let mut active = false;

        for b in &mut self.bubbles {
            let ta = if b.dying { 0.0 } else { 1.0 };
            let tr = if b.dying { b.target_r * 0.6 } else { b.target_r };
            let dr = tr - b.r;
            let dc = b.info.cpu as f64 - b.cpu;
            let da = ta - b.alpha;
            b.r += dr * k;
            b.cpu += dc * k;
            if dr.abs() * scale <= 0.2 {
                b.r = tr;
            }
            if dc.abs() <= 1.0 {
                b.cpu = b.info.cpu as f64;
            }
            b.alpha += da * (k * 1.4).min(1.0);
            // Stop animating once remaining changes are imperceptible.
            if dr.abs() * scale > 0.2 || da.abs() > 0.01 {
                active = true;
            } else if dc.abs() > 1.0 {
                subtle = true;
            }
        }

        if self.bubbles.iter().any(|b| b.dying && b.alpha < 0.02) {
            self.bubbles.retain(|b| !(b.dying && b.alpha < 0.02));
            self.reindex();
            self.rebuild_groups();
            self.layout_tree();
            if let Some(h) = self.hover
                && !self.index.contains_key(&h)
            {
                self.hover = None;
            }
        }

        let steps = ((dt * 60.0).round() as usize).clamp(1, 3);
        let mut moving = 0.0f64;
        for _ in 0..steps {
            moving = moving.max(self.physics());
        }
        // Measured in screen pixels so the scene rests regardless of zoom.
        let moved = moving * self.cam.2;
        active |= moved > 0.6;
        subtle |= moved > 0.15;
        active |= self.update_camera(dt);
        if active {
            Motion::Active
        } else if subtle {
            Motion::Subtle
        } else {
            Motion::Idle
        }
    }

    fn physics(&mut self) -> f64 {
        let (w, h) = self.size;
        let sa = (w / h.max(1.0)).clamp(0.5, 3.0).sqrt();
        let n = self.bubbles.len();
        let mut acc = vec![(0.0f64, 0.0f64); n];

        match self.layout {
            Layout::Free => {
                let g = 0.004;
                for (i, b) in self.bubbles.iter().enumerate() {
                    acc[i] = (-b.x * g / sa, -b.y * g * sa);
                }
            }
            Layout::App | Layout::Service | Layout::Parent => {
                self.group_physics(sa);
                for (i, b) in self.bubbles.iter().enumerate() {
                    let g = &self.groups[b.group];
                    acc[i] = ((g.x - b.x) * 0.03, (g.y - b.y) * 0.03);
                }
            }
            Layout::Tree => {
                for (i, b) in self.bubbles.iter().enumerate() {
                    acc[i] = ((b.tx - b.x) * 0.04, (b.ty - b.y) * 0.04);
                }
            }
        }

        // Position-based dynamics: integrate, resolve collisions, then derive
        // velocity from the actual displacement so pushes don't build up energy.
        let mut prev = std::mem::take(&mut self.prev);
        prev.clear();
        prev.extend(self.bubbles.iter().map(|b| (b.x, b.y)));
        for (b, a) in self.bubbles.iter_mut().zip(&acc) {
            b.vx += a.0;
            b.vy += a.1;
            let v = (b.vx * b.vx + b.vy * b.vy).sqrt();
            if v > 25.0 {
                b.vx *= 25.0 / v;
                b.vy *= 25.0 / v;
            }
            b.x += b.vx;
            b.y += b.vy;
        }
        self.collide();
        let mut max_d = 0.0f64;
        for (b, &(px, py)) in self.bubbles.iter_mut().zip(&prev) {
            let (dx, dy) = (b.x - px, b.y - py);
            b.vx = dx * 0.72;
            b.vy = dy * 0.72;
            if !b.dying {
                max_d = max_d.max(dx.abs().max(dy.abs()));
            }
        }
        self.prev = prev;
        max_d
    }

    fn group_physics(&mut self, sa: f64) {
        for g in &mut self.groups {
            g.members = 0;
            g.r = 0.0;
            g.cpu = 0.0;
            g.mem = 0;
            g.cx = 0.0;
            g.cy = 0.0;
            g.hull = 0.0;
        }
        for b in self.bubbles.iter().filter(|b| !b.dying) {
            let g = &mut self.groups[b.group];
            g.members += 1;
            g.r += (b.r + 3.0).powi(2);
            g.cpu += b.cpu;
            g.mem += b.info.rss;
            g.cx += b.x;
            g.cy += b.y;
        }
        for g in &mut self.groups {
            if g.members > 0 {
                g.cx /= g.members as f64;
                g.cy /= g.members as f64;
            }
        }
        for b in self.bubbles.iter().filter(|b| !b.dying) {
            let g = &mut self.groups[b.group];
            let d = ((b.x - g.cx).powi(2) + (b.y - g.cy).powi(2)).sqrt() + b.r;
            g.hull = g.hull.max(d);
        }

        // Groups are themselves circles packed with position-based dynamics.
        let prev: Vec<(f64, f64)> = self.groups.iter().map(|g| (g.x, g.y)).collect();
        for g in &mut self.groups {
            g.hull += 7.0;
            g.r = (g.r.sqrt() * 1.15 + 8.0).max(g.hull);
            g.vx -= g.x * 0.004 / sa;
            g.vy -= g.y * 0.004 * sa;
            g.x += g.vx;
            g.y += g.vy;
        }
        let m = self.groups.len();
        for _ in 0..2 {
            for i in 0..m {
                if self.groups[i].members == 0 {
                    continue;
                }
                for j in i + 1..m {
                    if self.groups[j].members == 0 {
                        continue;
                    }
                    let (a, b) = (&self.groups[i], &self.groups[j]);
                    let (dx, dy) = (b.x - a.x, b.y - a.y);
                    let min = a.r + b.r + 16.0;
                    if dx.abs() >= min || dy.abs() >= min {
                        continue;
                    }
                    let d = (dx * dx + dy * dy).sqrt();
                    if d < min {
                        let (nx, ny) = if d > 1e-6 { (dx / d, dy / d) } else { unit(i * 31 + j) };
                        let (ma, mb) = (a.r * a.r, b.r * b.r);
                        let push = (min - d) * 0.8;
                        let wa = mb / (ma + mb);
                        let wb = ma / (ma + mb);
                        self.groups[i].x -= nx * push * wa;
                        self.groups[i].y -= ny * push * wa;
                        self.groups[j].x += nx * push * wb;
                        self.groups[j].y += ny * push * wb;
                    }
                }
            }
        }
        for (g, (px, py)) in self.groups.iter_mut().zip(prev) {
            g.vx = (g.x - px) * 0.72;
            g.vy = (g.y - py) * 0.72;
        }
    }

    /// Radial tree: the root sits in the middle, each generation on its own
    /// ring, and every subtree owns an angular wedge proportional to its size.
    /// Children are ordered by PID so the layout stays put between samples.
    fn layout_tree(&mut self) {
        if self.layout != Layout::Tree {
            return;
        }
        const GAP: f64 = 6.0;
        let n = self.bubbles.len();
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut roots = Vec::new();
        for i in 0..n {
            if self.bubbles[i].dying {
                continue;
            }
            match self.bubbles[i].parent.filter(|&p| !self.bubbles[p].dying) {
                Some(p) => children[p].push(i),
                None => roots.push(i),
            }
        }
        let pid = |i: &usize| self.bubbles[*i].info.pid;
        for c in &mut children {
            c.sort_by_key(pid);
        }
        roots.sort_by_key(pid);
        for (b, c) in self.bubbles.iter_mut().zip(&children) {
            b.kids = c.len();
        }

        // Post-order weights and depths (iterative: process chains can be deep).
        let single_root = roots.len() == 1;
        let mut depth = vec![0usize; n];
        let mut order = Vec::with_capacity(n);
        let mut stack: Vec<(usize, usize)> = roots.iter().map(|&r| (r, if single_root { 0 } else { 1 })).collect();
        while let Some((i, d)) = stack.pop() {
            depth[i] = d;
            order.push(i);
            stack.extend(children[i].iter().map(|&c| (c, d + 1)));
        }
        let mut weight = vec![0.0f64; n];
        for &i in order.iter().rev() {
            let own = self.bubbles[i].target_r * 2.0 + GAP;
            let kids: f64 = children[i].iter().map(|&c| weight[c]).sum();
            weight[i] = own.max(kids);
        }

        // Ring radii: far enough from the previous ring, and long enough to
        // hold every bubble of that generation side by side.
        let max_depth = order.iter().map(|&i| depth[i]).max().unwrap_or(0);
        let mut ring_need = vec![0.0f64; max_depth + 1];
        let mut ring_maxr = vec![0.0f64; max_depth + 1];
        for &i in &order {
            let r = self.bubbles[i].target_r;
            ring_need[depth[i]] += r * 2.0 + GAP;
            ring_maxr[depth[i]] = ring_maxr[depth[i]].max(r);
        }
        let mut ring = vec![0.0f64; max_depth + 1];
        for d in 1..=max_depth {
            let base = if d == 1 && !single_root { 0.0 } else { ring[d - 1] + ring_maxr[d - 1] };
            ring[d] = (base + ring_maxr[d] + 34.0).max(ring_need[d] / TAU * 1.1);
        }

        // Assign wedges top-down.
        let total: f64 = roots.iter().map(|&r| weight[r]).sum::<f64>().max(1.0);
        let mut stack: Vec<(usize, f64, f64)> = Vec::new();
        let mut a = -PI / 2.0;
        for &r in &roots {
            let span = TAU * weight[r] / total;
            stack.push((r, a, span));
            a += span;
        }
        while let Some((i, a0, span)) = stack.pop() {
            let ang = a0 + span / 2.0;
            let rad = ring[depth[i]];
            self.bubbles[i].tx = rad * ang.cos();
            self.bubbles[i].ty = rad * ang.sin();
            let kids: f64 = children[i].iter().map(|&c| weight[c]).sum::<f64>().max(1e-6);
            let mut a = a0;
            for &c in &children[i] {
                let s = span * weight[c] / kids;
                stack.push((c, a, s));
                a += s;
            }
        }
    }

    /// Sort-and-sweep collision resolution (position based, no added velocity).
    fn collide(&mut self) -> f64 {
        let mut max_push = 0.0f64;
        let grouped = self.layout.grouped();
        let max_pad = 3.0;
        let mut order = std::mem::take(&mut self.order);
        order.clear();
        order.extend((0..self.bubbles.len()).filter(|&i| !self.bubbles[i].dying));
        for _ in 0..4 {
            let bs = &mut self.bubbles;
            order.sort_unstable_by(|&a, &b| (bs[a].x - bs[a].r).total_cmp(&(bs[b].x - bs[b].r)));
            for ii in 0..order.len() {
                let i = order[ii];
                for &j in &order[ii + 1..] {
                    let (xi, yi, ri, gi) = (bs[i].x, bs[i].y, bs[i].r, bs[i].group);
                    let (xj, yj, rj) = (bs[j].x, bs[j].y, bs[j].r);
                    if xj - rj > xi + ri + max_pad {
                        break;
                    }
                    // Across groups, packing is handled at the group level so
                    // members can travel through other clusters to their own.
                    if grouped && bs[j].group != gi {
                        continue;
                    }
                    let pad = 3.0;
                    let min = ri + rj + pad;
                    let (dx, dy) = (xj - xi, yj - yi);
                    if dy.abs() >= min {
                        continue;
                    }
                    let d2 = dx * dx + dy * dy;
                    if d2 >= min * min {
                        continue;
                    }
                    let d = d2.sqrt();
                    let (nx, ny) = if d > 1e-6 { (dx / d, dy / d) } else { unit(i * 7919 + j) };
                    let (mi, mj) = (ri * ri + 1.0, rj * rj + 1.0);
                    let push = (min - d) * 0.8;
                    max_push = max_push.max(push);
                    let wi = mj / (mi + mj);
                    let wj = mi / (mi + mj);
                    bs[i].x -= nx * push * wi;
                    bs[i].y -= ny * push * wi;
                    bs[j].x += nx * push * wj;
                    bs[j].y += ny * push * wj;
                }
            }
        }
        self.order = order;
        max_push
    }

    fn update_camera(&mut self, dt: f64) -> bool {
        let (w, h) = self.size;
        let mut bb = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for b in self.bubbles.iter().filter(|b| !b.dying) {
            bb.0 = bb.0.min(b.x - b.target_r);
            bb.1 = bb.1.min(b.y - b.target_r);
            bb.2 = bb.2.max(b.x + b.target_r);
            bb.3 = bb.3.max(b.y + b.target_r);
        }
        if bb.0 > bb.2 {
            return false;
        }
        let margin = 36.0;
        let bw = (bb.2 - bb.0).max(50.0);
        let bh = (bb.3 - bb.1).max(50.0);
        let s = ((w - 2.0 * margin) / bw).min((h - 2.0 * margin) / bh).clamp(0.05, 2.5);
        let t = ((bb.0 + bb.2) / 2.0, (bb.1 + bb.3) / 2.0, s);
        // Dead band: ignore tiny bbox wobble so the view can come to rest.
        let ct = self.cam_target;
        if !self.cam_init
            || (t.2 / ct.2 - 1.0).abs() > 0.03
            || (t.0 - ct.0).abs() * ct.2 > 12.0
            || (t.1 - ct.1).abs() * ct.2 > 12.0
            || t.2 < ct.2
        {
            self.cam_target = t;
        }
        if !self.cam_init {
            self.cam = self.cam_target;
            self.cam_init = true;
            return true;
        }
        let k = 1.0 - (-dt / 0.45).exp();
        let c = &mut self.cam;
        let tg = self.cam_target;
        c.0 += (tg.0 - c.0) * k;
        c.1 += (tg.1 - c.1) * k;
        c.2 += (tg.2 - c.2) * k;
        (tg.2 / c.2 - 1.0).abs() > 0.001 || (tg.0 - c.0).abs() * c.2 > 0.3 || (tg.1 - c.1).abs() * c.2 > 0.3
    }

    /// Screen position of a live bubble's center.
    pub fn screen_pos(&self, pid: i32) -> Option<(f64, f64)> {
        self.bubble(pid).map(|b| self.to_screen(b.x, b.y))
    }

    fn to_screen(&self, x: f64, y: f64) -> (f64, f64) {
        let (w, h) = self.size;
        ((x - self.cam.0) * self.cam.2 + w / 2.0, (y - self.cam.1) * self.cam.2 + h / 2.0)
    }

    pub fn pick(&self, sx: f64, sy: f64) -> Option<i32> {
        let (w, h) = self.size;
        let s = self.cam.2;
        let (x, y) = ((sx - w / 2.0) / s + self.cam.0, (sy - h / 2.0) / s + self.cam.1);
        let slop = 2.0 / s;
        self.bubbles
            .iter()
            .filter(|b| !b.dying)
            .find(|b| (b.x - x).powi(2) + (b.y - y).powi(2) <= (b.r + slop).powi(2))
            .map(|b| b.info.pid)
    }

    // ------------------------------------------------------------------ drawing
    //
    // Rendering goes through GtkSnapshot render nodes so gradients, clips,
    // strokes and (glyph-cached) text are drawn by the GPU renderer.

    pub fn render(&mut self, snap: &gtk::Snapshot, pctx: &pango::Context) {
        let (w, h) = self.size;
        let full = rect(0.0, 0.0, w, h);
        snap.append_radial_gradient(
            &full,
            &point(w / 2.0, h / 2.0),
            (w * 0.7) as f32,
            (h * 0.9) as f32,
            0.0,
            1.0,
            &[gsk::ColorStop::new(0.0, rgb(0.085, 0.10, 0.135)), gsk::ColorStop::new(1.0, rgb(0.045, 0.05, 0.068))],
        );

        if self.bubbles.is_empty() {
            let l = layout(pctx, "Sampling /proc…", 15.0, false);
            let (lw, lh) = l.pixel_size();
            text(snap, &l, (w - lw as f64) / 2.0, (h - lh as f64) / 2.0, rgba(1.0, 1.0, 1.0, 0.5));
            return;
        }

        let s = self.cam.2;
        let dimming = self.has_query();

        // Group hulls.
        if self.layout.grouped() {
            // One batched path per tint (service kinds differ; other layouts use one).
            let mut paths: Vec<(Option<ServiceKind>, gsk::PathBuilder)> = Vec::new();
            for g in self.groups.iter().filter(|g| g.members >= 2) {
                let (gx, gy) = self.to_screen(g.cx, g.cy);
                let pb = match paths.iter().position(|(k, _)| *k == g.kind) {
                    Some(i) => &paths[i].1,
                    None => {
                        paths.push((g.kind, gsk::PathBuilder::new()));
                        &paths.last().unwrap().1
                    }
                };
                pb.add_circle(&point(gx, gy), (g.hull * s) as f32);
            }
            let alpha = if self.layout == Layout::Service { 0.32 } else { 0.18 };
            for (kind, pb) in paths {
                let path = pb.to_path();
                let (r, g, b) = kind_tint(kind);
                snap.append_fill(&path, gsk::FillRule::Winding, &rgba(r, g, b, 0.03));
                let stroke = gsk::Stroke::new(1.0);
                stroke.set_dash(&[4.0, 4.0]);
                snap.append_stroke(&path, &stroke, &rgba(r, g, b, alpha));
            }
        }

        // Tree links, batched into two strokes (normal and dimmed).
        if self.layout == Layout::Tree {
            let (on, off) = (gsk::PathBuilder::new(), gsk::PathBuilder::new());
            for b in &self.bubbles {
                let Some(p) = b.parent else { continue };
                let pb = &self.bubbles[p];
                let (x0, y0) = self.to_screen(pb.x, pb.y);
                let (x1, y1) = self.to_screen(b.x, b.y);
                let faint = b.dying || pb.dying || (dimming && !(b.matched || pb.matched));
                let builder = if faint { &off } else { &on };
                builder.move_to(x0 as f32, y0 as f32);
                builder.line_to(x1 as f32, y1 as f32);
            }
            let stroke = gsk::Stroke::new(1.5);
            snap.append_stroke(&off.to_path(), &stroke, &rgba(0.55, 0.65, 0.85, 0.12));
            snap.append_stroke(&on.to_path(), &stroke, &rgba(0.55, 0.65, 0.85, 0.38));
        }

        // Bubbles.
        let hover = self.hover;
        for i in 0..self.bubbles.len() {
            let (sx, sy) = {
                let b = &self.bubbles[i];
                self.to_screen(b.x, b.y)
            };
            let b = &mut self.bubbles[i];
            let sr = b.r * s;
            if sr < 0.5 || sx + sr < 0.0 || sy + sr < 0.0 || sx - sr > w || sy - sr > h {
                continue;
            }
            let dim = if dimming && !b.matched { 0.16 } else { 1.0 };
            let a = b.alpha * dim;
            let c = cpu_color(b.cpu);

            if b.cpu > 35.0 && sr > 3.0 {
                let t = ((b.cpu - 35.0) / 65.0).clamp(0.0, 1.0);
                let gr = sr * 1.45;
                snap.append_radial_gradient(
                    &rect(sx - gr, sy - gr, gr * 2.0, gr * 2.0),
                    &point(sx, sy),
                    gr as f32,
                    gr as f32,
                    0.0,
                    1.0,
                    &[
                        gsk::ColorStop::new(0.0, rgba(c.0, c.1, c.2, 0.0)),
                        gsk::ColorStop::new(0.55, rgba(c.0, c.1, c.2, 0.45 * t * a)),
                        gsk::ColorStop::new(1.0, rgba(c.0, c.1, c.2, 0.0)),
                    ],
                );
            }

            let hi = mix(c, Rgb(1.0, 1.0, 1.0), 0.35);
            let lo = mix(c, Rgb(0.0, 0.0, 0.0), 0.25);
            let bounds = rect(sx - sr, sy - sr, sr * 2.0, sr * 2.0);
            let round = gsk::RoundedRect::from_rect(bounds, sr as f32);
            if sr < 4.0 {
                snap.push_rounded_clip(&round);
                snap.append_color(&rgba(c.0, c.1, c.2, 0.95 * a), &bounds);
                snap.pop();
            } else {
                snap.push_rounded_clip(&round);
                snap.append_radial_gradient(
                    &bounds,
                    &point(sx - sr * 0.35, sy - sr * 0.4),
                    (sr * 1.35) as f32,
                    (sr * 1.35) as f32,
                    0.0,
                    1.0,
                    &[
                        gsk::ColorStop::new(0.0, rgba(hi.0, hi.1, hi.2, 0.96 * a)),
                        gsk::ColorStop::new(1.0, rgba(lo.0, lo.1, lo.2, 0.92 * a)),
                    ],
                );
                snap.pop();
                ring(snap, sx, sy, sr, 1.0, rgba(hi.0, hi.1, hi.2, 0.5 * a));
            }

            if dimming && b.matched {
                ring(snap, sx, sy, sr + 4.5, 2.0, rgba(1.0, 1.0, 1.0, 0.9 * b.alpha));
            }
            if hover == Some(b.info.pid) && !b.dying {
                ring(snap, sx, sy, sr + 2.5, 2.5, rgba(1.0, 1.0, 1.0, 0.95));
            }

            if sr >= 15.0 {
                draw_label(snap, pctx, b, sx, sy, sr, c, a);
            }
        }

        if self.layout.grouped() {
            self.draw_group_labels(snap, pctx);
        }
        if self.layout == Layout::Tree {
            self.draw_hub_labels(snap, pctx);
        }
        self.draw_hover_card(snap, pctx);
    }

    /// Tree hubs are often small (e.g. systemd) but are where the structure
    /// meets, so tag every parent that is too small for an inside label.
    fn draw_hub_labels(&self, snap: &gtk::Snapshot, pctx: &pango::Context) {
        let s = self.cam.2;
        let dimming = self.has_query();
        for b in self.bubbles.iter().filter(|b| b.kids > 0 && !b.dying && b.r * s < 15.0) {
            let (sx, sy) = self.to_screen(b.x, b.y);
            let markup = format!(
                "<b>{}</b> <span alpha='60%'>{} · {} child{}</span>",
                glib_escape(&b.info.name),
                b.info.pid,
                b.kids,
                if b.kids == 1 { "" } else { "ren" }
            );
            let l = pango::Layout::new(pctx);
            l.set_font_description(Some(&font(10.5, false)));
            l.set_markup(&markup);
            let (lw, lh) = l.pixel_size();
            let (lw, lh) = (lw as f64, lh as f64);
            let x = sx + b.r * s + 6.0;
            let y = sy - lh / 2.0;
            let a = if dimming && !b.matched { 0.3 } else { 1.0 };
            card(
                snap,
                x - 6.0,
                y - 2.0,
                lw + 12.0,
                lh + 4.0,
                (lh + 4.0) / 2.0,
                rgba(0.09, 0.105, 0.14, 0.88 * a),
                rgba(0.6, 0.7, 0.9, 0.3 * a),
            );
            text(snap, &l, x, y, rgba(0.86, 0.89, 0.95, 0.95 * a));
        }
    }

    fn draw_group_labels(&self, snap: &gtk::Snapshot, pctx: &pango::Context) {
        let s = self.cam.2;
        for g in self.groups.iter().filter(|g| g.members >= 2) {
            let (gx, gy) = self.to_screen(g.cx, g.cy);
            let gr = g.hull * s;
            if gr < 30.0 {
                continue;
            }
            let kind = g.kind.map(|k| format!("{} · ", k.label())).unwrap_or_default();
            let markup = format!(
                "<b>{}</b>  <span alpha='65%'>{kind}×{} · {} · {}</span>",
                glib_escape(&g.label),
                g.members,
                fmt_cpu(g.cpu),
                fmt_bytes(g.mem)
            );
            let l = pango::Layout::new(pctx);
            l.set_font_description(Some(&font(11.0, false)));
            l.set_markup(&markup);
            l.set_width((gr * 2.0 * pango::SCALE as f64) as i32);
            l.set_ellipsize(pango::EllipsizeMode::End);
            let (lw, lh) = l.pixel_size();
            let (lw, lh) = (lw as f64, lh as f64);
            let x = gx - lw / 2.0;
            let y = (gy - gr - lh / 2.0).max(4.0);
            card(snap, x - 8.0, y - 3.0, lw + 16.0, lh + 6.0, (lh + 6.0) / 2.0, rgba(0.09, 0.105, 0.14, 0.92), {
                let (r, gr, b) = kind_tint(g.kind);
                rgba(r, gr, b, if g.kind.is_some() { 0.5 } else { 0.25 })
            });
            text(snap, &l, x, y, rgba(0.86, 0.89, 0.95, 0.95));
        }
    }

    fn draw_hover_card(&self, snap: &gtk::Snapshot, pctx: &pango::Context) {
        let Some(b) = self.hover.and_then(|p| self.bubble(p)) else { return };
        let p = &b.info;
        let mem_pct = if self.meta.mem_total > 0 { p.rss as f64 / self.meta.mem_total as f64 * 100.0 } else { 0.0 };
        let markup = format!(
            "<span size='large' weight='bold'>{}</span>\n\
             <span alpha='65%'>PID {} · {}\n{} · {}</span>\n\n\
             CPU\t\t<b>{}</b> <span alpha='60%'>of one core</span>\n\
             Memory\t<b>{}</b> <span alpha='60%'>({:.1}% RAM)</span>\n\
             Threads\t<b>{}</b>\n\n\
             <span alpha='50%' size='small'>Click for details and actions</span>",
            glib_escape(&p.name),
            p.pid,
            glib_escape(&p.user),
            p.service.kind.label(),
            glib_escape(&p.service.label),
            fmt_cpu(b.cpu),
            fmt_bytes(p.rss),
            mem_pct,
            p.threads,
        );
        let l = pango::Layout::new(pctx);
        l.set_font_description(Some(&font(12.5, false)));
        l.set_markup(&markup);
        let (lw, lh) = l.pixel_size();
        let pad = 12.0;
        let (cw, ch) = (lw as f64 + pad * 2.0 + 6.0, lh as f64 + pad * 2.0);
        let (w, h) = self.size;
        let (mx, my) = self.mouse;
        let mut x = mx + 18.0;
        let mut y = my + 18.0;
        if x + cw > w - 6.0 {
            x = mx - cw - 14.0;
        }
        if y + ch > h - 6.0 {
            y = (h - ch - 6.0).max(6.0);
        }
        x = x.max(6.0);

        card(snap, x, y, cw, ch, 10.0, rgba(0.075, 0.085, 0.11, 0.96), rgba(1.0, 1.0, 1.0, 0.12));
        let c = cpu_color(b.cpu);
        card(snap, x + 6.0, y + pad, 3.0, ch - pad * 2.0, 1.5, rgb(c.0, c.1, c.2), rgb(c.0, c.1, c.2));
        text(snap, &l, x + pad + 6.0, y + pad, rgba(0.93, 0.95, 0.98, 1.0));
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_label(snap: &gtk::Snapshot, pctx: &pango::Context, b: &mut Bubble, sx: f64, sy: f64, sr: f64, c: Rgb, a: f64) {
    let px = (sr * 0.32).clamp(9.0, 15.0).round() as i32;
    let width = (sr * 1.65 / 4.0).round() as i32 * 4;
    let key = (px, width);
    if b.label.as_ref().map(|l| l.key) != Some(key) {
        let name = layout(pctx, &b.info.name, px as f64, true);
        name.set_width(width * pango::SCALE);
        name.set_ellipsize(pango::EllipsizeMode::End);
        name.set_alignment(pango::Alignment::Center);
        let pid = layout(pctx, &b.info.pid.to_string(), (px as f64 * 0.82).max(8.0), false);
        pid.set_width(width * pango::SCALE);
        pid.set_alignment(pango::Alignment::Center);
        b.label = Some(LabelCache { key, name, pid });
    }
    let lc = b.label.as_ref().unwrap();
    let (_, nh) = lc.name.pixel_size();
    let (_, ph) = lc.pid.pixel_size();
    let show_pid = sr >= 22.0;
    let total = nh as f64 + if show_pid { ph as f64 } else { 0.0 };
    let x = sx - width as f64 / 2.0;
    let y = sy - total / 2.0;
    // Dark text on bright (yellow) bubbles, light text with a soft shadow elsewhere.
    let dark = 0.299 * c.0 + 0.587 * c.1 + 0.114 * c.2 > 0.66;
    let lines: &[(&pango::Layout, f64, f64)] =
        if show_pid { &[(&lc.name, y, 1.0), (&lc.pid, y + nh as f64, 0.75)] } else { &[(&lc.name, y, 1.0)] };
    for &(l, yy, alpha) in lines {
        if dark {
            text(snap, l, x, yy, rgba(0.07, 0.07, 0.09, alpha * a));
        } else {
            text(snap, l, x, yy + 1.0, rgba(0.0, 0.0, 0.0, 0.45 * a));
            text(snap, l, x, yy, rgba(1.0, 1.0, 1.0, alpha * a));
        }
    }
}

fn layout(pctx: &pango::Context, s: &str, px: f64, bold: bool) -> pango::Layout {
    let l = pango::Layout::new(pctx);
    l.set_font_description(Some(&font(px, bold)));
    l.set_text(s);
    l
}

fn text(snap: &gtk::Snapshot, l: &pango::Layout, x: f64, y: f64, color: gdk::RGBA) {
    snap.save();
    snap.translate(&point(x.round(), y.round()));
    snap.append_layout(l, &color);
    snap.restore();
}

fn ring(snap: &gtk::Snapshot, cx: f64, cy: f64, r: f64, width: f64, color: gdk::RGBA) {
    let rr = gsk::RoundedRect::from_rect(rect(cx - r, cy - r, r * 2.0, r * 2.0), r as f32);
    snap.append_border(&rr, &[width as f32; 4], &[color; 4]);
}

#[allow(clippy::too_many_arguments)]
fn card(snap: &gtk::Snapshot, x: f64, y: f64, w: f64, h: f64, radius: f64, fill: gdk::RGBA, border: gdk::RGBA) {
    let b = rect(x, y, w, h);
    let rr = gsk::RoundedRect::from_rect(b, radius as f32);
    snap.push_rounded_clip(&rr);
    snap.append_color(&fill, &b);
    snap.pop();
    snap.append_border(&rr, &[1.0; 4], &[border; 4]);
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> graphene::Rect {
    graphene::Rect::new(x as f32, y as f32, w as f32, h as f32)
}

fn point(x: f64, y: f64) -> graphene::Point {
    graphene::Point::new(x as f32, y as f32)
}

fn rgba(r: f64, g: f64, b: f64, a: f64) -> gdk::RGBA {
    gdk::RGBA::new(r as f32, g as f32, b as f32, a.clamp(0.0, 1.0) as f32)
}

fn rgb(r: f64, g: f64, b: f64) -> gdk::RGBA {
    rgba(r, g, b, 1.0)
}

pub fn font(px: f64, bold: bool) -> pango::FontDescription {
    let mut fd = pango::FontDescription::from_string(if bold { "Sans Bold" } else { "Sans" });
    fd.set_absolute_size(px * pango::SCALE as f64);
    fd
}

fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    Rgb(a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t, a.2 + (b.2 - a.2) * t)
}

fn spiral(k: usize, spacing: f64) -> (f64, f64) {
    let ang = k as f64 * 2.399_963;
    let d = (k as f64).sqrt() * spacing;
    (ang.cos() * d, ang.sin() * d)
}

fn unit(seed: usize) -> (f64, f64) {
    let a = (seed as f64 * 2.399_963) % TAU;
    (a.cos(), a.sin())
}

pub fn glib_escape(s: &str) -> String {
    gtk::glib::markup_escape_text(s).to_string()
}
