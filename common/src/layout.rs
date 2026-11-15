//! Deterministic layouts for the global map.
//!
//! Force-directed layouts jump around as pods come and go, which makes a live
//! view hard to read. Every layout here is a pure function of the graph: the
//! same input always gives the same picture, and a new pod only shifts its
//! own namespace (packed) or its own column (layered).
//!
//! Three independent choices ([`LayoutStyle`]):
//! - node: circles, or Hubble-style cards (rounded rectangles with text inside)
//! - container: namespaces as circles (members on a sunflower spiral) or as
//!   boxes (members in a grid); cards always use boxes
//! - arrangement: namespaces packed in rows, or a left-to-right layered view
//!   that follows the direction of traffic

use crate::aggregate::{AggGraph, AggNode, Level};
use crate::model::{NodeKind, EXTERNAL_GROUP};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

const GOLDEN_ANGLE: f64 = 2.399_963_229_728_653;
const NODE_GAP: f64 = 26.0;
const GROUP_PAD: f64 = 28.0;
const GROUP_GAP: f64 = 36.0;
/// Space for the namespace name at the top of a box.
const BOX_HEADER: f64 = 30.0;
const BOX_PAD: f64 = 14.0;
/// Pseudo-group holding nodes without traffic in the layered view.
pub const IDLE_GROUP: &str = "no traffic";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeStyle {
    #[default]
    Circle,
    Card,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Container {
    #[default]
    Circle,
    Box,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Arrangement {
    #[default]
    Packed,
    Layered,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LayoutStyle {
    pub node: NodeStyle,
    pub container: Container,
    pub arrangement: Arrangement,
}

impl LayoutStyle {
    /// Cards don't fit on a spiral, so they always sit in boxes.
    pub fn effective_container(self) -> Container {
        match self.node {
            NodeStyle::Card => Container::Box,
            NodeStyle::Circle => self.container,
        }
    }
}

/// A placed node; (x, y) is the centre.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placed {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub card: bool,
}

impl Placed {
    /// Radius for circles; half the shorter side for cards.
    pub fn r(&self) -> f64 {
        self.w.min(self.h) / 2.0
    }

    pub fn contains(&self, px: f64, py: f64, slop: f64) -> bool {
        if self.card {
            (px - self.x).abs() <= self.w / 2.0 + slop && (py - self.y).abs() <= self.h / 2.0 + slop
        } else {
            (px - self.x).hypot(py - self.y) <= self.r() + slop
        }
    }

    /// Distance from the centre to the outline along unit direction (dx, dy).
    pub fn reach(&self, dx: f64, dy: f64) -> f64 {
        if !self.card {
            return self.r();
        }
        let tx = if dx.abs() < 1e-9 { f64::MAX } else { self.w / 2.0 / dx.abs() };
        let ty = if dy.abs() < 1e-9 { f64::MAX } else { self.h / 2.0 / dy.abs() };
        tx.min(ty)
    }
}

/// A namespace container; (x, y) is the centre.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupShape {
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// Box (true) or circle (false; then w == h == diameter).
    pub rect: bool,
    pub members: usize,
}

impl GroupShape {
    pub fn r(&self) -> f64 {
        self.w / 2.0
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layout {
    pub nodes: BTreeMap<String, Placed>,
    pub groups: Vec<GroupShape>,
    /// Bounding box (min_x, min_y, max_x, max_y).
    pub bounds: (f64, f64, f64, f64),
}

/// Circle radius of a node; grows slowly with the number of pods folded in.
pub fn node_radius(n: &AggNode, level: Level) -> f64 {
    let m = n.members.max(1) as f64;
    match (level, n.kind) {
        (_, NodeKind::External) => 11.0,
        (Level::Pod, _) => 8.0,
        (Level::Workload, _) => 8.0 + 3.0 * m.log2(),
        (Level::Namespace, _) => 14.0 + 3.0 * m.sqrt(),
    }
}

/// Width and height a node occupies.
pub fn node_size(n: &AggNode, level: Level, style: NodeStyle) -> (f64, f64) {
    match style {
        NodeStyle::Circle => {
            let d = 2.0 * node_radius(n, level);
            (d, d)
        }
        NodeStyle::Card => match level {
            Level::Pod => (200.0, 40.0),
            Level::Workload => (176.0, 46.0),
            Level::Namespace => (180.0, 52.0),
        },
    }
}

/// Grid cell for a node: room for the label under circles.
fn cell(w: f64, h: f64, style: NodeStyle) -> (f64, f64) {
    match style {
        NodeStyle::Circle => (w.max(h) + 74.0, w.max(h) + 36.0),
        NodeStyle::Card => (w + 24.0, h + 22.0),
    }
}

fn placed(x: f64, y: f64, (w, h): (f64, f64), style: NodeStyle) -> Placed {
    Placed { x, y, w, h, card: style == NodeStyle::Card }
}

/// `aspect` is the viewport's width / height; rows are packed to roughly
/// match it, so phones in portrait get a tall layout.
pub fn layout(g: &AggGraph, level: Level, aspect: f64, style: LayoutStyle) -> Layout {
    let aspect = if aspect.is_finite() { aspect.clamp(0.3, 4.0) } else { 1.6 };
    let mut out = match style.arrangement {
        Arrangement::Layered => layered(g, level, aspect, style.node),
        Arrangement::Packed if level == Level::Namespace => flat(g, level, aspect, style),
        Arrangement::Packed => grouped(g, level, aspect, style),
    };
    centre(&mut out);
    out
}

// ---- packed: namespace level -------------------------------------------------

fn flat(g: &AggGraph, level: Level, aspect: f64, style: LayoutStyle) -> Layout {
    let mut nodes = BTreeMap::new();
    let mut inner: Vec<&AggNode> = g.nodes.iter().filter(|n| n.kind == NodeKind::Pod).collect();
    inner.sort_by(|a, b| a.label.cmp(&b.label));
    let mut ext: Vec<&AggNode> = g.nodes.iter().filter(|n| n.kind == NodeKind::External).collect();
    ext.sort_by(|a, b| a.id.cmp(&b.id));

    if style.effective_container() == Container::Circle {
        // One spiral of namespaces; reserved entities on an outer ring.
        let items: Vec<(&str, f64)> = inner.iter().map(|n| (n.id.as_str(), node_radius(n, level) + 24.0)).collect();
        let (spiral, extent) = sunflower(&items);
        for (id, x, y, r) in spiral {
            let d = 2.0 * (r - 24.0);
            nodes.insert(id, placed(x, y, (d, d), NodeStyle::Circle));
        }
        let ring = extent + 60.0;
        for (i, n) in ext.iter().enumerate() {
            let a = (i as f64 / ext.len() as f64) * std::f64::consts::TAU - std::f64::consts::FRAC_PI_2;
            nodes.insert(
                n.id.clone(),
                placed(ring * a.cos(), ring * a.sin(), node_size(n, level, NodeStyle::Circle), NodeStyle::Circle),
            );
        }
        return Layout { nodes, ..Default::default() };
    }

    // Grid of namespaces, then a row of external entities underneath.
    let sizes: HashMap<&str, (f64, f64)> =
        g.nodes.iter().map(|n| (n.id.as_str(), node_size(n, level, style.node))).collect();
    let (_, gh, inner_pos) = grid(&inner, level, style.node, aspect);
    for (id, x, y) in inner_pos {
        let size = sizes[id.as_str()];
        nodes.insert(id, placed(x, y, size, style.node));
    }
    if !ext.is_empty() {
        // A single row.
        let (_, eh, ext_pos) = grid(&ext, level, style.node, 100.0);
        for (id, x, y) in ext_pos {
            let size = sizes[id.as_str()];
            nodes.insert(id, placed(x, y + gh / 2.0 + eh / 2.0 + 30.0, size, style.node));
        }
    }
    Layout { nodes, ..Default::default() }
}

/// Lays `items` out in a grid whose overall shape roughly matches `aspect`.
/// Returns width, height and centre-relative positions.
fn grid(items: &[&AggNode], level: Level, style: NodeStyle, aspect: f64) -> (f64, f64, Vec<(String, f64, f64)>) {
    if items.is_empty() {
        return (0.0, 0.0, Vec::new());
    }
    let max =
        items.iter().map(|n| node_size(n, level, style)).fold((0.0_f64, 0.0_f64), |a, s| (a.0.max(s.0), a.1.max(s.1)));
    let (cw, ch) = cell(max.0, max.1, style);
    let n = items.len();
    let cols = ((n as f64 * aspect * ch / cw).sqrt().round() as usize).clamp(1, n);
    let rows = n.div_ceil(cols);
    let (w, h) = (cols as f64 * cw, rows as f64 * ch);
    let pos = items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            let (c, r) = (i % cols, i / cols);
            (it.id.clone(), (c as f64 + 0.5) * cw - w / 2.0, (r as f64 + 0.5) * ch - h / 2.0)
        })
        .collect();
    (w, h, pos)
}

// ---- packed: workload / pod level ---------------------------------------------

struct Pending {
    name: String,
    w: f64,
    h: f64,
    rect: bool,
    /// Centre-relative member positions.
    members: Vec<(String, Placed)>,
}

/// Places `items` (id, radius) on a sunflower spiral centred on (0,0).
/// Returns (id, x, y, r) and the radius of the enclosing circle.
fn sunflower(items: &[(&str, f64)]) -> (Vec<(String, f64, f64, f64)>, f64) {
    if items.is_empty() {
        return (Vec::new(), GROUP_PAD);
    }
    let max_r = items.iter().map(|i| i.1).fold(0.0, f64::max);
    if items.len() == 1 {
        let (id, r) = items[0];
        return (vec![(id.to_string(), 0.0, 0.0, r)], r + GROUP_PAD);
    }
    let spacing = 2.0 * max_r + NODE_GAP;
    // Each point of a sunflower spiral covers about pi*c^2 of area.
    let c = spacing / std::f64::consts::PI.sqrt() * 1.08;
    let mut out = Vec::with_capacity(items.len());
    let mut extent: f64 = 0.0;
    for (i, (id, r)) in items.iter().enumerate() {
        let rad = c * (i as f64 + 0.5).sqrt();
        let theta = i as f64 * GOLDEN_ANGLE;
        extent = extent.max(rad + r);
        out.push((id.to_string(), rad * theta.cos(), rad * theta.sin(), *r));
    }
    (out, extent + GROUP_PAD)
}

fn grouped(g: &AggGraph, level: Level, aspect: f64, style: LayoutStyle) -> Layout {
    let container = style.effective_container();
    let mut by_group: BTreeMap<(bool, String), Vec<&AggNode>> = BTreeMap::new();
    for n in &g.nodes {
        let is_ext = n.group == EXTERNAL_GROUP;
        by_group.entry((is_ext, n.group.clone())).or_default().push(n);
    }

    let mut pending = Vec::new();
    for ((_, name), mut members) in by_group {
        members.sort_by(|a, b| a.id.cmp(&b.id));
        pending.push(match container {
            Container::Circle => {
                let items: Vec<(&str, f64)> = members.iter().map(|n| (n.id.as_str(), node_radius(n, level))).collect();
                let (spiral, r) = sunflower(&items);
                Pending {
                    name,
                    w: 2.0 * r,
                    h: 2.0 * r,
                    rect: false,
                    members: spiral
                        .into_iter()
                        .map(|(id, x, y, nr)| (id, placed(x, y, (2.0 * nr, 2.0 * nr), NodeStyle::Circle)))
                        .collect(),
                }
            }
            Container::Box => {
                let (gw, gh, pos) = grid(&members, level, style.node, 1.6);
                let (w, h) = (gw + 2.0 * BOX_PAD, gh + BOX_HEADER + BOX_PAD);
                let sizes: HashMap<&str, (f64, f64)> =
                    members.iter().map(|n| (n.id.as_str(), node_size(n, level, style.node))).collect();
                Pending {
                    name,
                    w: w.max(150.0),
                    h,
                    rect: true,
                    members: pos
                        .into_iter()
                        .map(|(id, x, y)| {
                            // Shift down by half the header, since it sits on top.
                            let size = sizes[id.as_str()];
                            let p = placed(x, y + (BOX_HEADER - BOX_PAD) / 2.0, size, style.node);
                            (id, p)
                        })
                        .collect(),
                }
            }
        });
    }

    // Shelf packing in name order. Uneven sizes make the result's shape hard
    // to predict, so try several row widths and keep the one whose bounding
    // box best matches the viewport.
    let sizes: Vec<(f64, f64)> = pending.iter().map(|p| (p.w, p.h)).collect();
    let total_area: f64 = sizes.iter().map(|(w, h)| (w + GROUP_GAP) * (h + GROUP_GAP)).sum();
    let max_w = sizes.iter().map(|s| s.0).fold(0.0, f64::max);
    let base = (total_area * aspect).sqrt();
    let score = |(_, w, h): &(Vec<(f64, f64)>, f64, f64)| ((w / h.max(1.0)).ln() - aspect.ln()).abs();
    let (centres, _, _) = [0.7, 0.85, 1.0, 1.2, 1.45, 1.75, 2.1, 2.5]
        .iter()
        .map(|f| shelf_pack(&sizes, (base * f).max(max_w)))
        .min_by(|a, b| score(a).total_cmp(&score(b)))
        .unwrap_or_default();

    let mut nodes = BTreeMap::new();
    let mut groups = Vec::new();
    for (p, (cx, cy)) in pending.into_iter().zip(centres) {
        let count = p.members.len();
        for (id, m) in p.members {
            nodes.insert(id, Placed { x: cx + m.x, y: cy + m.y, ..m });
        }
        groups.push(GroupShape { name: p.name, x: cx, y: cy, w: p.w, h: p.h, rect: p.rect, members: count });
    }
    Layout { nodes, groups, bounds: (0.0, 0.0, 0.0, 0.0) }
}

/// Places boxes left to right in rows no wider than `max_w`, aligned to the
/// top of each row. Returns the centres plus the overall width and height.
fn shelf_pack(sizes: &[(f64, f64)], max_w: f64) -> (Vec<(f64, f64)>, f64, f64) {
    let mut rows: Vec<Vec<usize>> = vec![Vec::new()];
    let mut x = 0.0;
    for (i, (w, _)) in sizes.iter().enumerate() {
        if x + w > max_w && !rows.last().unwrap().is_empty() {
            rows.push(Vec::new());
            x = 0.0;
        }
        rows.last_mut().unwrap().push(i);
        x += w + GROUP_GAP;
    }
    let mut centres = vec![(0.0, 0.0); sizes.len()];
    let (mut y, mut width) = (0.0_f64, 0.0_f64);
    for row in rows {
        let h = row.iter().map(|&i| sizes[i].1).fold(0.0, f64::max);
        let mut x = 0.0;
        for i in row {
            let (w, bh) = sizes[i];
            // Circles are centred in the row; boxes hang from its top edge.
            let cy = if (w - bh).abs() < 1e-9 { y + h / 2.0 } else { y + bh / 2.0 };
            centres[i] = (x + w / 2.0, cy);
            x += w + GROUP_GAP;
        }
        width = width.max(x - GROUP_GAP);
        y += h + GROUP_GAP;
    }
    (centres, width, (y - GROUP_GAP).max(0.0))
}

// ---- layered ------------------------------------------------------------------

/// Columns that follow the traffic: sources on the left, sinks on the right.
/// Cycles are broken by a depth-first search in name order; nodes without
/// traffic go into a grid underneath.
fn layered(g: &AggGraph, level: Level, aspect: f64, style: NodeStyle) -> Layout {
    let mut order: Vec<usize> = (0..g.nodes.len()).collect();
    order.sort_by(|&a, &b| {
        let (na, nb) = (&g.nodes[a], &g.nodes[b]);
        (&na.group, &na.label, &na.id).cmp(&(&nb.group, &nb.label, &nb.id))
    });
    let index: HashMap<&str, usize> = g.nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();

    let n = g.nodes.len();
    let mut out_adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut connected = vec![false; n];
    for e in &g.edges {
        if let (Some(&s), Some(&t)) = (index.get(e.source.as_str()), index.get(e.target.as_str())) {
            if s != t {
                out_adj[s].push(t);
                connected[s] = true;
                connected[t] = true;
            }
        }
    }
    let mut rank = vec![0usize; n];
    for (i, &v) in order.iter().enumerate() {
        rank[v] = i;
    }
    for adj in &mut out_adj {
        adj.sort_by_key(|&v| rank[v]);
        adj.dedup();
    }

    // Iterative DFS; edges to a node still on the stack are back edges.
    let mut state = vec![0u8; n]; // 0 new, 1 on stack, 2 done
    let mut kept: Vec<(usize, usize)> = Vec::new();
    for &root in &order {
        if state[root] != 0 || !connected[root] {
            continue;
        }
        let mut stack: Vec<(usize, usize)> = vec![(root, 0)];
        state[root] = 1;
        while let Some(&mut (v, ref mut next)) = stack.last_mut() {
            if *next < out_adj[v].len() {
                let w = out_adj[v][*next];
                *next += 1;
                match state[w] {
                    0 => {
                        kept.push((v, w));
                        state[w] = 1;
                        stack.push((w, 0));
                    }
                    2 => kept.push((v, w)),
                    _ => {} // back edge: ignored for layering
                }
            } else {
                state[v] = 2;
                stack.pop();
            }
        }
    }

    // Longest-path layering over the acyclic edges.
    let mut indeg = vec![0usize; n];
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut pred: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(s, t) in &kept {
        indeg[t] += 1;
        succ[s].push(t);
        pred[t].push(s);
    }
    let mut layer = vec![0usize; n];
    let mut queue: std::collections::VecDeque<usize> =
        order.iter().copied().filter(|&v| connected[v] && indeg[v] == 0).collect();
    while let Some(v) = queue.pop_front() {
        for &w in &succ[v] {
            layer[w] = layer[w].max(layer[v] + 1);
            indeg[w] -= 1;
            if indeg[w] == 0 {
                queue.push_back(w);
            }
        }
    }

    let layers = order.iter().filter(|&&v| connected[v]).map(|&v| layer[v] + 1).max().unwrap_or(0);
    let mut cols: Vec<Vec<usize>> = vec![Vec::new(); layers];
    for &v in &order {
        if connected[v] {
            cols[layer[v]].push(v);
        }
    }

    // Barycentre sweeps to reduce crossings.
    let mut pos = vec![0.0f64; n];
    let set_pos = |cols: &Vec<Vec<usize>>, pos: &mut Vec<f64>| {
        for c in cols {
            for (i, &v) in c.iter().enumerate() {
                pos[v] = i as f64 - (c.len() as f64 - 1.0) / 2.0;
            }
        }
    };
    set_pos(&cols, &mut pos);
    for sweep in 0..6 {
        let forward = sweep % 2 == 0;
        let range: Vec<usize> =
            if forward { (1..layers).collect() } else { (0..layers.saturating_sub(1)).rev().collect() };
        for l in range {
            let neighbours = if forward { &pred } else { &succ };
            let mut keyed: Vec<(f64, usize)> = cols[l]
                .iter()
                .map(|&v| {
                    let ns = &neighbours[v];
                    let key =
                        if ns.is_empty() { pos[v] } else { ns.iter().map(|&u| pos[u]).sum::<f64>() / ns.len() as f64 };
                    (key, v)
                })
                .collect();
            keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
            cols[l] = keyed.into_iter().map(|(_, v)| v).collect();
            set_pos(&cols, &mut pos);
        }
    }

    let sizes: Vec<(f64, f64)> = g.nodes.iter().map(|nd| node_size(nd, level, style)).collect();
    let max = sizes.iter().fold((0.0_f64, 0.0_f64), |a, s| (a.0.max(s.0), a.1.max(s.1)));
    let (cw, ch) = cell(max.0, max.1, style);
    let col_w = cw + if style == NodeStyle::Card { 70.0 } else { 60.0 };

    let mut nodes = BTreeMap::new();
    let mut max_y: f64 = 0.0;
    for (l, c) in cols.iter().enumerate() {
        let x = l as f64 * col_w;
        for (i, &v) in c.iter().enumerate() {
            let y = (i as f64 - (c.len() as f64 - 1.0) / 2.0) * ch;
            max_y = max_y.max(y + ch / 2.0);
            nodes.insert(g.nodes[v].id.clone(), placed(x, y, sizes[v], style));
        }
    }

    // Idle nodes: a box underneath, as wide as the columns.
    let mut groups = Vec::new();
    let idle: Vec<&AggNode> = order.iter().filter(|&&v| !connected[v]).map(|&v| &g.nodes[v]).collect();
    if !idle.is_empty() {
        // About as wide as the columns above (grid() picks cols from aspect).
        let width = ((layers.max(1) - 1) as f64 * col_w + cw).max(cw * 3.0);
        let target = if layers == 0 { aspect } else { width * width / (idle.len() as f64 * cw * ch) };
        let (gw, gh, pos) = grid(&idle, level, style, target.max(0.05));
        let cx = if layers == 0 { 0.0 } else { (layers as f64 - 1.0) * col_w / 2.0 };
        let top = if layers == 0 { 0.0 } else { max_y + 50.0 };
        let (bw, bh) = (gw + 2.0 * BOX_PAD, gh + BOX_HEADER + BOX_PAD);
        let cy = top + bh / 2.0;
        for (id, x, y) in pos {
            let size = sizes[index[id.as_str()]];
            nodes.insert(id, placed(cx + x, cy + y + (BOX_HEADER - BOX_PAD) / 2.0, size, style));
        }
        groups.push(GroupShape {
            name: IDLE_GROUP.into(),
            x: cx,
            y: cy,
            w: bw,
            h: bh,
            rect: true,
            members: idle.len(),
        });
    }
    Layout { nodes, groups, bounds: (0.0, 0.0, 0.0, 0.0) }
}

// ---- helpers ------------------------------------------------------------------

/// Moves the layout so its bounding box is centred on the origin and fills
/// in `bounds`.
fn centre(l: &mut Layout) {
    let (x0, y0, x1, y1) = bounds_of(&l.nodes, &l.groups);
    let (dx, dy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    for p in l.nodes.values_mut() {
        p.x -= dx;
        p.y -= dy;
    }
    for g in &mut l.groups {
        g.x -= dx;
        g.y -= dy;
    }
    l.bounds = bounds_of(&l.nodes, &l.groups);
}

fn bounds_of(nodes: &BTreeMap<String, Placed>, groups: &[GroupShape]) -> (f64, f64, f64, f64) {
    let mut b = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    let mut grow = |x: f64, y: f64, hw: f64, hh: f64| {
        b.0 = b.0.min(x - hw);
        b.1 = b.1.min(y - hh);
        b.2 = b.2.max(x + hw);
        b.3 = b.3.max(y + hh);
    };
    for p in nodes.values() {
        grow(p.x, p.y, p.w / 2.0, p.h / 2.0);
    }
    for g in groups {
        grow(g.x, g.y, g.w / 2.0, g.h / 2.0);
    }
    if b.0 > b.2 {
        (0.0, 0.0, 0.0, 0.0)
    } else {
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregate::aggregate;
    use crate::model::{Edge, Graph, Node};

    fn graph(namespaces: usize, pods: usize) -> Graph {
        let mut nodes = Vec::new();
        for n in 0..namespaces {
            for p in 0..pods * (n + 1) {
                nodes.push(Node {
                    id: format!("ns{n}/pod-{p}"),
                    kind: NodeKind::Pod,
                    namespace: format!("ns{n}"),
                    name: format!("pod-{p}"),
                    workload: format!("Deployment/wl{}", p % 3),
                    node_name: String::new(),
                    ip: String::new(),
                    phase: String::new(),
                });
            }
        }
        Graph { nodes, edges: vec![], generated_at_ms: 0 }
    }

    fn edge(s: &str, t: &str) -> Edge {
        Edge {
            source: s.into(),
            target: t.into(),
            flows_per_sec: 1.0,
            drops_per_sec: 0.0,
            total_flows: 1,
            total_drops: 0,
            ports: vec![],
            idle_secs: 0,
        }
    }

    const ALL_STYLES: [LayoutStyle; 5] = [
        LayoutStyle { node: NodeStyle::Circle, container: Container::Circle, arrangement: Arrangement::Packed },
        LayoutStyle { node: NodeStyle::Circle, container: Container::Box, arrangement: Arrangement::Packed },
        LayoutStyle { node: NodeStyle::Card, container: Container::Box, arrangement: Arrangement::Packed },
        LayoutStyle { node: NodeStyle::Circle, container: Container::Circle, arrangement: Arrangement::Layered },
        LayoutStyle { node: NodeStyle::Card, container: Container::Box, arrangement: Arrangement::Layered },
    ];

    fn overlap(a: &Placed, b: &Placed) -> bool {
        if !a.card && !b.card {
            return (a.x - b.x).hypot(a.y - b.y) < a.r() + b.r() - 1e-6;
        }
        (a.x - b.x).abs() < (a.w + b.w) / 2.0 - 1e-6 && (a.y - b.y).abs() < (a.h + b.h) / 2.0 - 1e-6
    }

    fn inside(p: &Placed, g: &GroupShape) -> bool {
        if g.rect {
            (p.x - g.x).abs() + p.w / 2.0 <= g.w / 2.0 + 1e-6 && (p.y - g.y).abs() + p.h / 2.0 <= g.h / 2.0 + 1e-6
        } else {
            (p.x - g.x).hypot(p.y - g.y) + p.r() <= g.r() + 1e-6
        }
    }

    fn sample_with_traffic() -> Graph {
        let mut g = graph(4, 4);
        g.edges = vec![
            edge("ns0/pod-0", "ns1/pod-1"),
            edge("ns1/pod-1", "ns2/pod-2"),
            edge("ns2/pod-2", "ns3/pod-3"),
            edge("ns3/pod-3", "ns1/pod-1"), // cycle
            edge("ns0/pod-1", "ns3/pod-3"),
        ];
        g
    }

    #[test]
    fn no_overlaps_in_any_style_or_level() {
        let g = sample_with_traffic();
        for style in ALL_STYLES {
            for level in [Level::Namespace, Level::Workload, Level::Pod] {
                let agg = aggregate(&g, level);
                let l = layout(&agg, level, 1.6, style);
                assert_eq!(l.nodes.len(), agg.nodes.len(), "{style:?} {level:?}");
                let v: Vec<_> = l.nodes.iter().collect();
                for i in 0..v.len() {
                    for j in i + 1..v.len() {
                        assert!(!overlap(v[i].1, v[j].1), "{style:?} {level:?}: {} overlaps {}", v[i].0, v[j].0);
                    }
                }
            }
        }
    }

    #[test]
    fn packed_members_stay_in_their_group_and_groups_do_not_overlap() {
        let g = graph(8, 3);
        for style in &ALL_STYLES[..3] {
            for level in [Level::Workload, Level::Pod] {
                let agg = aggregate(&g, level);
                let l = layout(&agg, level, 1.6, *style);
                for n in &agg.nodes {
                    let gs = l.groups.iter().find(|x| x.name == n.group).unwrap();
                    assert!(inside(&l.nodes[&n.id], gs), "{style:?}: {} escapes {}", n.id, gs.name);
                }
                for i in 0..l.groups.len() {
                    for j in i + 1..l.groups.len() {
                        let (a, b) = (&l.groups[i], &l.groups[j]);
                        let sep = if a.rect && b.rect {
                            (a.x - b.x).abs() >= (a.w + b.w) / 2.0 || (a.y - b.y).abs() >= (a.h + b.h) / 2.0
                        } else {
                            (a.x - b.x).hypot(a.y - b.y) >= a.r() + b.r()
                        };
                        assert!(sep, "{style:?}: {} overlaps {}", a.name, b.name);
                    }
                }
            }
        }
    }

    #[test]
    fn cards_force_boxes() {
        let s = LayoutStyle { node: NodeStyle::Card, container: Container::Circle, arrangement: Arrangement::Packed };
        let agg = aggregate(&graph(3, 2), Level::Pod);
        assert!(layout(&agg, Level::Pod, 1.6, s).groups.iter().all(|g| g.rect));
    }

    #[test]
    fn layered_follows_traffic_and_parks_idle_nodes() {
        let g = sample_with_traffic();
        let agg = aggregate(&g, Level::Pod);
        let l = layout(&agg, Level::Pod, 1.6, ALL_STYLES[3]);
        let x = |id: &str| l.nodes[id].x;
        assert!(x("ns0/pod-0") < x("ns1/pod-1"));
        assert!(x("ns1/pod-1") < x("ns2/pod-2"));
        assert!(x("ns2/pod-2") < x("ns3/pod-3"));
        let idle = l.groups.iter().find(|g| g.name == IDLE_GROUP).unwrap();
        assert_eq!(idle.members, agg.nodes.len() - 5);
        // Idle box sits below every connected node.
        let connected_bottom = ["ns0/pod-0", "ns0/pod-1", "ns1/pod-1", "ns2/pod-2", "ns3/pod-3"]
            .iter()
            .map(|id| l.nodes[*id].y)
            .fold(f64::MIN, f64::max);
        assert!(idle.y - idle.h / 2.0 > connected_bottom);
    }

    #[test]
    fn follows_viewport_aspect() {
        let agg = aggregate(&graph(12, 3), Level::Workload);
        for style in &ALL_STYLES[..3] {
            let (x0, y0, x1, y1) = layout(&agg, Level::Workload, 0.5, *style).bounds;
            assert!(y1 - y0 > x1 - x0, "{style:?}: portrait should be tall");
            let (x0, y0, x1, y1) = layout(&agg, Level::Workload, 2.0, *style).bounds;
            assert!(x1 - x0 > y1 - y0, "{style:?}: landscape should be wide");
        }
    }

    #[test]
    fn layout_is_deterministic() {
        let g = sample_with_traffic();
        for style in ALL_STYLES {
            let agg = aggregate(&g, Level::Pod);
            assert_eq!(layout(&agg, Level::Pod, 1.6, style), layout(&agg, Level::Pod, 1.6, style));
        }
    }

    #[test]
    fn card_reach_hits_the_outline() {
        let p = Placed { x: 0.0, y: 0.0, w: 100.0, h: 40.0, card: true };
        assert!((p.reach(1.0, 0.0) - 50.0).abs() < 1e-9);
        assert!((p.reach(0.0, 1.0) - 20.0).abs() < 1e-9);
        let c = Placed { card: false, w: 20.0, h: 20.0, ..p };
        assert_eq!(c.reach(0.6, 0.8), 10.0);
    }
}
