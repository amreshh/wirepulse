//! Canvas renderer and pointer interaction for the global map.
//!
//! Lives outside Leptos' reactive system: the component pushes new data in
//! with [`Scene::update`], and a requestAnimationFrame loop draws. Mouse,
//! touch and pen all go through pointer events (pan, pinch/wheel zoom, tap).

mod three;

use crate::icons;
use crate::settings::{EdgeStyle, LabelMode};
use crate::util::{fmt_rate, hue, window};
use common::aggregate::{AggEdge, AggGraph, AggNode, Level};
use common::layout::{layout, Arrangement, Layout, LayoutStyle, Placed, IDLE_GROUP};
use common::{is_unresolved, NodeKind};
use leptos::prelude::*;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::f64::consts::TAU;
use std::rc::Rc;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, Path2d, PointerEvent, WheelEvent};

const BG: &str = "#0d1117";
const CARD_BG: &str = "#161b22";
const EDGE_OK: &str = "#3fb6a8";
const EDGE_DROP: &str = "#ff5c5c";
const EDGE_IDLE: &str = "#4a5363";
const HIGHLIGHT: &str = "#f5c542";
const EXTERNAL: &str = "#8a94a6";
// No Inter here: its contextual alternates turn "4x2" in pod names into "4×2".
const FONT: &str = "system-ui, -apple-system, \"Segoe UI\", Roboto, sans-serif";

/// What the component tells the scene to show.
#[derive(Clone, PartialEq, Default)]
pub struct SceneInput {
    pub agg: AggGraph,
    pub level: Option<Level>,
    pub style: LayoutStyle,
    pub edges: EdgeStyle,
    pub labels: LabelMode,
    pub icons: bool,
    pub animate: bool,
    pub three_d: bool,
    pub tower: bool,
    pub auto_rotate: bool,
    pub selected: Option<String>,
    pub highlight: BTreeSet<String>,
    pub search: String,
    pub dropped_only: bool,
    pub hide_idle: bool,
    /// Bumped to request "fit to screen".
    pub fit: u32,
}

#[derive(Clone, Copy)]
struct View {
    x: f64,
    y: f64,
    scale: f64,
}

pub struct Scene {
    canvas: HtmlCanvasElement,
    ctx: CanvasRenderingContext2d,
    input: SceneInput,
    layout: Layout,
    nodes: HashMap<String, AggNode>,
    /// Node x positions per row (y rounded), to see what sits between two
    /// nodes on the same row.
    rows: HashMap<i64, Vec<f64>>,
    icon_paths: HashMap<&'static str, Path2d>,
    view: View,
    needs_fit: bool,
    /// CSS pixel size and device pixel ratio the backing store was sized for.
    size: (f64, f64, f64),
    /// Viewport aspect ratio the current layout was computed for.
    layout_aspect: f64,
    /// Newly selected node to bring into view if the details panel covers it.
    reveal: Option<String>,
    hover: Option<String>,
    pointer: Option<(f64, f64)>,
    pointers: HashMap<i32, (f64, f64)>,
    pinch: Option<f64>,
    down_at: Option<(f64, f64)>,
    moved: bool,
    last_draw: f64,
    on_select: RwSignal<Option<String>>,
    // 3D state (see scene/three.rs).
    cam: three::Cam,
    pos3: HashMap<String, (f64, f64, f64)>,
    plates: Vec<three::Plate>,
    /// Screen positions of nodes from the last 3D frame, far to near:
    /// (id, x, y, radius).
    proj: RefCell<Vec<(String, f64, f64, f64)>>,
    pan_drag: bool,
    last_interaction: f64,
    last_ts: f64,
}

impl Scene {
    pub fn new(canvas: HtmlCanvasElement, on_select: RwSignal<Option<String>>) -> Self {
        let ctx = canvas
            .get_context("2d")
            .ok()
            .flatten()
            .and_then(|c| c.dyn_into::<CanvasRenderingContext2d>().ok())
            .expect("2d canvas");
        let icon_paths = icons::LEGEND
            .iter()
            .map(|(k, _)| *k)
            .chain(["ingress", "health", "unknown"])
            .filter_map(|k| Path2d::new_with_path_string(icons::path(k)).ok().map(|p| (k, p)))
            .collect();
        Self {
            canvas,
            ctx,
            input: SceneInput::default(),
            layout: Layout::default(),
            nodes: HashMap::new(),
            rows: HashMap::new(),
            icon_paths,
            view: View { x: 0.0, y: 0.0, scale: 1.0 },
            needs_fit: true,
            size: (0.0, 0.0, 1.0),
            layout_aspect: 1.6,
            reveal: None,
            hover: None,
            pointer: None,
            pointers: HashMap::new(),
            pinch: None,
            down_at: None,
            moved: false,
            last_draw: 0.0,
            on_select,
            cam: three::Cam::default(),
            pos3: HashMap::new(),
            plates: Vec::new(),
            proj: RefCell::new(Vec::new()),
            pan_drag: false,
            last_interaction: 0.0,
            last_ts: 0.0,
        }
    }

