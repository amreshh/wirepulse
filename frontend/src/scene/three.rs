//! 3D view: an orbit camera with perspective projection, drawn with the same
//! 2D canvas (no WebGL needed).
//!
//! - Map: namespaces lie on the ground where the 2D layout put them, and
//!   traffic arcs over them.
//! - Tower: each namespace is a floor, stacked in name order; traffic runs
//!   between floors.
//!
//! The world uses y as "up"; the 2D layout's (x, y) becomes (x, z).

use super::{lane, Scene, EDGE_DROP, EDGE_IDLE, EDGE_OK, EXTERNAL, FONT, HIGHLIGHT};
use crate::settings::LabelMode;
use crate::util::hue;
use common::aggregate::Level;
use common::layout::{Arrangement, IDLE_GROUP};
use common::{is_entity_group, NodeKind};
use std::collections::HashMap;
use std::f64::consts::TAU;

type V3 = (f64, f64, f64);

pub struct Cam {
    pub target: V3,
    /// Rotation around the vertical axis.
    pub yaw: f64,
    /// Tilt: 0 looks from the side, π/2 straight down.
    pub pitch: f64,
    /// Distance from the target.
    pub dist: f64,
}

impl Default for Cam {
    fn default() -> Self {
        Self { target: (0.0, 0.0, 0.0), yaw: -0.55, pitch: 0.82, dist: 2000.0 }
    }
}

/// A namespace drawn as a flat outline at height `y`.
pub struct Plate {
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub w: f64,
    pub d: f64,
    pub round: bool,
    pub members: usize,
}

struct Projected {
    x: f64,
    y: f64,
    /// Screen pixels per world unit at this depth.
    f: f64,
    depth: f64,
}

impl Scene {
    fn tower_active(&self) -> bool {
        self.input.tower
            && self.level() != Level::Namespace
            && self.input.style.arrangement == Arrangement::Packed
            && !self.layout.groups.is_empty()
    }

    /// World radius of a node's sphere.
    fn r3(&self, id: &str) -> f64 {
        self.layout.nodes.get(id).map_or(8.0, |p| if p.card { 12.0 } else { p.r() })
    }

    /// Places nodes and namespace plates in 3D from the current 2D layout.
    pub(super) fn build3d(&mut self) {
        self.pos3.clear();
        self.plates.clear();
        if self.tower_active() {
            // Floors in name order; (cluster), (external) and idle groups on top.
            let mut groups: Vec<_> = self.layout.groups.iter().collect();
            groups.sort_by_key(|g| (is_entity_group(&g.name) || g.name == IDLE_GROUP, g.name.clone()));
            let span = groups.iter().map(|g| g.w.max(g.h)).fold(0.0, f64::max);
            let tier = (span * 0.38).clamp(90.0, 420.0);
            let mut floor_of: HashMap<&str, (f64, f64, f64)> = HashMap::new();
            for (i, g) in groups.iter().enumerate() {
                let y = i as f64 * tier;
                floor_of.insert(g.name.as_str(), (g.x, y, g.y));
                self.plates.push(Plate {
                    name: g.name.clone(),
                    x: 0.0,
                    y,
                    z: 0.0,
                    w: g.w,
                    d: g.h,
                    round: !g.rect,
                    members: g.members,
                });
            }
            for (id, p) in &self.layout.nodes {
                let group = self.nodes.get(id).map(|n| n.group.as_str()).unwrap_or("");
                let (gx, gy, gz) = floor_of.get(group).copied().unwrap_or((0.0, 0.0, 0.0));
                let r = if p.card { 12.0 } else { p.r() };
                self.pos3.insert(id.clone(), (p.x - gx, gy + r, p.y - gz));
            }
        } else {
            for g in &self.layout.groups {
                self.plates.push(Plate {
                    name: g.name.clone(),
                    x: g.x,
                    y: 0.0,
                    z: g.y,
                    w: g.w,
                    d: g.h,
                    round: !g.rect,
                    members: g.members,
                });
            }
            for (id, p) in &self.layout.nodes {
                let r = if p.card { 12.0 } else { p.r() };
                self.pos3.insert(id.clone(), (p.x, r, p.y));
            }
        }
    }

