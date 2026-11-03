//! Bubble view of processes: what each bubble is, how processes group in
//! each layout, search, and rendering. The layout simulation itself lives
//! in `piddle_scene`, shared with kpiddle through piddle-common.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gsk, pango};

use piddle_core::containers::{self, Choice, Filter, Inventory, Placement};
use piddle_core::encode::{Rgb, cpu_color, fmt_bytes, fmt_cpu, mem_radius, mix};
use piddle_core::sampler::{Memory, ProcInfo, Snapshot};
use piddle_core::service::ServiceKind;
use piddle_gtk::Paint;
use piddle_gtk::draw::{card, font, glib_escape, layout, point, rect, rgb, rgba, ring, text};
pub use piddle_scene::Motion;
use piddle_scene::{Arrangement, GroupSpec, Item, Scene};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Free,
    App,
    Service,
    Parent,
    Tree,
    /// One hull per container; offered only while containers run.
    Containers,
}

impl Layout {
    /// Layouts that pull bubbles into clusters with a shared hull.
    fn grouped(self) -> bool {
        matches!(self, Layout::App | Layout::Service | Layout::Parent | Layout::Containers)
    }

    fn arrangement(self) -> Arrangement {
        match self {
            Layout::Free => Arrangement::Free,
            Layout::App | Layout::Service | Layout::Parent | Layout::Containers => Arrangement::Grouped,
            Layout::Tree => Arrangement::Tree,
        }
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

/// Outer hulls of Compose projects and pods, apart from the container tint.
const MANAGER_TINT: (f64, f64, f64) = (0.98, 0.67, 0.47);

/// A process counts as "active" if it used at least this much CPU recently.
const ACTIVE_CPU: f32 = 0.5;
const ACTIVE_HOLD: Duration = Duration::from_secs(4);

/// A process as a bubble.
pub struct Proc {
    pub info: ProcInfo,
    /// Its container, if it is in one.
    pub container: Option<Arc<Placement>>,
}

#[derive(Clone)]
pub struct GroupInfo {
    label: String,
    kind: Option<ServiceKind>,
    /// The Compose project or pod, in the containers layout.
    cluster: Option<Arc<str>>,
}

pub struct LabelCache {
    key: (i32, i32),
    name: pango::Layout,
    pid: pango::Layout,
}

impl Item for Proc {
    type Key = i32;
    type Group = GroupInfo;
    type Cache = Option<LabelCache>;

    fn key(&self) -> i32 {
        self.info.pid
    }
    fn same_instance(&self, other: &Self) -> bool {
        self.info.start_ticks == other.info.start_ticks
    }
    fn parent(&self) -> Option<i32> {
        Some(self.info.ppid)
    }
    fn radius(&self) -> f64 {
        mem_radius(self.info.rss)
    }
    fn cpu(&self) -> f64 {
        self.info.cpu as f64
    }
    fn mem(&self) -> u64 {
        self.info.rss
    }
    fn affinity(&self) -> &str {
        &self.info.app
    }
}

pub type Bubble = piddle_scene::Bubble<Proc>;

#[derive(Clone, Copy, Default)]
pub struct Meta {
    pub ncpu: usize,
    pub clk_tck: u64,
    pub boot_time: Option<u64>,
    pub total_cpu: Option<f32>,
    pub memory: Option<Memory>,
    pub sample_cost: Duration,
    pub self_cpu: f32,
    pub procs: usize,
    pub threads: u64,
}

pub struct View {
    pub scene: Scene<Proc>,
    activity: HashMap<i32, Instant>,
    names: HashMap<i32, Arc<str>>,
    pub layout: Layout,
    query: String,
    pub show_kernel: bool,
    pub active_only: bool,
    pub meta: Meta,
    last_procs: Vec<ProcInfo>,
    pub top: Vec<(Arc<str>, i32, f32)>,
    /// What the containers picker limits the view to.
    pub filter: Filter,
    /// The picker's entries; empty when no process is in a container.
    pub choices: Vec<Choice>,
    /// The filter's container or project is gone.
    pub filter_gone: bool,
    pub inventory: Arc<Inventory>,
    /// Each service's container placement, for the last sample.
    placed: HashMap<usize, Option<Arc<Placement>>>,
    /// Compose projects and pods by key, for the outer hulls' labels.
    cluster_groups: HashMap<Arc<str>, containers::Group>,
}

/// A process's group in a layout.
fn group_of(layout: Layout, names: &HashMap<i32, Arc<str>>, item: &Proc) -> GroupSpec<GroupInfo> {
    let p = &item.info;
    let mut cluster = None;
    let (key, label, kind) = match layout {
        Layout::App => (p.app.clone(), p.app.to_string(), None),
        Layout::Service => {
            let s = &p.service;
            (s.key.clone(), s.label.to_string(), Some(s.kind))
        }
        Layout::Parent => {
            let pp = p.ppid;
            let label = match names.get(&pp) {
                Some(n) => format!("{n} · {pp}"),
                None if pp == 0 => "kernel".to_string(),
                None => format!("PID {pp}"),
            };
            (Arc::from(pp.to_string()), label, None)
        }
        Layout::Containers => match &item.container {
            Some(c) => {
                // A Compose project's or pod's containers share an outer hull.
                cluster = c.group.as_ref().map(|g| g.key.clone());
                // Inside its project's hull, "shop-web-1" reads as "web-1".
                let name = c
                    .group
                    .as_ref()
                    .and_then(|g| c.name.strip_prefix(&*g.label))
                    .and_then(|rest| rest.strip_prefix(['-', '_']))
                    .filter(|rest| !rest.is_empty())
                    .unwrap_or(&c.name);
                (c.key.clone(), name.to_string(), Some(ServiceKind::Container))
            }
            None => (Arc::from("host"), "not in a container".to_string(), None),
        },
        Layout::Free | Layout::Tree => (Arc::from(""), String::new(), None),
    };
    GroupSpec { key, info: GroupInfo { label, kind, cluster } }
}

impl View {
    pub fn new() -> Self {
        let mut scene = Scene::new();
        scene.set_cluster_of(Some(|g: &GroupInfo| g.cluster.clone()));
        Self {
            scene,
            activity: HashMap::new(),
            names: HashMap::new(),
            layout: Layout::Free,
            query: String::new(),
            show_kernel: false,
            active_only: false,
            meta: Meta::default(),
            last_procs: Vec::new(),
            top: Vec::new(),
            filter: Filter::All,
            choices: Vec::new(),
            filter_gone: false,
            inventory: Arc::default(),
            placed: HashMap::new(),
            cluster_groups: HashMap::new(),
        }
    }

    /// The container a process is in, as of the last sample.
    pub fn placement(&self, p: &ProcInfo) -> Option<Arc<Placement>> {
        self.placed.get(&(Arc::as_ptr(&p.service) as usize))?.clone()
    }

    /// Limit the view to a picker entry.
    pub fn set_filter(&mut self, f: Filter) {
        self.filter = f;
        self.filter_gone = false;
        self.sync_bubbles();
    }

    /// How the view is limited, for the banner: (text, gone).
    pub fn filter_text(&self) -> Option<(String, bool)> {
        if self.filter.is_all() {
            return None;
        }
        let name = containers::filter_label(&self.filter, &self.choices);
        Some(if self.filter_gone {
            (format!("{name} is no longer running"), true)
        } else if self.filter == Filter::Containers {
            ("Containers only".to_string(), false)
        } else {
            (format!("Only {name}"), false)
        })
    }

    /// Current world→screen scale, used by the legend to draw true-size references.
    pub fn scale(&self) -> f64 {
        self.scene.scale()
    }

    pub fn bubble(&self, pid: i32) -> Option<&Bubble> {
        self.scene.bubble(&pid)
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
            memory: snap.memory,
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

        self.placed.clear();
        for p in &snap.procs {
            let k = Arc::as_ptr(&p.service) as usize;
            self.placed.entry(k).or_insert_with(|| containers::place(&p.service, &snap.containers).map(Arc::new));
        }
        self.cluster_groups =
            self.placed.values().filter_map(|p| p.as_ref()?.group.clone()).map(|g| (g.key.clone(), g)).collect();
        self.choices = containers::choices(&snap.procs, &snap.containers);
        match containers::resolve(&self.filter, &self.choices) {
            Some(f) => {
                self.filter = f;
                self.filter_gone = false;
            }
            None => self.filter_gone = true,
        }
        self.inventory = snap.containers;
        self.last_procs = snap.procs;
        self.sync_bubbles();
    }

    /// Reconcile bubbles with the latest sample and current filters.
    fn sync_bubbles(&mut self) {
        let items = self
            .last_procs
            .iter()
            .filter(|p| self.show_kernel || !p.kthread)
            .filter(|p| !self.active_only || self.activity.contains_key(&p.pid))
            .map(|p| Proc { info: p.clone(), container: self.placement(p) })
            .filter(|p| self.filter.is_all() || (!self.filter_gone && self.filter.shows(p.container.as_deref())))
            .collect();
        let (layout, names) = (self.layout, &self.names);
        self.scene.sync(items, |p| group_of(layout, names, p));
        self.refresh_matches();
    }

    pub fn set_query(&mut self, q: &str) {
        self.query = q.trim().to_lowercase();
        self.refresh_matches();
    }

    pub fn has_query(&self) -> bool {
        !self.query.is_empty()
    }

    fn refresh_matches(&mut self) {
        let q = self.query.as_str();
        let numeric = !q.is_empty() && q.bytes().all(|c| c.is_ascii_digit());
        self.scene.set_matches(!q.is_empty(), |p| {
            let p = &p.info;
            (numeric && p.pid.to_string().starts_with(q)) || p.search_text.contains(q) || p.service.search.contains(q)
        });
    }

    pub fn set_layout(&mut self, l: Layout) {
        self.layout = l;
        self.scene.spawn_near_parent = l == Layout::Parent;
        let names = &self.names;
        self.scene.set_arrangement(l.arrangement(), |p| group_of(l, names, p));
    }

    /// Re-apply filters (kernel threads / active-only) against the last sample.
    pub fn refilter(&mut self) {
        self.sync_bubbles();
    }

    /// Advance animation by `dt` seconds and report how much is still changing.
    pub fn step(&mut self, dt: f64) -> Motion {
        self.scene.step(dt)
    }

    /// Screen position of a live bubble's center.
    pub fn screen_pos(&self, pid: i32) -> Option<(f64, f64)> {
        self.scene.screen_pos(&pid)
    }

    pub fn pick(&self, sx: f64, sy: f64) -> Option<i32> {
        self.scene.pick(sx, sy)
    }

    // ------------------------------------------------------------------ drawing
    //
    // Rendering goes through GtkSnapshot render nodes so gradients, clips,
    // strokes and (glyph-cached) text are drawn by the GPU renderer.

    pub fn render(&mut self, snap: &gtk::Snapshot, pctx: &pango::Context) {
        let (w, h) = self.scene.size;
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

        if self.scene.bubbles.is_empty() {
            let msg = match self.filter_text() {
                // The banner over the canvas says what happened.
                Some((_, true)) => return,
                _ if self.last_procs.is_empty() => "Sampling /proc…",
                _ => "No processes to show",
            };
            let l = layout(pctx, msg, 15.0, false);
            let (lw, lh) = l.pixel_size();
            text(snap, &l, (w - lw as f64) / 2.0, (h - lh as f64) / 2.0, rgba(1.0, 1.0, 1.0, 0.5));
            return;
        }

        let s = self.scene.scale();
        let dimming = self.has_query();

        // Compose projects and pods: an outer hull around their containers.
        if self.layout == Layout::Containers && !self.scene.clusters.is_empty() {
            let pb = gsk::PathBuilder::new();
            for c in self.scene.clusters.iter().filter(|c| c.members > 0) {
                let (cx, cy) = self.scene.to_screen(c.cx, c.cy);
                pb.add_circle(&point(cx, cy), (c.hull * s) as f32);
            }
            let path = pb.to_path();
            let (r, g, b) = MANAGER_TINT;
            snap.append_fill(&path, gsk::FillRule::Winding, &rgba(r, g, b, 0.035));
            snap.append_stroke(&path, &gsk::Stroke::new(1.5), &rgba(r, g, b, 0.4));
        }

        // Group hulls.
        if self.layout.grouped() {
            // One batched path per tint (service kinds differ; other layouts use one).
            let mut paths: Vec<(Option<ServiceKind>, gsk::PathBuilder)> = Vec::new();
            let min = self.min_hull_members();
            for g in self.scene.groups.iter().filter(|g| g.members >= min) {
                let (gx, gy) = self.scene.to_screen(g.cx, g.cy);
                let pb = match paths.iter().position(|(k, _)| *k == g.info.kind) {
                    Some(i) => &paths[i].1,
                    None => {
                        paths.push((g.info.kind, gsk::PathBuilder::new()));
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
            for b in &self.scene.bubbles {
                let Some(p) = b.parent else { continue };
                let pb = &self.scene.bubbles[p];
                let (x0, y0) = self.scene.to_screen(pb.x, pb.y);
                let (x1, y1) = self.scene.to_screen(b.x, b.y);
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
        let hover = self.scene.hover;
        for i in 0..self.scene.bubbles.len() {
            let (sx, sy) = {
                let b = &self.scene.bubbles[i];
                self.scene.to_screen(b.x, b.y)
            };
            let b = &mut self.scene.bubbles[i];
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
            if hover == Some(b.item.info.pid) && !b.dying {
                ring(snap, sx, sy, sr + 2.5, 2.5, rgba(1.0, 1.0, 1.0, 0.95));
            }

            if sr >= 15.0 {
                draw_label(snap, pctx, b, sx, sy, sr, c, a);
            }
        }

        if self.layout.grouped() {
            self.draw_group_labels(snap, pctx);
        }
        if self.layout == Layout::Containers {
            self.draw_cluster_labels(snap, pctx);
        }
        if self.layout == Layout::Tree {
            self.draw_hub_labels(snap, pctx);
        }
        self.draw_hover_card(snap, pctx);
    }

    /// Tree hubs are often small (e.g. systemd) but are where the structure
    /// meets, so tag every parent that is too small for an inside label.
    fn draw_hub_labels(&self, snap: &gtk::Snapshot, pctx: &pango::Context) {
        let s = self.scene.scale();
        let dimming = self.has_query();
        for b in self.scene.bubbles.iter().filter(|b| b.kids > 0 && !b.dying && b.r * s < 15.0) {
            let (sx, sy) = self.scene.to_screen(b.x, b.y);
            let markup = format!(
                "<b>{}</b> <span alpha='60%'>{} · {} child{}</span>",
                glib_escape(&b.item.info.name),
                b.item.info.pid,
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

    /// Compose project and pod names, under their outer hulls (the
    /// containers' own labels sit at the top of theirs).
    fn draw_cluster_labels(&self, snap: &gtk::Snapshot, pctx: &pango::Context) {
        let s = self.scene.scale();
        let h = self.scene.size.1;
        for c in self.scene.clusters.iter().filter(|c| c.members > 0) {
            let Some(g) = self.cluster_groups.get(&c.key) else { continue };
            let (cx, cy) = self.scene.to_screen(c.cx, c.cy);
            let cr = c.hull * s;
            let markup = format!(
                "<b>{}</b>  <span alpha='65%'>{} · {} container{} · {} · {}</span>",
                glib_escape(&g.label),
                g.kind.label(),
                c.groups,
                if c.groups == 1 { "" } else { "s" },
                fmt_cpu(c.cpu),
                fmt_bytes(c.mem)
            );
            let l = pango::Layout::new(pctx);
            l.set_font_description(Some(&font(11.5, false)));
            l.set_markup(&markup);
            l.set_width((cr.max(80.0) * 2.0 * pango::SCALE as f64) as i32);
            l.set_ellipsize(pango::EllipsizeMode::End);
            let (lw, lh) = l.pixel_size();
            let (lw, lh) = (lw as f64, lh as f64);
            let (w, _) = self.scene.size;
            let x = (cx - lw / 2.0).clamp(12.0, (w - lw - 12.0).max(12.0));
            let y = (cy + cr - lh / 2.0).min(h - lh - 6.0);
            let (r, gg, b) = MANAGER_TINT;
            card(snap, x - 9.0, y - 3.0, lw + 18.0, lh + 6.0, (lh + 6.0) / 2.0, rgba(0.09, 0.105, 0.14, 0.94), {
                rgba(r, gg, b, 0.6)
            });
            text(snap, &l, x, y, rgba(0.95, 0.9, 0.85, 0.98));
        }
    }

    /// Groups with fewer members get no hull: a lone process needs none,
    /// except in the containers layout, where every container is outlined.
    fn min_hull_members(&self) -> usize {
        if self.layout == Layout::Containers { 1 } else { 2 }
    }

    fn draw_group_labels(&self, snap: &gtk::Snapshot, pctx: &pango::Context) {
        let s = self.scene.scale();
        let min = self.min_hull_members();
        for g in self.scene.groups.iter().filter(|g| g.members >= min) {
            let (gx, gy) = self.scene.to_screen(g.cx, g.cy);
            let gr = g.hull * s;
            // Containers are named however small: that's what the layout is for.
            if gr < 30.0 && self.layout != Layout::Containers {
                continue;
            }
            // Small containers: the name only, so neighbours' labels fit.
            let markup = if gr < 60.0 {
                format!("<b>{}</b>", glib_escape(&g.info.label))
            } else {
                let kind = g.info.kind.map(|k| format!("{} · ", k.label())).unwrap_or_default();
                format!(
                    "<b>{}</b>  <span alpha='65%'>{kind}×{} · {} · {}</span>",
                    glib_escape(&g.info.label),
                    g.members,
                    fmt_cpu(g.cpu),
                    fmt_bytes(g.mem)
                )
            };
            let gr = gr.max(60.0);
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
                let (r, gr, b) = kind_tint(g.info.kind);
                rgba(r, gr, b, if g.info.kind.is_some() { 0.5 } else { 0.25 })
            });
            text(snap, &l, x, y, rgba(0.86, 0.89, 0.95, 0.95));
        }
    }

    fn draw_hover_card(&self, snap: &gtk::Snapshot, pctx: &pango::Context) {
        let Some(b) = self.scene.hover.and_then(|p| self.bubble(p)) else { return };
        let p = &b.item.info;
        let mem_pct = self.meta.memory.map_or(String::new(), |m| format!(" ({:.1}% RAM)", m.percent_of(p.rss)));
        let markup = format!(
            "<span size='large' weight='bold'>{}</span>\n\
             <span alpha='65%'>PID {} · {}\n{} · {}</span>\n\n\
             CPU\t\t<b>{}</b> <span alpha='60%'>of one core</span>\n\
             Memory\t<b>{}</b><span alpha='60%'>{}</span>\n\
             Threads\t<b>{}</b>\n\n\
             <span alpha='50%' size='small'>Click for details and actions</span>",
            glib_escape(&p.name),
            p.pid,
            glib_escape(&p.user),
            if b.item.container.is_some() { "container" } else { p.service.kind.label() },
            glib_escape(b.item.container.as_ref().map_or(&*p.service.label, |c| &*c.name)),
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
        let (w, h) = self.scene.size;
        let (mx, my) = self.scene.mouse;
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
    if b.cache.as_ref().map(|l| l.key) != Some(key) {
        let name = layout(pctx, &b.item.info.name, px as f64, true);
        name.set_width(width * pango::SCALE);
        name.set_ellipsize(pango::EllipsizeMode::End);
        name.set_alignment(pango::Alignment::Center);
        let pid = layout(pctx, &b.item.info.pid.to_string(), (px as f64 * 0.82).max(8.0), false);
        pid.set_width(width * pango::SCALE);
        pid.set_alignment(pango::Alignment::Center);
        b.cache = Some(LabelCache { key, name, pid });
    }
    let lc = b.cache.as_ref().unwrap();
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

impl Paint for View {
    fn resize(&mut self, width: f64, height: f64) {
        self.scene.size = (width, height);
    }

    fn paint(&mut self, snapshot: &gtk::Snapshot, pctx: &pango::Context) {
        self.render(snapshot, pctx);
    }
}