    pub fn update(&mut self, input: SceneInput) {
        let mode_changed = input.three_d != self.input.three_d || input.tower != self.input.tower;
        let relayout = input.agg != self.input.agg
            || input.level != self.input.level
            || input.style != self.input.style
            || mode_changed;
        if input.level != self.input.level
            || input.fit != self.input.fit
            || input.style != self.input.style
            || mode_changed
        {
            self.needs_fit = true;
        }
        if input.selected != self.input.selected {
            self.reveal = input.selected.clone();
        }
        if input.three_d && (mode_changed || !self.input.three_d) {
            // A lower angle separates the tower's floors; the map reads best
            // from higher up.
            self.cam.pitch = if input.tower { 0.42 } else { 0.82 };
        }
        if relayout {
            if self.input.agg.nodes.is_empty() {
                self.needs_fit = true;
            }
            self.nodes = input.agg.nodes.iter().map(|n| (n.id.clone(), n.clone())).collect();
        }
        self.input = input;
        if relayout {
            self.relayout();
        }
    }

    fn relayout(&mut self) {
        if let Some(level) = self.input.level {
            self.layout = layout(&self.input.agg, level, self.layout_aspect, self.input.style);
        }
        self.rows.clear();
        for p in self.layout.nodes.values() {
            self.rows.entry(p.y.round() as i64).or_default().push(p.x);
        }
        if self.input.three_d {
            self.build3d();
        }
    }

    /// Whether another node sits on the same row strictly between a and b.
    fn blocked(&self, a: &Placed, b: &Placed) -> bool {
        if (a.y - b.y).abs() > 0.5 {
            return false;
        }
        let (lo, hi) = (a.x.min(b.x) + 1.0, a.x.max(b.x) - 1.0);
        self.rows.get(&(a.y.round() as i64)).is_some_and(|xs| xs.iter().any(|&x| x > lo && x < hi))
    }

    fn level(&self) -> Level {
        self.input.level.unwrap_or(Level::Workload)
    }

    // ---- coordinates -------------------------------------------------------

    fn to_world(&self, sx: f64, sy: f64) -> (f64, f64) {
        let (w, h, _) = self.size;
        ((sx - w / 2.0) / self.view.scale + self.view.x, (sy - h / 2.0) / self.view.scale + self.view.y)
    }

    fn fit(&mut self) {
        if self.input.three_d {
            self.fit3d();
            return;
        }
        let (w, h, _) = self.size;
        let (x0, y0, x1, y1) = self.layout.bounds;
        if w <= 0.0 || x1 <= x0 {
            return;
        }
        // Room for labels under the outermost nodes and the overlays at the bottom.
        let (pad_x, pad_top, pad_bottom) = (40.0, 24.0, 56.0);
        let scale = ((w - pad_x) / (x1 - x0)).min((h - pad_top - pad_bottom) / (y1 - y0)).clamp(0.03, 2.5);
        self.view = View { x: (x0 + x1) / 2.0, y: (y0 + y1) / 2.0 + (pad_bottom - pad_top) / 2.0 / scale, scale };
        self.needs_fit = false;
    }

    /// The details panel is a right-hand sidebar on wide screens and a bottom
    /// sheet on phones; pan so the selected node is not underneath it.
    fn reveal_selected(&mut self) {
        if self.input.three_d {
            self.reveal3d();
            return;
        }
        let Some(id) = self.reveal.take() else { return };
        let Some(p) = self.layout.nodes.get(&id).copied() else { return };
        let (w, h, _) = self.size;
        let s = self.view.scale;
        let sx = (p.x - self.view.x) * s + w / 2.0;
        let sy = (p.y - self.view.y) * s + h / 2.0;
        if w <= 760.0 {
            if sy > h * 0.42 || sy < 0.0 || sx < 0.0 || sx > w {
                self.view.x = p.x;
                self.view.y = p.y + h * 0.22 / s;
            }
        } else if sx > w - 370.0 || sx < 0.0 || sy < 0.0 || sy > h {
            self.view.x = p.x + 180.0 / s;
            self.view.y = p.y;
        }
    }

    fn zoom_at(&mut self, sx: f64, sy: f64, factor: f64) {
        let (wx, wy) = self.to_world(sx, sy);
        let (w, h, _) = self.size;
        self.view.scale = (self.view.scale * factor).clamp(0.02, 8.0);
        self.view.x = wx - (sx - w / 2.0) / self.view.scale;
        self.view.y = wy - (sy - h / 2.0) / self.view.scale;
    }

    fn hit(&self, sx: f64, sy: f64) -> Option<String> {
        if self.input.three_d {
            return self.hit3d(sx, sy);
        }
        let (wx, wy) = self.to_world(sx, sy);
        let slop = 4.0 / self.view.scale;
        self.layout
            .nodes
            .iter()
            .filter(|(_, p)| p.contains(wx, wy, slop))
            .min_by(|a, b| {
                let da = (a.1.x - wx).hypot(a.1.y - wy);
                let db = (b.1.x - wx).hypot(b.1.y - wy);
                da.total_cmp(&db)
            })
            .map(|(id, _)| id.clone())
    }

    // ---- input -------------------------------------------------------------

    fn pointer_down(&mut self, e: &PointerEvent) {
        let p = (f64::from(e.offset_x()), f64::from(e.offset_y()));
        self.pointers.insert(e.pointer_id(), p);
        let _ = self.canvas.set_pointer_capture(e.pointer_id());
        self.last_interaction = js_sys::Date::now();
        // In 3D, plain drag orbits; Shift- or right-drag pans.
        self.pan_drag = e.shift_key() || e.button() == 2;
        if self.pointers.len() == 1 {
            self.down_at = Some(p);
            self.moved = false;
        } else {
            self.pinch = self.pinch_distance();
            self.moved = true;
        }
    }