    fn focal(&self) -> f64 {
        self.size.0.min(self.size.1) * 1.2
    }

    fn project(&self, p: V3) -> Option<Projected> {
        let c = &self.cam;
        let (x, y, z) = (p.0 - c.target.0, p.1 - c.target.1, p.2 - c.target.2);
        let (sy, cy) = c.yaw.sin_cos();
        let (sp, cp) = c.pitch.sin_cos();
        let x1 = x * cy - z * sy;
        let z1 = x * sy + z * cy;
        let y2 = cp * y + sp * z1;
        let z2 = -sp * y + cp * z1;
        let depth = c.dist + z2;
        if depth < 20.0 {
            return None;
        }
        let f = self.focal() / depth;
        Some(Projected { x: self.size.0 / 2.0 + x1 * f, y: self.size.1 / 2.0 - y2 * f, f, depth })
    }

    pub(super) fn fit3d(&mut self) {
        if self.pos3.is_empty() || self.size.0 <= 0.0 {
            return;
        }
        // Everything that should end up on screen.
        let mut pts: Vec<V3> = self.pos3.values().copied().collect();
        for p in &self.plates {
            let (hw, hd) = (p.w / 2.0, p.d / 2.0);
            for (dx, dz) in [(-hw, -hd), (hw, -hd), (hw, hd), (-hw, hd), (0.0, -hd), (0.0, hd), (-hw, 0.0), (hw, 0.0)] {
                pts.push((p.x + dx, p.y, p.z + dz));
            }
        }
        let n = pts.len() as f64;
        let c = pts.iter().fold((0.0, 0.0, 0.0), |a, p| (a.0 + p.0, a.1 + p.1, a.2 + p.2));
        self.cam.target = (c.0 / n, c.1 / n, c.2 / n);
        let r = pts
            .iter()
            .map(|p| {
                ((p.0 - self.cam.target.0).powi(2)
                    + (p.1 - self.cam.target.1).powi(2)
                    + (p.2 - self.cam.target.2).powi(2))
                .sqrt()
            })
            .fold(1.0, f64::max);
        self.cam.dist = r * 2.5;

        // Perspective makes the result depend on distance, so measure the
        // projected picture and correct a few times.
        let (w, h, _) = self.size;
        let pad_x = (w * 0.04).clamp(12.0, 60.0);
        let (pad_top, pad_bottom) = (40.0, 50.0);
        for _ in 0..4 {
            let proj: Vec<Projected> = pts.iter().filter_map(|p| self.project(*p)).collect();
            if proj.is_empty() {
                break;
            }
            let (x0, x1) = proj.iter().fold((f64::MAX, f64::MIN), |a, p| (a.0.min(p.x), a.1.max(p.x)));
            let (y0, y1) = proj.iter().fold((f64::MAX, f64::MIN), |a, p| (a.0.min(p.y), a.1.max(p.y)));
            let k = ((x1 - x0) / (w - 2.0 * pad_x)).max((y1 - y0) / (h - pad_top - pad_bottom));
            if k > 0.0 {
                self.cam.dist = (self.cam.dist * k).clamp(r * 0.7, 200_000.0);
            }
            let proj: Vec<Projected> = pts.iter().filter_map(|p| self.project(*p)).collect();
            let (x0, x1) = proj.iter().fold((f64::MAX, f64::MIN), |a, p| (a.0.min(p.x), a.1.max(p.x)));
            let (y0, y1) = proj.iter().fold((f64::MAX, f64::MIN), |a, p| (a.0.min(p.y), a.1.max(p.y)));
            let target_y = pad_top + (h - pad_top - pad_bottom) / 2.0;
            self.pan3(w / 2.0 - (x0 + x1) / 2.0, target_y - (y0 + y1) / 2.0);
        }
        self.needs_fit = false;
    }

    pub(super) fn orbit(&mut self, dx: f64, dy: f64) {
        self.cam.yaw += dx * 0.006;
        self.cam.pitch = (self.cam.pitch + dy * 0.005).clamp(0.08, 1.52);
    }