    fn pinch_distance(&self) -> Option<f64> {
        let v: Vec<_> = self.pointers.values().collect();
        (v.len() == 2).then(|| (v[0].0 - v[1].0).hypot(v[0].1 - v[1].1))
    }

    fn pointer_move(&mut self, e: &PointerEvent) {
        let p = (f64::from(e.offset_x()), f64::from(e.offset_y()));
        self.pointer = Some(p);
        let Some(prev) = self.pointers.get(&e.pointer_id()).copied() else {
            // Hover without a pressed button (mouse only).
            self.hover = self.hit(p.0, p.1);
            return;
        };
        self.pointers.insert(e.pointer_id(), p);
        self.last_interaction = js_sys::Date::now();
        if self.pointers.len() == 2 {
            let v: Vec<_> = self.pointers.values().copied().collect();
            let mid = ((v[0].0 + v[1].0) / 2.0, (v[0].1 + v[1].1) / 2.0);
            if let (Some(before), Some(now)) = (self.pinch, self.pinch_distance()) {
                if before > 0.0 {
                    if self.input.three_d {
                        self.zoom3(before / now);
                    } else {
                        self.zoom_at(mid.0, mid.1, now / before);
                    }
                }
                self.pinch = Some(now);
            }
            if self.input.three_d {
                self.pan3((p.0 - prev.0) / 2.0, (p.1 - prev.1) / 2.0);
                return;
            }
            // Two-finger drag pans by half the movement of the moving finger.
            self.view.x -= (p.0 - prev.0) / 2.0 / self.view.scale;
            self.view.y -= (p.1 - prev.1) / 2.0 / self.view.scale;
            return;
        }
        if let Some(d) = self.down_at {
            if (p.0 - d.0).hypot(p.1 - d.1) > 5.0 {
                self.moved = true;
            }
        }
        if self.moved {
            let (dx, dy) = (p.0 - prev.0, p.1 - prev.1);
            if self.input.three_d {
                if self.pan_drag {
                    self.pan3(dx, dy);
                } else {
                    self.orbit(dx, dy);
                }
            } else {
                self.view.x -= dx / self.view.scale;
                self.view.y -= dy / self.view.scale;
            }
        }
    }

    /// Returns the tapped node id (or `Some(None)` for a tap on empty space).
    fn pointer_up(&mut self, e: &PointerEvent) -> Option<Option<String>> {
        self.pointers.remove(&e.pointer_id());
        if self.pointers.len() < 2 {
            self.pinch = None;
        }
        if !self.pointers.is_empty() {
            return None;
        }
        let tap = !self.moved;
        self.down_at = None;
        self.moved = false;
        tap.then(|| self.hit(f64::from(e.offset_x()), f64::from(e.offset_y())))
    }

    fn wheel(&mut self, e: &WheelEvent) {
        self.last_interaction = js_sys::Date::now();
        if self.input.three_d {
            self.zoom3((e.delta_y() * 0.0015).exp());
            return;
        }
        let factor = (-e.delta_y() * 0.0015).exp();
        self.zoom_at(f64::from(e.offset_x()), f64::from(e.offset_y()), factor);
    }

    // ---- frame -------------------------------------------------------------

    fn resize(&mut self) -> bool {
        let w = f64::from(self.canvas.client_width());
        let h = f64::from(self.canvas.client_height());
        if w <= 0.0 || h <= 0.0 {
            return false;
        }
        let dpr = window().device_pixel_ratio().max(1.0);
        if (w, h, dpr) != self.size {
            self.canvas.set_width((w * dpr) as u32);
            self.canvas.set_height((h * dpr) as u32);
            self.size = (w, h, dpr);
        }
        true
    }

    pub fn frame(&mut self, ts: f64) {
        // Hidden tab: nothing to draw.
        if !self.resize() {
            return;
        }
        // Large graphs: cap at ~25 fps.
        if self.input.agg.edges.len() > 1500 && ts - self.last_draw < 40.0 {
            return;
        }
        let dt = (ts - self.last_ts).clamp(0.0, 100.0);
        self.last_ts = ts;
        self.last_draw = ts;
        if self.input.three_d
            && self.input.auto_rotate
            && self.pointers.is_empty()
            && js_sys::Date::now() - self.last_interaction > 4000.0
        {
            self.cam.yaw += dt * 0.00012;
        }
        // Re-pack when the viewport shape changes a lot (rotation, resize).
        let aspect = self.size.0 / self.size.1;
        if (aspect / self.layout_aspect - 1.0).abs() > 0.25 {
            self.layout_aspect = aspect;
            self.relayout();
            self.needs_fit = true;
        }
        if self.needs_fit {
            self.fit();
        }
        self.reveal_selected();
        if self.input.three_d {
            self.draw3d(ts / 1000.0);
        } else {
            self.draw(ts / 1000.0);
        }
    }

    // ---- drawing -----------------------------------------------------------

    fn draw(&self, t: f64) {
        let c = &self.ctx;
        let (w, h, dpr) = self.size;
        let s = self.view.scale;
        let _ = c.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0);
        c.set_fill_style_str(BG);
        c.fill_rect(0.0, 0.0, w, h);
        c.set_line_cap("round");
        c.set_line_join("round");

        if self.input.agg.nodes.is_empty() {
            c.set_fill_style_str("#7d8590");
            c.set_font(&format!("14px {FONT}"));
            c.set_text_align("center");
            let _ = c.fill_text("Waiting for data…", w / 2.0, h / 2.0);
            return;
        }

        // World transform.
        let _ = c.set_transform(
            dpr * s,
            0.0,
            0.0,
            dpr * s,
            dpr * (w / 2.0 - self.view.x * s),
            dpr * (h / 2.0 - self.view.y * s),
        );

        let focus = self.focus_set();
        let lit = |id: &str| self.lit(id, &focus);

        self.draw_groups(s);
        self.draw_edges(s, t, &focus);
        self.draw_nodes(s, &lit);
        self.draw_tooltip();
    }

    /// Whether a node stays bright under the current selection, policy
    /// highlight and search.
    fn lit(&self, id: &str, focus: &Option<HashSet<&str>>) -> bool {
        let search = self.input.search.trim().to_lowercase();
        let matches = |n: &AggNode| {
            search.is_empty()
                || n.label.to_lowercase().contains(&search)
                || n.id.to_lowercase().contains(&search)
                || n.group.to_lowercase().contains(&search)
        };
        let in_focus = focus.as_ref().is_none_or(|f| f.contains(id));
        let in_hl = self.input.highlight.is_empty() || self.input.highlight.contains(id);
        in_focus && in_hl && self.nodes.get(id).is_some_and(matches)
    }

    fn dimming(&self) -> bool {
        self.input.selected.is_some() || !self.input.highlight.is_empty() || !self.input.search.trim().is_empty()
    }

    /// Selected node plus its neighbours, when something is selected.
    fn focus_set(&self) -> Option<HashSet<&str>> {
        let sel = self.input.selected.as_deref()?;
        if !self.nodes.contains_key(sel) {
            return None;
        }
        let mut f: HashSet<&str> = HashSet::from([sel]);
        for e in &self.input.agg.edges {
            if e.source == sel {
                f.insert(&e.target);
            } else if e.target == sel {
                f.insert(&e.source);
            }
        }
        Some(f)
    }

    fn draw_groups(&self, s: f64) {
        let c = &self.ctx;
        for g in &self.layout.groups {
            let neutral = g.name == common::EXTERNAL_GROUP || g.name == IDLE_GROUP;
            let hue = hue(&g.name);
            c.begin_path();
            if g.rect {
                rrect(c, g.x - g.w / 2.0, g.y - g.h / 2.0, g.w, g.h, 10.0);
            } else {
                let _ = c.arc(g.x, g.y, g.r(), 0.0, TAU);
            }
            if neutral {
                c.set_fill_style_str("rgba(138,148,166,0.05)");
                c.set_stroke_style_str("rgba(138,148,166,0.35)");
            } else {
                c.set_fill_style_str(&format!("hsla({hue},45%,50%,0.06)"));
                c.set_stroke_style_str(&format!("hsla({hue},45%,60%,0.35)"));
            }
            c.fill();
            c.set_line_width(1.0 / s);
            if g.name == IDLE_GROUP {
                let _ = c.set_line_dash(&dash(6.0 / s, 5.0 / s));
            }
            c.stroke();
            let _ = c.set_line_dash(&js_sys::Array::new());

            // Name: top-left in boxes, top-centre in circles; constant on screen.
            let fs = 13.0 / s;
            if g.w * s > 60.0 {
                c.set_font(&format!("600 {fs}px {FONT}"));
                c.set_text_baseline("top");
                c.set_fill_style_str(if neutral { "rgba(180,188,200,0.8)" } else { "rgba(220,226,235,0.85)" });
                let label = format!("{} · {}", g.name, g.members);
                if g.rect {
                    c.set_text_align("left");
                    let _ = c.fill_text(&label, g.x - g.w / 2.0 + 10.0, g.y - g.h / 2.0 + 8.0 / s.max(1.0));
                } else {
                    c.set_text_align("center");
                    let _ = c.fill_text(&label, g.x, g.y - g.r() + 8.0 / s);
                }
            }
        }
    }

    fn edge_visible(&self, e: &AggEdge) -> bool {
        let active = e.flows_per_sec + e.drops_per_sec > 0.0;
        if self.input.hide_idle && !active {
            return false;
        }
        if self.input.dropped_only && !(e.drops_per_sec > 0.0 || (e.total_drops > 0 && e.idle_secs < 60)) {
            return false;
        }
        true
    }

    fn draw_edges(&self, s: f64, t: f64, focus: &Option<HashSet<&str>>) {
        let c = &self.ctx;
        let animate = self.input.animate && self.input.agg.edges.len() <= 3000;
        let moving = dash(3.0 / s, 9.0 / s);
        let solid = js_sys::Array::new();

        for e in &self.input.agg.edges {
            if !self.edge_visible(e) {
                continue;
            }
            let (Some(a), Some(b)) = (self.layout.nodes.get(&e.source), self.layout.nodes.get(&e.target)) else {
                continue;
            };
            let in_focus = match (focus, self.input.selected.as_deref()) {
                (Some(_), Some(sel)) => e.source == sel || e.target == sel,
                _ => true,
            };
            let in_hl = self.input.highlight.is_empty()
                || self.input.highlight.contains(&e.source)
                || self.input.highlight.contains(&e.target);
            let alpha = if in_focus && in_hl { 1.0 } else { 0.08 };

            let rate = f64::from(e.flows_per_sec + e.drops_per_sec);
            let color = if e.drops_per_sec > 0.0 {
                EDGE_DROP
            } else if rate > 0.0 {
                EDGE_OK
            } else {
                EDGE_IDLE
            };
            let width_px = (1.0 + (1.0 + rate).log2() * 1.1).min(7.0);

            let blocked = self.input.edges == EdgeStyle::Orthogonal && self.blocked(a, b);
            let Some(geo) = EdgeGeo::new(a, b, s, self.input.edges, lane(&e.source, &e.target), blocked) else {
                continue;
            };
            c.set_global_alpha(alpha * if rate > 0.0 { 0.45 } else { 0.35 });
            c.set_stroke_style_str(color);
            c.set_line_width(width_px / s);
            let _ = c.set_line_dash(&solid);
            geo.path(c, s);
            c.stroke();

            // Moving dashes show direction and activity.
            if animate && rate > 0.0 && alpha > 0.5 {
                let speed = (18.0 + 14.0 * (1.0 + rate).log2()) / s;
                let _ = c.set_line_dash(&moving);
                c.set_line_dash_offset(-(t * speed) % (12.0 / s));
                c.set_global_alpha(alpha * 0.95);
                c.set_line_width((width_px * 0.8).max(1.5) / s);
                geo.path(c, s);
                c.stroke();
                let _ = c.set_line_dash(&solid);
            }

            // Arrowhead.
            c.set_global_alpha(alpha * 0.8);
            c.set_fill_style_str(color);
            let size = (5.0 + width_px) / s;
            let (ex, ey, dx, dy) = (geo.end.0, geo.end.1, geo.dir.0, geo.dir.1);
            c.begin_path();
            c.move_to(ex, ey);
            c.line_to(ex - dx * size - dy * size * 0.5, ey - dy * size + dx * size * 0.5);
            c.line_to(ex - dx * size + dy * size * 0.5, ey - dy * size - dx * size * 0.5);
            c.close_path();
            c.fill();
        }
        c.set_global_alpha(1.0);
    }

    fn node_color(&self, n: &AggNode) -> String {
        if n.kind == NodeKind::External {
            return EXTERNAL.into();
        }
        let h = hue(&n.group);
        let active = n.flows_in + n.flows_out + n.internal_flows > 0.0;
        format!("hsl({h},{}%,{}%)", if active { 60 } else { 25 }, if active { 52 } else { 38 })
    }

    /// Strokes the icon for `n` centred on (x, y), `size` world units wide.
    fn draw_icon(&self, n: &AggNode, x: f64, y: f64, size: f64, color: &str) {
        let Some(path) = self.icon_paths.get(icons::key(&n.workload_kind)) else { return };
        let c = &self.ctx;
        let k = size / 24.0;
        c.save();
        let _ = c.translate(x - size / 2.0, y - size / 2.0);
        let _ = c.scale(k, k);
        c.set_stroke_style_str(color);
        c.set_line_width(2.0);
        c.stroke_with_path(path);
        c.restore();
    }

    fn draw_nodes(&self, s: f64, lit: &dyn Fn(&str) -> bool) {
        let c = &self.ctx;
        let any_dim_rule = self.dimming();

        for (id, p) in &self.layout.nodes {
            let Some(n) = self.nodes.get(id) else { continue };
            let on = !any_dim_rule || lit(id);
            c.set_global_alpha(if on { 1.0 } else { 0.18 });
            let selected = self.input.selected.as_deref() == Some(id.as_str());
            let hovered = self.hover.as_deref() == Some(id.as_str());
            let highlighted = self.input.highlight.contains(id);
            if p.card {
                self.draw_card(n, p, s, selected, highlighted);
            } else {
                self.draw_circle(n, p, s, selected, highlighted, hovered);
            }
        }
        c.set_global_alpha(1.0);
    }

    fn draw_circle(&self, n: &AggNode, p: &Placed, s: f64, selected: bool, highlighted: bool, hovered: bool) {
        let c = &self.ctx;
        let r = p.r();
        if highlighted {
            c.begin_path();
            let _ = c.arc(p.x, p.y, r + 5.0 / s, 0.0, TAU);
            c.set_fill_style_str("rgba(245,197,66,0.30)");
            c.fill();
            c.set_stroke_style_str(HIGHLIGHT);
            c.set_line_width(1.5 / s);
            c.stroke();
        }
        c.begin_path();
        let _ = c.arc(p.x, p.y, r, 0.0, TAU);
        c.set_fill_style_str(&self.node_color(n));
        c.fill();
        if selected || n.drops > 0.0 {
            c.set_line_width((if selected { 2.5 } else { 2.0 }) / s);
            c.set_stroke_style_str(if selected { "#ffffff" } else { EDGE_DROP });
            c.stroke();
        }
        if self.input.icons && r * s >= 7.0 {
            self.draw_icon(n, p.x, p.y, r * 1.15, "rgba(255,255,255,0.92)");
        }

        // Label under the circle.
        let screen_r = r * s;
        let auto = match self.level() {
            Level::Namespace => true,
            Level::Workload => screen_r >= 5.0,
            Level::Pod => screen_r >= 11.0,
        };
        let show = match self.input.labels {
            LabelMode::Always => true,
            LabelMode::Auto => auto,
            LabelMode::Off => false,
        } || selected
            || hovered;
        if !show {
            return;
        }
        let fs = 11.0 / s;
        c.set_font(&format!("{fs}px {FONT}"));
        c.set_text_align("center");
        c.set_text_baseline("top");
        c.set_fill_style_str("rgba(230,237,243,0.9)");
        let mut label = truncate(&n.label, 26);
        if self.level() != Level::Pod && n.members > 1 {
            label = format!("{label} ×{}", n.members);
        }
        let _ = c.fill_text(&label, p.x, p.y + r + 3.0 / s);
        // No namespace containers in the layered view: say where it lives.
        if self.input.style.arrangement == Arrangement::Layered
            && n.kind == NodeKind::Pod
            && self.level() != Level::Namespace
        {
            c.set_fill_style_str("rgba(157,167,179,0.85)");
            c.set_font(&format!("{}px {FONT}", 10.0 / s));
            let _ = c.fill_text(&n.group, p.x, p.y + r + 16.0 / s);
        }
    }

    fn draw_card(&self, n: &AggNode, p: &Placed, s: f64, selected: bool, highlighted: bool) {
        let c = &self.ctx;
        let (x0, y0) = (p.x - p.w / 2.0, p.y - p.h / 2.0);
        let color = self.node_color(n);
        let radius = 8.0;

        if highlighted {
            c.begin_path();
            let pad = 5.0;
            rrect(c, x0 - pad, y0 - pad, p.w + 2.0 * pad, p.h + 2.0 * pad, radius + pad);
            c.set_fill_style_str("rgba(245,197,66,0.25)");
            c.fill();
        }
        c.begin_path();
        rrect(c, x0, y0, p.w, p.h, radius);
        c.set_fill_style_str(CARD_BG);
        c.fill();
        c.set_line_width((if selected { 2.5 } else { 1.2 }) / s.max(0.5));
        c.set_stroke_style_str(if selected {
            "#ffffff"
        } else if n.drops > 0.0 {
            EDGE_DROP
        } else if highlighted {
            HIGHLIGHT
        } else {
            &color
        });
        c.stroke();

        // Too small to read: a filled chip is enough.
        if p.h * s < 26.0 {
            c.set_fill_style_str(&color);
            c.fill();
            return;
        }

        // Icon badge.
        let badge = p.h - 14.0;
        let (bx, by) = (x0 + 7.0, p.y - badge / 2.0);
        c.begin_path();
        rrect(c, bx, by, badge, badge, 6.0);
        c.set_fill_style_str(&color);
        c.fill();
        if self.input.icons {
            self.draw_icon(n, bx + badge / 2.0, p.y, badge * 0.66, "rgba(255,255,255,0.95)");
        }

        // Title and subtitle.
        let tx = bx + badge + 8.0;
        let max_w = x0 + p.w - tx - 8.0;
        c.set_text_align("left");
        c.set_text_baseline("alphabetic");
        c.set_fill_style_str("#e6edf3");
        c.set_font(&format!("600 12.5px {FONT}"));
        let title = fit_text(c, &n.label, max_w);
        let _ = c.fill_text(&title, tx, p.y - 2.0);

        let mut sub = match (n.kind, self.level()) {
            (NodeKind::External, _) if is_unresolved(&n.id) => "pod not known".to_string(),
            (NodeKind::External, _) => "outside the cluster".to_string(),
            (_, Level::Namespace) => format!("{} pods", n.members),
            _ if self.input.style.arrangement == Arrangement::Layered => {
                if n.members > 1 {
                    format!("{} ×{} · {}", n.workload_kind, n.members, n.group)
                } else {
                    format!("{} · {}", n.workload_kind, n.group)
                }
            }
            _ if n.members > 1 => format!("{} ×{}", n.workload_kind, n.members),
            _ => n.workload_kind.clone(),
        };
        let rate = n.flows_in + n.flows_out + n.internal_flows;
        if rate > 0.0 {
            sub = format!("{sub} · {}", fmt_rate(rate));
        }
        c.set_font(&format!("11px {FONT}"));
        c.set_fill_style_str("#9da7b3");
        let sub = fit_text(c, &sub, max_w);
        let _ = c.fill_text(&sub, tx, p.y + 12.0);

        if n.drops > 0.0 {
            c.begin_path();
            let _ = c.arc(x0 + p.w - 9.0, y0 + 9.0, 4.0, 0.0, TAU);
            c.set_fill_style_str(EDGE_DROP);
            c.fill();
        }
    }

    fn draw_tooltip(&self) {
        let (Some(id), Some((px, py))) = (self.hover.as_ref(), self.pointer) else { return };
        let Some(n) = self.nodes.get(id) else { return };
        let c = &self.ctx;
        let (w, _, dpr) = self.size;
        let _ = c.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0);

        let mut lines = vec![n.label.clone()];
        let mut kind = n.workload_kind.clone();
        if n.members > 1 {
            kind = format!("{kind} · {} pods", n.members);
        }
        lines.push(kind);
        if n.group != common::EXTERNAL_GROUP && self.level() != Level::Namespace {
            lines.push(format!("namespace {}", n.group));
        }
        lines.push(format!("in {} · out {}", fmt_rate(n.flows_in), fmt_rate(n.flows_out)));
        if n.drops > 0.0 {
            lines.push(format!("dropped {}", fmt_rate(n.drops)));
        }

        c.set_font(&format!("12px {FONT}"));
        let tw = lines.iter().filter_map(|l| c.measure_text(l).ok()).map(|m| m.width()).fold(0.0, f64::max);
        let (bw, bh) = (tw + 16.0, lines.len() as f64 * 16.0 + 10.0);
        let x = if px + 14.0 + bw > w { px - bw - 10.0 } else { px + 14.0 };
        let y = py + 12.0;
        c.set_fill_style_str("rgba(22,27,34,0.95)");
        c.begin_path();
        rrect(c, x, y, bw, bh, 6.0);
        c.fill();
        c.set_stroke_style_str("rgba(110,118,129,0.6)");
        c.set_line_width(1.0);
        c.stroke();
        c.set_text_align("left");
        c.set_text_baseline("top");
        for (i, l) in lines.iter().enumerate() {
            c.set_fill_style_str(if i == 0 { "#e6edf3" } else { "#9da7b3" });
            let _ = c.fill_text(l, x + 8.0, y + 6.0 + i as f64 * 16.0);
        }
    }
}