    /// Moves the target along the ground so the scene follows the pointer.
    pub(super) fn pan3(&mut self, dx: f64, dy: f64) {
        let f = self.focal() / self.cam.dist;
        let (sy, cy) = self.cam.yaw.sin_cos();
        let sp = self.cam.pitch.sin().max(0.25);
        let (rx, rz) = (cy, -sy);
        let (fx, fz) = (sy, cy);
        let t = &mut self.cam.target;
        t.0 -= rx * dx / f;
        t.2 -= rz * dx / f;
        t.0 += fx * dy / (f * sp);
        t.2 += fz * dy / (f * sp);
    }

    pub(super) fn zoom3(&mut self, factor: f64) {
        self.cam.dist = (self.cam.dist * factor).clamp(60.0, 200_000.0);
    }

    pub(super) fn hit3d(&self, sx: f64, sy: f64) -> Option<String> {
        // Nearest to the camera first.
        self.proj
            .borrow()
            .iter()
            .rev()
            .find(|(_, x, y, r)| (x - sx).hypot(y - sy) <= r + 4.0)
            .map(|(id, ..)| id.clone())
    }

    /// Re-centres on a newly selected node if the details panel would cover it.
    pub(super) fn reveal3d(&mut self) {
        let Some(id) = self.reveal.take() else { return };
        let Some(&p) = self.pos3.get(&id) else { return };
        let Some(s) = self.project(p) else {
            self.cam.target = p;
            return;
        };
        let (w, h, _) = self.size;
        let hidden = if w <= 760.0 { s.y > h * 0.42 } else { s.x > w - 370.0 };
        if hidden || s.x < 0.0 || s.y < 0.0 || s.x > w || s.y > h {
            self.cam.target = p;
        }
    }

    pub(super) fn draw3d(&self, t: f64) {
        let c = &self.ctx;
        let (w, h, dpr) = self.size;
        let _ = c.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0);
        c.set_fill_style_str(super::BG);
        c.fill_rect(0.0, 0.0, w, h);
        c.set_line_cap("round");
        c.set_line_join("round");
        if self.pos3.is_empty() {
            c.set_fill_style_str("#7d8590");
            c.set_font(&format!("14px {FONT}"));
            c.set_text_align("center");
            let _ = c.fill_text("Waiting for data…", w / 2.0, h / 2.0);
            self.proj.borrow_mut().clear();
            return;
        }

        // Project every node once.
        let mut nodes: Vec<(&String, Projected, f64)> =
            self.pos3.iter().filter_map(|(id, p)| self.project(*p).map(|s| (id, s, self.r3(id)))).collect();
        nodes.sort_by(|a, b| b.1.depth.total_cmp(&a.1.depth));
        let (dmin, dmax) =
            nodes.iter().fold((f64::MAX, f64::MIN), |(lo, hi), n| (lo.min(n.1.depth), hi.max(n.1.depth)));
        // Far things fade a little: depth cue.
        let fog = |depth: f64| {
            if dmax - dmin < 1.0 {
                1.0
            } else {
                1.0 - 0.5 * ((depth - dmin) / (dmax - dmin)).clamp(0.0, 1.0)
            }
        };

        self.draw_ground();
        self.draw_plates(&fog);
        self.draw_edges3d(t, &fog);

        let focus = self.focus_set();
        let dimming = self.dimming();
        let mut hits = Vec::with_capacity(nodes.len());
        let shaded = nodes.len() <= 800;
        for (id, s, r) in &nodes {
            let Some(n) = self.nodes.get(*id) else { continue };
            let sr = (r * s.f).max(1.5);
            hits.push(((*id).clone(), s.x, s.y, sr));
            let on = !dimming || self.lit(id, &focus);
            c.set_global_alpha(if on { fog(s.depth) } else { 0.15 * fog(s.depth) });
            let selected = self.input.selected.as_deref() == Some(id.as_str());

            if self.input.highlight.contains(*id) {
                c.begin_path();
                let _ = c.arc(s.x, s.y, sr + 5.0, 0.0, TAU);
                c.set_fill_style_str("rgba(245,197,66,0.30)");
                c.fill();
                c.set_stroke_style_str(HIGHLIGHT);
                c.set_line_width(1.5);
                c.stroke();
            }

            // Sphere: radial gradient lit from the top left.
            let (hue_deg, sat, light): (f64, f64, f64) = if n.kind == NodeKind::External {
                (220.0, 12.0, 60.0)
            } else {
                let active = n.flows_in + n.flows_out + n.internal_flows > 0.0;
                (f64::from(hue(&n.group)), if active { 60.0 } else { 25.0 }, if active { 52.0 } else { 38.0 })
            };
            c.begin_path();
            let _ = c.arc(s.x, s.y, sr, 0.0, TAU);
            if shaded && sr > 3.0 {
                if let Ok(g) = c.create_radial_gradient(s.x - sr * 0.35, s.y - sr * 0.4, sr * 0.1, s.x, s.y, sr) {
                    let _ = g.add_color_stop(0.0, &format!("hsl({hue_deg},{sat}%,{}%)", (light + 22.0).min(92.0)));
                    let _ = g.add_color_stop(0.55, &format!("hsl({hue_deg},{sat}%,{light}%)"));
                    let _ = g.add_color_stop(1.0, &format!("hsl({hue_deg},{sat}%,{}%)", (light - 18.0).max(8.0)));
                    c.set_fill_style_canvas_gradient(&g);
                }
            } else if n.kind == NodeKind::External {
                c.set_fill_style_str(EXTERNAL);
            } else {
                c.set_fill_style_str(&format!("hsl({hue_deg},{sat}%,{light}%)"));
            }
            c.fill();
            if selected || n.drops > 0.0 {
                c.set_line_width(if selected { 2.5 } else { 2.0 });
                c.set_stroke_style_str(if selected { "#ffffff" } else { EDGE_DROP });
                c.stroke();
            }
            if self.input.icons && sr >= 8.0 {
                self.draw_icon(n, s.x, s.y, sr * 1.1, "rgba(255,255,255,0.92)");
            }

            let auto = match self.level() {
                Level::Namespace => sr >= 6.0,
                Level::Workload => sr >= 7.0,
                Level::Pod => sr >= 10.0,
            };
            let show = match self.input.labels {
                LabelMode::Always => true,
                LabelMode::Auto => auto,
                LabelMode::Off => false,
            } || selected
                || self.hover.as_deref() == Some(id.as_str());
            if show {
                c.set_font(&format!("11px {FONT}"));
                c.set_text_align("center");
                c.set_text_baseline("top");
                c.set_fill_style_str("rgba(230,237,243,0.92)");
                let mut label = super::truncate(&n.label, 26);
                if self.level() != Level::Pod && n.members > 1 {
                    label = format!("{label} ×{}", n.members);
                }
                let _ = c.fill_text(&label, s.x, s.y + sr + 3.0);
            }
        }
        c.set_global_alpha(1.0);
        *self.proj.borrow_mut() = hits;
        self.draw_tooltip();
    }

    /// A faint grid on the ground for orientation.
    fn draw_ground(&self) {
        let c = &self.ctx;
        let (mut x0, mut z0, mut x1, mut z1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for p in self.pos3.values() {
            x0 = x0.min(p.0);
            x1 = x1.max(p.0);
            z0 = z0.min(p.2);
            z1 = z1.max(p.2);
        }
        for p in &self.plates {
            x0 = x0.min(p.x - p.w / 2.0);
            x1 = x1.max(p.x + p.w / 2.0);
            z0 = z0.min(p.z - p.d / 2.0);
            z1 = z1.max(p.z + p.d / 2.0);
        }
        let pad = 120.0;
        let (x0, x1, z0, z1) = (x0 - pad, x1 + pad, z0 - pad, z1 + pad);
        let step = (((x1 - x0).max(z1 - z0)) / 16.0).max(40.0);
        let y = if self.tower_active() { -30.0 } else { 0.0 };
        c.set_stroke_style_str("rgba(110,118,129,0.14)");
        c.set_line_width(1.0);
        c.begin_path();
        let line = |a: V3, b: V3| {
            if let (Some(p), Some(q)) = (self.project(a), self.project(b)) {
                c.move_to(p.x, p.y);
                c.line_to(q.x, q.y);
            }
        };
        let mut x = x0;
        while x <= x1 {
            line((x, y, z0), (x, y, z1));
            x += step;
        }
        let mut z = z0;
        while z <= z1 {
            line((x0, y, z), (x1, y, z));
            z += step;
        }
        c.stroke();
    }

    fn draw_plates(&self, fog: &dyn Fn(f64) -> f64) {
        let c = &self.ctx;
        let mut plates: Vec<(&Plate, f64)> =
            self.plates.iter().filter_map(|p| self.project((p.x, p.y, p.z)).map(|s| (p, s.depth))).collect();
        plates.sort_by(|a, b| b.1.total_cmp(&a.1));
        for (p, depth) in plates {
            let outline: Vec<V3> = if p.round {
                (0..48)
                    .map(|i| {
                        let a = i as f64 / 48.0 * TAU;
                        (p.x + p.w / 2.0 * a.cos(), p.y, p.z + p.d / 2.0 * a.sin())
                    })
                    .collect()
            } else {
                let (hw, hd) = (p.w / 2.0, p.d / 2.0);
                vec![
                    (p.x - hw, p.y, p.z - hd),
                    (p.x + hw, p.y, p.z - hd),
                    (p.x + hw, p.y, p.z + hd),
                    (p.x - hw, p.y, p.z + hd),
                ]
            };
            let pts: Option<Vec<Projected>> = outline.iter().map(|q| self.project(*q)).collect();
            let Some(pts) = pts else { continue };
            let neutral = is_entity_group(&p.name) || p.name == IDLE_GROUP;
            let h = hue(&p.name);
            c.set_global_alpha(fog(depth));
            c.begin_path();
            c.move_to(pts[0].x, pts[0].y);
            for q in &pts[1..] {
                c.line_to(q.x, q.y);
            }
            c.close_path();
            if neutral {
                c.set_fill_style_str("rgba(138,148,166,0.06)");
                c.set_stroke_style_str("rgba(138,148,166,0.4)");
            } else {
                c.set_fill_style_str(&format!("hsla({h},45%,50%,0.09)"));
                c.set_stroke_style_str(&format!("hsla({h},50%,60%,0.5)"));
            }
            c.fill();
            c.set_line_width(1.2);
            c.stroke();

            // Name at the plate's highest point on screen.
            if let Some(top) = pts.iter().min_by(|a, b| a.y.total_cmp(&b.y)) {
                c.set_font(&format!("600 12px {FONT}"));
                c.set_text_align("center");
                c.set_text_baseline("bottom");
                c.set_fill_style_str(if neutral { "rgba(180,188,200,0.85)" } else { "rgba(220,226,235,0.9)" });
                let _ = c.fill_text(&format!("{} · {}", p.name, p.members), top.x, top.y - 4.0);
            }
        }
        c.set_global_alpha(1.0);
    }

    fn draw_edges3d(&self, t: f64, fog: &dyn Fn(f64) -> f64) {
        let c = &self.ctx;
        let focus = self.focus_set();
        let animate = self.input.animate && self.input.agg.edges.len() <= 3000;
        let moving = super::dash(3.0, 9.0);
        let solid = js_sys::Array::new();
        let tower = self.tower_active();
        const SAMPLES: usize = 20;

        for e in &self.input.agg.edges {
            if !self.edge_visible(e) {
                continue;
            }
            let (Some(&a), Some(&b)) = (self.pos3.get(&e.source), self.pos3.get(&e.target)) else {
                continue;
            };
            let (dx, dz) = (b.0 - a.0, b.2 - a.2);
            let len = (dx * dx + (b.1 - a.1).powi(2) + dz * dz).sqrt();
            if len < 1e-6 {
                continue;
            }
            // Control point: lifted arcs on the map; bowed sideways in the
            // tower so edges between floors fan out around it.
            let mid = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0, (a.2 + b.2) / 2.0);
            let ln = lane(&e.source, &e.target);
            let ctrl = if tower {
                let ang = ln * 0.9 + 0.4;
                let out = len * 0.22 + 40.0;
                (mid.0 + ang.cos() * out, mid.1, mid.2 + ang.sin() * out)
            } else {
                let hl = (dx * dx + dz * dz).sqrt().max(1e-6);
                let side = hl * 0.08;
                (mid.0 - dz / hl * side, mid.1 + (hl * 0.3).clamp(30.0, 900.0), mid.2 + dx / hl * side)
            };
            let mut pts = Vec::with_capacity(SAMPLES + 1);
            for i in 0..=SAMPLES {
                let u = i as f64 / SAMPLES as f64;
                let v = 1.0 - u;
                let p = (
                    v * v * a.0 + 2.0 * v * u * ctrl.0 + u * u * b.0,
                    v * v * a.1 + 2.0 * v * u * ctrl.1 + u * u * b.1,
                    v * v * a.2 + 2.0 * v * u * ctrl.2 + u * u * b.2,
                );
                match self.project(p) {
                    Some(s) => pts.push(s),
                    None => break,
                }
            }
            if pts.len() <= SAMPLES {
                continue;
            }
            // Trim the ends to the spheres' outlines.
            let (ra, rb) = (self.r3(&e.source) * pts[0].f, self.r3(&e.target) * pts[SAMPLES].f);
            let (sa, sb) = ((pts[0].x, pts[0].y), (pts[SAMPLES].x, pts[SAMPLES].y));
            let first = pts.iter().position(|p| (p.x - sa.0).hypot(p.y - sa.1) > ra + 2.0).unwrap_or(0);
            let last = pts.iter().rposition(|p| (p.x - sb.0).hypot(p.y - sb.1) > rb + 2.0).unwrap_or(SAMPLES);
            if last <= first {
                continue;
            }
            let pts = &pts[first..=last];

            let in_focus = match (&focus, self.input.selected.as_deref()) {
                (Some(_), Some(sel)) => e.source == sel || e.target == sel,
                _ => true,
            };
            let in_hl = self.input.highlight.is_empty()
                || self.input.highlight.contains(&e.source)
                || self.input.highlight.contains(&e.target);
            let depth = pts.iter().map(|p| p.depth).sum::<f64>() / pts.len() as f64;
            let alpha = if in_focus && in_hl { fog(depth) } else { 0.06 };

            let rate = f64::from(e.flows_per_sec + e.drops_per_sec);
            let color = if e.drops_per_sec > 0.0 {
                EDGE_DROP
            } else if rate > 0.0 {
                EDGE_OK
            } else {
                EDGE_IDLE
            };
            let width = (1.0 + (1.0 + rate).log2() * 1.1).min(7.0);
            let path = || {
                c.begin_path();
                c.move_to(pts[0].x, pts[0].y);
                for p in &pts[1..] {
                    c.line_to(p.x, p.y);
                }
            };
            c.set_global_alpha(alpha * if rate > 0.0 { 0.45 } else { 0.35 });
            c.set_stroke_style_str(color);
            c.set_line_width(width);
            let _ = c.set_line_dash(&solid);
            path();
            c.stroke();
            if animate && rate > 0.0 && alpha > 0.3 {
                let speed = 18.0 + 14.0 * (1.0 + rate).log2();
                let _ = c.set_line_dash(&moving);
                c.set_line_dash_offset(-(t * speed) % 12.0);
                c.set_global_alpha(alpha * 0.95);
                c.set_line_width((width * 0.8).max(1.5));
                path();
                c.stroke();
                let _ = c.set_line_dash(&solid);
            }
            // Arrowhead along the last segment.
            let (p, q) = (&pts[pts.len() - 2], &pts[pts.len() - 1]);
            let l = (q.x - p.x).hypot(q.y - p.y).max(1e-6);
            let (ux, uy) = ((q.x - p.x) / l, (q.y - p.y) / l);
            let size = 5.0 + width;
            c.set_global_alpha(alpha * 0.85);
            c.set_fill_style_str(color);
            c.begin_path();
            c.move_to(q.x, q.y);
            c.line_to(q.x - ux * size - uy * size * 0.5, q.y - uy * size + ux * size * 0.5);
            c.line_to(q.x - ux * size + uy * size * 0.5, q.y - uy * size - ux * size * 0.5);
            c.close_path();
            c.fill();
        }
        c.set_global_alpha(1.0);
        let _ = c.set_line_dash(&solid);
    }
}