/// Small per-edge offset in -3..=3 so right-angle edges don't all share
/// one line.
fn lane(a: &str, b: &str) -> f64 {
    let h = a
        .bytes()
        .chain([0])
        .chain(b.bytes())
        .fold(2_166_136_261u32, |h, x| (h ^ u32::from(x)).wrapping_mul(16_777_619));
    f64::from(h % 7) - 3.0
}

fn dash(on: f64, off: f64) -> js_sys::Array {
    js_sys::Array::of2(&JsValue::from_f64(on), &JsValue::from_f64(off))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        s.chars().take(max - 1).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

/// Shortens `s` with an ellipsis until it fits `max_w` in the current font.
fn fit_text(c: &CanvasRenderingContext2d, s: &str, max_w: f64) -> String {
    let width = |t: &str| c.measure_text(t).map(|m| m.width()).unwrap_or(0.0);
    if width(s) <= max_w {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let t: String = chars[..mid].iter().collect::<String>() + "…";
        if width(&t) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    chars[..lo].iter().collect::<String>() + "…"
}

/// Rounded rectangle path (`roundRect` is not available everywhere).
fn rrect(c: &CanvasRenderingContext2d, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    c.move_to(x + r, y);
    let _ = c.arc_to(x + w, y, x + w, y + h, r);
    let _ = c.arc_to(x + w, y + h, x, y + h, r);
    let _ = c.arc_to(x, y + h, x, y, r);
    let _ = c.arc_to(x, y, x + w, y, r);
    c.close_path();
}

/// Edge path between two node outlines in one of three styles.
struct EdgeGeo {
    /// Curved: [start, control, end]; otherwise the polyline corners.
    points: Vec<(f64, f64)>,
    curved: bool,
    end: (f64, f64),
    dir: (f64, f64),
}

impl EdgeGeo {
    /// `blocked`: another node sits between a and b on the same row.
    fn new(a: &Placed, b: &Placed, s: f64, style: EdgeStyle, lane: f64, blocked: bool) -> Option<Self> {
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        let len = dx.hypot(dy);
        if len < 1e-6 {
            return None;
        }
        let gap = 2.0 / s;
        let unit = |fx: f64, fy: f64, tx: f64, ty: f64| {
            let l = (tx - fx).hypot(ty - fy).max(1e-6);
            ((tx - fx) / l, (ty - fy) / l)
        };
        match style {
            EdgeStyle::Curved => {
                let (nx, ny) = (-dy / len, dx / len);
                let bend = (len * 0.12).min(60.0);
                let ctrl = ((a.x + b.x) / 2.0 + nx * bend, (a.y + b.y) / 2.0 + ny * bend);
                let sd = unit(a.x, a.y, ctrl.0, ctrl.1);
                let ed = unit(ctrl.0, ctrl.1, b.x, b.y);
                let start = (a.x + sd.0 * (a.reach(sd.0, sd.1) + gap), a.y + sd.1 * (a.reach(sd.0, sd.1) + gap));
                let end = (b.x - ed.0 * (b.reach(ed.0, ed.1) + gap), b.y - ed.1 * (b.reach(ed.0, ed.1) + gap));
                Some(Self { points: vec![start, ctrl, end], curved: true, end, dir: ed })
            }
            EdgeStyle::Straight => {
                let d = (dx / len, dy / len);
                let start = (a.x + d.0 * (a.reach(d.0, d.1) + gap), a.y + d.1 * (a.reach(d.0, d.1) + gap));
                let end = (b.x - d.0 * (b.reach(d.0, d.1) + gap), b.y - d.1 * (b.reach(d.0, d.1) + gap));
                Some(Self { points: vec![start, end], curved: false, end, dir: d })
            }
            EdgeStyle::Orthogonal => {
                // Leave and enter through the sides facing each other, with
                // one bend in the middle; vertical when mostly above/below.
                let horizontal = dx.abs() >= dy.abs() * 0.6;
                if blocked {
                    // Over the top, so the edge doesn't run through the
                    // nodes in between.
                    let top = (a.y - a.h / 2.0).min(b.y - b.h / 2.0) - 18.0 - (lane + 3.0) * 5.0;
                    let start = (a.x + lane * 2.0, a.y - a.h / 2.0 - gap);
                    let end = (b.x - lane * 2.0, b.y - b.h / 2.0 - gap);
                    return Some(Self {
                        points: vec![start, (start.0, top), (end.0, top), end],
                        curved: false,
                        end,
                        dir: (0.0, 1.0),
                    });
                }
                if horizontal {
                    let sx = dx.signum();
                    let start = (a.x + sx * (a.w / 2.0 + gap), a.y);
                    let end = (b.x - sx * (b.w / 2.0 + gap), b.y);
                    let mx = (start.0 + end.0) / 2.0 + lane * 6.0;
                    Some(Self {
                        points: vec![start, (mx, start.1), (mx, end.1), end],
                        curved: false,
                        end,
                        dir: (sx, 0.0),
                    })
                } else {
                    let sy = dy.signum();
                    let start = (a.x, a.y + sy * (a.h / 2.0 + gap));
                    let end = (b.x, b.y - sy * (b.h / 2.0 + gap));
                    let my = (start.1 + end.1) / 2.0 + lane * 6.0;
                    Some(Self {
                        points: vec![start, (start.0, my), (end.0, my), end],
                        curved: false,
                        end,
                        dir: (0.0, sy),
                    })
                }
            }
        }
    }

    fn path(&self, c: &CanvasRenderingContext2d, s: f64) {
        c.begin_path();
        let p = &self.points;
        c.move_to(p[0].0, p[0].1);
        if self.curved {
            c.quadratic_curve_to(p[1].0, p[1].1, p[2].0, p[2].1);
            return;
        }
        // Rounded corners on polylines.
        let corner = 10.0 / s.max(0.3);
        for i in 1..p.len() - 1 {
            let _ = c.arc_to(p[i].0, p[i].1, p[i + 1].0, p[i + 1].1, corner);
        }
        let last = p[p.len() - 1];
        c.line_to(last.0, last.1);
    }
}

/// Wires pointer/wheel events and starts the animation loop.
pub fn attach(scene: Rc<RefCell<Scene>>) {
    let canvas = scene.borrow().canvas.clone();

    let on = |name: &str, passive: bool, f: Box<dyn FnMut(web_sys::Event)>| {
        let cb = Closure::<dyn FnMut(web_sys::Event)>::wrap(f);
        let opts = web_sys::AddEventListenerOptions::new();
        opts.set_passive(passive);
        let _ = canvas.add_event_listener_with_callback_and_add_event_listener_options(
            name,
            cb.as_ref().unchecked_ref(),
            &opts,
        );
        cb.forget();
    };

    let s = scene.clone();
    on(
        "pointerdown",
        true,
        Box::new(move |e| {
            if let Some(e) = e.dyn_ref::<PointerEvent>() {
                s.borrow_mut().pointer_down(e);
            }
        }),
    );
    let s = scene.clone();
    on(
        "pointermove",
        true,
        Box::new(move |e| {
            if let Some(e) = e.dyn_ref::<PointerEvent>() {
                s.borrow_mut().pointer_move(e);
            }
        }),
    );
    for name in ["pointerup", "pointercancel"] {
        let s = scene.clone();
        on(
            name,
            true,
            Box::new(move |e| {
                let Some(e) = e.dyn_ref::<PointerEvent>() else { return };
                // Release the borrow before touching signals.
                let tapped = s.borrow_mut().pointer_up(e);
                if let Some(hit) = tapped {
                    let signal = s.borrow().on_select;
                    signal.set(hit);
                }
            }),
        );
    }
    let s = scene.clone();
    on(
        "pointerleave",
        true,
        Box::new(move |_| {
            let mut s = s.borrow_mut();
            s.hover = None;
            s.pointer = None;
        }),
    );
    // Right-drag pans in 3D; don't open the browser menu.
    on("contextmenu", false, Box::new(|e| e.prevent_default()));
    let s = scene.clone();
    on(
        "wheel",
        false,
        Box::new(move |e| {
            e.prevent_default();
            if let Some(e) = e.dyn_ref::<WheelEvent>() {
                s.borrow_mut().wheel(e);
            }
        }),
    );

    // requestAnimationFrame loop.
    type FrameCallback = Rc<RefCell<Option<Closure<dyn FnMut(f64)>>>>;
    let slot: FrameCallback = Rc::new(RefCell::new(None));
    let slot2 = slot.clone();
    *slot2.borrow_mut() = Some(Closure::new(move |ts: f64| {
        scene.borrow_mut().frame(ts);
        if let Some(cb) = slot.borrow().as_ref() {
            let _ = window().request_animation_frame(cb.as_ref().unchecked_ref());
        }
    }));
    if let Some(cb) = slot2.borrow().as_ref() {
        let _ = window().request_animation_frame(cb.as_ref().unchecked_ref());
    };
}
