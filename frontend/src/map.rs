//! Global map tab: toolbar, view options, canvas, details panel and the
//! traffic panel underneath.

use crate::app::{use_ctx, Ctx, EpIndex, Tab};
use crate::filter_bar::FilterBar;
use crate::flows::{
    filtered, ColumnsMenu, FlowDetailPanel, FlowFilterControls, FlowFilters, FlowTable, PauseButton, Scope,
};
use crate::icons;
use crate::scene::{self, Scene, SceneInput};
use crate::settings::{EdgeStyle, LabelMode, ViewSettings, PRESETS};
use crate::util::{fmt_rate, namespace_of, short_name, window};
use common::aggregate::{aggregate, group_id, AggEdge, AggGraph, Level};
use common::filter::{EndpointRef, Filter, FlowRef};
use common::layout::{Arrangement, Container, NodeStyle};
use common::{Graph, Node, NodeKind, PolicyInfo};
use leptos::prelude::*;
use send_wrapper::SendWrapper;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

/// The canvas renderer, created once the canvas is mounted.
type SceneSlot = Rc<RefCell<Option<Rc<RefCell<Scene>>>>>;

/// Pod ids the highlighted policy selects.
fn policy_pods(ctx: Ctx) -> Option<(String, HashSet<String>)> {
    let key = ctx.highlight.get()?;
    ctx.policies
        .with(|ps| ps.iter().find(|p| p.key() == key).map(|p| (key.clone(), p.selected.iter().cloned().collect())))
}

/// Aggregated ids of the pods the highlighted policy selects.
fn highlight_ids(ctx: Ctx) -> BTreeSet<String> {
    let Some((_, pods)) = policy_pods(ctx) else {
        return BTreeSet::new();
    };
    let level = ctx.level.get();
    ctx.graph.with(|g| g.nodes.iter().filter(|n| pods.contains(&n.id)).map(|n| group_id(n, level)).collect())
}

/// The part of the graph the filter lets through: connections whose flow
/// matches, their endpoints, and (for filters about endpoints only) every
/// matching pod, so idle pods still show.
fn filter_graph(g: &Graph, filter: &Filter, ix: &EpIndex) -> Graph {
    if filter.is_empty() {
        return g.clone();
    }
    let by_id: HashMap<&str, &Node> = g.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut labels: HashMap<&str, Vec<String>> = HashMap::new();
    for id in
        g.nodes.iter().map(|n| n.id.as_str()).chain(g.edges.iter().flat_map(|e| [e.source.as_str(), e.target.as_str()]))
    {
        labels.entry(id).or_insert_with(|| ix.labels_for(id));
    }
    let endpoint = |id: &str| -> (String, String, u32) {
        let n = by_id.get(id);
        (
            n.map(|n| n.workload.clone()).unwrap_or_default(),
            n.map(|n| n.ip.clone()).unwrap_or_default(),
            ix.identity.get(id).copied().unwrap_or(0),
        )
    };

    let mut keep: HashSet<String> = HashSet::new();
    let mut edges = Vec::new();
    for e in &g.edges {
        let (sw, sip, sid) = endpoint(&e.source);
        let (dw, dip, did) = endpoint(&e.target);
        let ports: Vec<(String, u32)> = e
            .ports
            .iter()
            .filter_map(|p| p.split_once('/').map(|(proto, port)| (proto.to_string(), port.parse().unwrap_or(0))))
            .collect();
        let r = FlowRef {
            src: EndpointRef {
                id: &e.source,
                workload: &sw,
                ip: &sip,
                identity: sid,
                labels: &labels[e.source.as_str()],
                dns: "",
            },
            dst: EndpointRef {
                id: &e.target,
                workload: &dw,
                ip: &dip,
                identity: did,
                labels: &labels[e.target.as_str()],
                dns: "",
            },
            ports: ports.iter().map(|(p, n)| (p.as_str(), *n)).collect(),
            forwarded: e.flows_per_sec > 0.0 || e.total_flows > 0,
            dropped: e.drops_per_sec > 0.0 || (e.total_drops > 0 && e.idle_secs < 60),
            verdict: "",
            node: "",
            policies: &[],
            l7: "",
        };
        if filter.matches_flow(&r) {
            keep.insert(e.source.clone());
            keep.insert(e.target.clone());
            edges.push(e.clone());
        }
    }
    let endpoint_only = !filter.has_flow_terms();
    let nodes = g
        .nodes
        .iter()
        .filter(|n| {
            keep.contains(&n.id)
                || (endpoint_only && {
                    let (w, ip, id) = endpoint(&n.id);
                    filter.matches_endpoint(&EndpointRef {
                        id: &n.id,
                        workload: &w,
                        ip: &ip,
                        identity: id,
                        labels: &labels[n.id.as_str()],
                        dns: "",
                    })
                })
        })
        .cloned()
        .collect();
    Graph { nodes, edges, generated_at_ms: g.generated_at_ms }
}

#[component]
pub fn MapTab() -> impl IntoView {
    let ctx = use_ctx();
    let dropped_only = RwSignal::new(false);
    let hide_idle = RwSignal::new(false);
    let fit = RwSignal::new(0u32);

    let agg = Memo::new(move |_| {
        let level = ctx.level.get();
        if ctx.view.with(|v| v.filter_map) {
            let filter = ctx.parsed.get();
            if !filter.is_empty() {
                let g = ctx.index.with(|ix| ctx.graph.with(|g| filter_graph(g, &filter, ix)));
                return aggregate(&g, level);
            }
        }
        ctx.graph.with(|g| aggregate(g, level))
    });
    let highlight = Memo::new(move |_| highlight_ids(ctx));
    // A new filter usually shows a very different part of the map.
    Effect::new(move |prev: Option<()>| {
        ctx.filter.track();
        ctx.view.with(|v| v.filter_map);
        if prev.is_some() {
            fit.update(|f| *f += 1);
        }
    });

    let canvas_ref = NodeRef::<leptos::html::Canvas>::new();
    let slot: SendWrapper<SceneSlot> = SendWrapper::new(Rc::new(RefCell::new(None)));
    Effect::new(move |_| {
        let v = ctx.view.get();
        let input = SceneInput {
            agg: agg.get(),
            level: Some(ctx.level.get()),
            style: v.layout,
            edges: v.edges,
            labels: v.labels,
            icons: v.icons,
            animate: v.animate,
            three_d: v.three_d,
            tower: v.tower,
            auto_rotate: v.auto_rotate,
            selected: ctx.selected.get(),
            highlight: highlight.get(),
            search: String::new(),
            dropped_only: dropped_only.get(),
            hide_idle: hide_idle.get(),
            fit: fit.get(),
        };
        let Some(canvas) = canvas_ref.get() else { return };
        let mut slot = slot.borrow_mut();
        let scene = slot.get_or_insert_with(|| {
            let s = Rc::new(RefCell::new(Scene::new(canvas, ctx.selected)));
            scene::attach(s.clone());
            s
        });
        scene.borrow_mut().update(input);
    });

    let level_btn = move |l: Level, label: &'static str| {
        view! {
            <button
                class:active=move || ctx.level.get() == l
                on:click=move |_| {
                    ctx.selected.set(None);
                    ctx.level.set(l);
                }
            >
                {label}
            </button>
        }
    };

    let stats = move || {
        let (pods, edges, rate, drops) = ctx.graph.with(|g| {
            (
                g.nodes.iter().filter(|n| n.kind == NodeKind::Pod).count(),
                g.edges.len(),
                g.edges.iter().map(|e| e.flows_per_sec).sum::<f32>(),
                g.edges.iter().map(|e| e.drops_per_sec).sum::<f32>(),
            )
        });
        format!("{pods} pods · {edges} connections · {} flows · {} drops", fmt_rate(rate), fmt_rate(drops))
    };

    view! {
        <div class="toolbar">
            <div class="seg">
                {level_btn(Level::Namespace, "Namespaces")} {level_btn(Level::Workload, "Workloads")}
                {level_btn(Level::Pod, "Pods")}
            </div>
            <FilterBar />
            <label class="chk">
                <input
                    type="checkbox"
                    prop:checked=move || dropped_only.get()
                    on:change=move |ev| dropped_only.set(event_target_checked(&ev))
                />
                "Drops only"
            </label>
            <label class="chk">
                <input
                    type="checkbox"
                    prop:checked=move || hide_idle.get()
                    on:change=move |ev| hide_idle.set(event_target_checked(&ev))
                />
                "Hide idle"
            </label>
            <button class="btn" on:click=move |_| fit.update(|f| *f += 1)>
                "Fit"
            </button>
            <div class="seg">
                <button
                    class:active=move || !ctx.view.with(|v| v.three_d)
                    on:click=move |_| ctx.view.update(|v| v.three_d = false)
                >
                    "2D"
                </button>
                <button
                    class:active=move || ctx.view.with(|v| v.three_d)
                    on:click=move |_| ctx.view.update(|v| v.three_d = true)
                >
                    "3D"
                </button>
            </div>
            <ViewMenu />
            {move || {
                ctx.highlight
                    .get()
                    .map(|key| {
                        view! {
                            <span class="chip hl">
                                "Policy " <b>{key}</b> <button class="x" on:click=move |_| ctx.highlight.set(None)>
                                    "✕"
                                </button>
                            </span>
                        }
                    })
            }}
        </div>
        <div class="map-area">
            <canvas class="map" node_ref=canvas_ref></canvas>
            <div class="legend">
                <span>
                    <i class="sw ok"></i>
                    "traffic"
                </span>
                <span>
                    <i class="sw drop"></i>
                    "drops"
                </span>
                <span>
                    <i class="sw idle"></i>
                    "idle"
                </span>
            </div>
            <div class="stats">{stats}</div>
            {move || {
                ctx.view
                    .with(|v| v.three_d)
                    .then(|| {
                        view! {
                            <div class="hint">
                                "Drag to rotate · Shift- or right-drag to pan · scroll or pinch to zoom"
                            </div>
                        }
                    })
            }}
            <Details agg=agg />
        </div>
        <TrafficPanel agg=agg />
    }
}

// ---- view menu ----------------------------------------------------------------

/// One row of mutually exclusive options in the View menu.
fn seg_row<T>(
    ctx: Ctx,
    label: &'static str,
    options: &'static [(T, &'static str)],
    get: fn(&ViewSettings) -> T,
    set: fn(&mut ViewSettings, T),
    disabled: Signal<bool>,
) -> impl IntoView
where
    T: Copy + PartialEq + Send + Sync + 'static,
{
    view! {
        <div class="menu-row" class:disabled=move || disabled.get()>
            <span class="menu-label">{label}</span>
            <div class="seg small">
                {options
                    .iter()
                    .map(|&(value, text)| {
                        view! {
                            <button
                                class:active=move || ctx.view.with(|v| get(v) == value)
                                disabled=move || disabled.get()
                                on:click=move |_| ctx.view.update(|v| set(v, value))
                            >
                                {text}
                            </button>
                        }
                    })
                    .collect_view()}
            </div>
        </div>
    }
}

#[component]
fn ViewMenu() -> impl IntoView {
    let ctx = use_ctx();
    let open = RwSignal::new(false);
    let cards = Signal::derive(move || ctx.view.with(|v| v.layout.node == NodeStyle::Card));
    let never = Signal::derive(|| false);
    let flat = Signal::derive(move || !ctx.view.with(|v| v.three_d));

    let toggle = move |label: &'static str, get: fn(&ViewSettings) -> bool, set: fn(&mut ViewSettings, bool)| {
        view! {
            <label class="chk">
                <input
                    type="checkbox"
                    prop:checked=move || ctx.view.with(get)
                    on:change=move |ev| {
                        let on = event_target_checked(&ev);
                        ctx.view.update(|v| set(v, on));
                    }
                />
                {label}
            </label>
        }
    };

    view! {
        <div class="menu-wrap">
            <button class="btn" class:active=move || open.get() on:click=move |_| open.update(|o| *o = !*o)>
                "View ▾"
            </button>
            {move || {
                open.get()
                    .then(|| {
                        view! {
                            <div class="backdrop" on:click=move |_| open.set(false)></div>
                            <div class="menu view-menu">
                                <div class="menu-title">"Presets"</div>
                                <div class="presets">
                                    {PRESETS
                                        .iter()
                                        .map(|&(name, layout, edges)| {
                                            view! {
                                                <button
                                                    class="preset"
                                                    class:active=move || {
                                                        ctx.view.with(|v| v.layout == layout && v.edges == edges)
                                                    }
                                                    on:click=move |_| {
                                                        ctx.view
                                                            .update(|v| {
                                                                v.layout = layout;
                                                                v.edges = edges;
                                                            })
                                                    }
                                                >
                                                    <span class="preview" inner_html=preset_svg(name)></span>
                                                    {name}
                                                </button>
                                            }
                                        })
                                        .collect_view()}
                                </div>
                                {seg_row(
                                    ctx,
                                    "Nodes",
                                    &[(NodeStyle::Circle, "Circles"), (NodeStyle::Card, "Cards")],
                                    |v| v.layout.node,
                                    |v, x| v.layout.node = x,
                                    never,
                                )}
                                {seg_row(
                                    ctx,
                                    "Namespaces",
                                    &[(Container::Circle, "Circles"), (Container::Box, "Boxes")],
                                    |v| v.layout.effective_container(),
                                    |v, x| v.layout.container = x,
                                    cards,
                                )}
                                {seg_row(
                                    ctx,
                                    "Arrangement",
                                    &[(Arrangement::Packed, "Packed"), (Arrangement::Layered, "Left → right")],
                                    |v| v.layout.arrangement,
                                    |v, x| v.layout.arrangement = x,
                                    never,
                                )}
                                {seg_row(
                                    ctx,
                                    "Edges",
                                    &[
                                        (EdgeStyle::Curved, "Curved"),
                                        (EdgeStyle::Straight, "Straight"),
                                        (EdgeStyle::Orthogonal, "Right-angle"),
                                    ],
                                    |v| v.edges,
                                    |v, x| v.edges = x,
                                    never,
                                )}
                                {seg_row(
                                    ctx,
                                    "Labels",
                                    &[
                                        (LabelMode::Auto, "Auto"),
                                        (LabelMode::Always, "Always"),
                                        (LabelMode::Off, "Off"),
                                    ],
                                    |v| v.labels,
                                    |v, x| v.labels = x,
                                    never,
                                )}
                                <div class="menu-row toggles">
                                    {toggle("Icons", |v| v.icons, |v, x| v.icons = x)}
                                    {toggle("Animate traffic", |v| v.animate, |v, x| v.animate = x)}
                                    {toggle("Filter the map", |v| v.filter_map, |v, x| v.filter_map = x)}
                                </div>
                                <div class="menu-title">"3D"</div>
                                {seg_row(
                                    ctx,
                                    "Mode",
                                    &[(false, "2D"), (true, "3D")],
                                    |v| v.three_d,
                                    |v, x| v.three_d = x,
                                    never,
                                )}
                                {seg_row(
                                    ctx,
                                    "3D style",
                                    &[(false, "Map"), (true, "Tower")],
                                    |v| v.tower,
                                    |v, x| v.tower = x,
                                    flat,
                                )}
                                <div class="menu-row toggles" class:disabled=move || flat.get()>
                                    {toggle("Auto-rotate", |v| v.auto_rotate, |v, x| v.auto_rotate = x)}
                                </div>
                                <div class="menu-title">"Icons"</div>
                                <div class="icon-key">
                                    {icons::LEGEND
                                        .iter()
                                        .map(|(k, label)| {
                                            view! {
                                                <span>
                                                    <span class="ico" inner_html=icons::svg(k)></span>
                                                    {*label}
                                                </span>
                                            }
                                        })
                                        .collect_view()}
                                </div>
                            </div>
                        }
                    })
            }}
        </div>
    }
}

/// Tiny picture of each preset for the View menu.
fn preset_svg(name: &str) -> &'static str {
    match name {
        "Bubbles" => {
            r#"<svg viewBox="0 0 60 36"><circle cx="16" cy="18" r="13" class="g"/><circle cx="44" cy="18" r="12" class="g"/><circle cx="12" cy="15" r="3" class="n"/><circle cx="19" cy="22" r="3" class="n"/><circle cx="44" cy="18" r="3.5" class="n"/><path d="M21 21Q32 28 41 19" class="e"/></svg>"#
        }
        "Boxes" => {
            r#"<svg viewBox="0 0 60 36"><rect x="3" y="4" width="24" height="28" rx="3" class="g"/><rect x="33" y="4" width="24" height="28" rx="3" class="g"/><circle cx="11" cy="14" r="3" class="n"/><circle cx="19" cy="24" r="3" class="n"/><circle cx="45" cy="20" r="3.5" class="n"/><path d="M22 24H32V20H41" class="e"/></svg>"#
        }
        "Cards" => {
            r#"<svg viewBox="0 0 60 36"><rect x="3" y="4" width="54" height="28" rx="3" class="g"/><rect x="7" y="9" width="20" height="8" rx="2" class="c"/><rect x="33" y="19" width="20" height="8" rx="2" class="c"/><rect x="7" y="21" width="20" height="8" rx="2" class="c"/><path d="M27 13Q32 13 33 21" class="e"/></svg>"#
        }
        _ => {
            r#"<svg viewBox="0 0 60 36"><rect x="3" y="7" width="15" height="8" rx="2" class="c"/><rect x="3" y="21" width="15" height="8" rx="2" class="c"/><rect x="23" y="14" width="15" height="8" rx="2" class="c"/><rect x="43" y="14" width="14" height="8" rx="2" class="c"/><path d="M18 11Q21 11 23 17M18 25Q21 25 23 19M38 18H43" class="e"/></svg>"#
        }
    }
}

// ---- details panel ------------------------------------------------------------

/// Filter that selects this map node's traffic.
fn flow_filter_for(id: &str, level: Level, label: &str, group: &str) -> String {
    if id.starts_with("reserved:") {
        return format!("pod={id}");
    }
    if id.starts_with("identity:") {
        return id.to_string();
    }
    match level {
        Level::Pod => format!("pod={id}"),
        Level::Workload => format!("ns={group} workload={label}"),
        Level::Namespace => format!("ns={group}"),
    }
}

#[component]
fn Details(agg: Memo<AggGraph>) -> impl IntoView {
    let ctx = use_ctx();
    move || {
        let id = ctx.selected.get()?;
        let level = ctx.level.get();
        let (node, out_edges, in_edges) = agg.with(|a| {
            let node = a.node(&id).cloned()?;
            let mut outs: Vec<AggEdge> = a.edges.iter().filter(|e| e.source == id).cloned().collect();
            let mut ins: Vec<AggEdge> = a.edges.iter().filter(|e| e.target == id).cloned().collect();
            let by_rate = |a: &AggEdge, b: &AggEdge| {
                (b.flows_per_sec + b.drops_per_sec).total_cmp(&(a.flows_per_sec + a.drops_per_sec))
            };
            outs.sort_by(by_rate);
            ins.sort_by(by_rate);
            Some((node, outs, ins))
        })?;
        let members: Vec<Node> =
            ctx.graph.with(|g| g.nodes.iter().filter(|m| group_id(m, level) == id).cloned().collect());
        let policies: Vec<PolicyInfo> = ctx
            .policies
            .with(|ps| ps.iter().filter(|p| members.iter().any(|m| p.selected.contains(&m.id))).cloned().collect());
        let single = (members.len() == 1).then(|| members[0].clone());
        let filter = flow_filter_for(&id, level, &node.label, &node.group);
        let icon = icons::svg(&node.workload_kind);

        Some(view! {
            <aside class="details">
                <div class="details-head">
                    <div class="head-main">
                        <span class="ico big" inner_html=icon></span>
                        <div>
                            <div class="title">{node.label.clone()}</div>
                            <div class="sub">
                                {if node.kind == NodeKind::External {
                                    "outside the cluster".to_string()
                                } else if level == Level::Namespace {
                                    "namespace".to_string()
                                } else {
                                    format!("{} · namespace {}", node.workload_kind, node.group)
                                }}
                            </div>
                        </div>
                    </div>
                    <button class="x" on:click=move |_| ctx.selected.set(None)>
                        "✕"
                    </button>
                </div>
                <div class="kv">
                    <span>"in"</span>
                    <b>{fmt_rate(node.flows_in)}</b>
                    <span>"out"</span>
                    <b>{fmt_rate(node.flows_out)}</b>
                    {(node.internal_flows > 0.0)
                        .then(|| {
                            view! {
                                <span>"internal"</span>
                                <b>{fmt_rate(node.internal_flows)}</b>
                            }
                        })}
                    {(node.drops > 0.0)
                        .then(|| {
                            view! {
                                <span>"dropped"</span>
                                <b class="bad">{fmt_rate(node.drops)}</b>
                            }
                        })}
                    {(node.members > 1)
                        .then(|| {
                            view! {
                                <span>"pods"</span>
                                <b>{node.members}</b>
                            }
                        })}
                    {single
                        .map(|p| {
                            view! {
                                <span>"workload"</span>
                                <b>{p.workload}</b>
                                <span>"node"</span>
                                <b>{p.node_name}</b>
                                <span>"IP"</span>
                                <b>{p.ip}</b>
                                <span>"phase"</span>
                                <b>{p.phase}</b>
                            }
                        })}
                </div>
                <button
                    class="btn wide"
                    on:click=move |_| {
                        ctx.filter.set(filter.clone());
                        ctx.tab.set(Tab::Flows);
                    }
                >
                    "Open in Flows tab"
                </button>
                <EdgeList title="Outgoing" edges=out_edges outgoing=true />
                <EdgeList title="Incoming" edges=in_edges outgoing=false />
                {(!policies.is_empty())
                    .then(|| {
                        view! {
                            <h4>"Policies selecting this"</h4>
                            <ul class="plist">
                                {policies
                                    .into_iter()
                                    .map(|p| {
                                        let key = p.key();
                                        view! {
                                            <li on:click=move |_| ctx.highlight.set(Some(key.clone()))>
                                                <span class=format!(
                                                    "kind k-{}",
                                                    p.kind.short().to_lowercase(),
                                                )>{p.kind.short()}</span>
                                                {p.namespace.map(|n| format!("{n}/")).unwrap_or_default()}
                                                {p.name}
                                            </li>
                                        }
                                    })
                                    .collect_view()}
                            </ul>
                        }
                    })}
            </aside>
        })
    }
}

#[component]
fn EdgeList(title: &'static str, edges: Vec<AggEdge>, outgoing: bool) -> impl IntoView {
    let ctx = use_ctx();
    if edges.is_empty() {
        return None;
    }
    let total = edges.len();
    Some(view! {
        <h4>{title} " (" {total} ")"</h4>
        <ul class="elist">
            {edges
                .into_iter()
                .take(15)
                .map(|e| {
                    let peer = if outgoing { e.target.clone() } else { e.source.clone() };
                    let peer_click = peer.clone();
                    let ns = namespace_of(&peer).to_string();
                    view! {
                        <li on:click=move |_| ctx.selected.set(Some(peer_click.clone()))>
                            <span class="peer">
                                {short_name(&peer).to_string()}
                                {(!ns.is_empty()).then(|| view! { <small>{ns}</small> })}
                            </span>
                            <span class="ports">{e.ports.join(" ")}</span>
                            <span class="rate" class:bad={e.drops_per_sec > 0.0}>
                                {if e.drops_per_sec > 0.0 {
                                    format!("{} drop", fmt_rate(e.drops_per_sec))
                                } else {
                                    fmt_rate(e.flows_per_sec)
                                }}
                            </span>
                        </li>
                    }
                })
                .collect_view()}
        </ul>
    })
}

// ---- traffic panel ------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum PanelTab {
    Flows,
    Connections,
}

const PANEL_ROWS: usize = 200;
const PANEL_HEADER: f64 = 40.0;

/// Collapsible, resizable panel under the map, like Hubble UI's flows table.
/// Follows the map: a selected node or highlighted policy narrows it down.
#[component]
fn TrafficPanel(agg: Memo<AggGraph>) -> impl IntoView {
    let ctx = use_ctx();
    let tab = RwSignal::new(PanelTab::Flows);
    let filters = FlowFilters::new();
    // A clicked connection narrows the flows to selected node -> this peer.
    let pair = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        ctx.selected.track();
        ctx.level.track();
        pair.set(None);
    });

    // Pod ids under an aggregated map id.
    let members = move |id: &str| -> HashSet<String> {
        let level = ctx.level.get();
        let mut ids: HashSet<String> =
            ctx.graph.with(|g| g.nodes.iter().filter(|n| group_id(n, level) == id).map(|n| n.id.clone()).collect());
        ids.insert(id.to_string());
        ids
    };
    // In scope: the selected node's pods (optionally one connection), else
    // the highlighted policy's pods.
    let scope = Memo::new(move |_| -> Option<Scope> {
        if let Some(sel) = ctx.selected.get() {
            let label = agg.with(|a| a.node(&sel).map(|n| n.label.clone()))?;
            let peer = pair.get().map(|p| {
                let name =
                    agg.with(|a| a.node(&p).map(|n| n.label.clone())).unwrap_or_else(|| short_name(&p).to_string());
                (name, members(&p))
            });
            return Some(Scope { label, ids: members(&sel), peer });
        }
        policy_pods(ctx).map(|(key, pods)| Scope { label: format!("policy {key}"), ids: pods, peer: None })
    });
    let rows = Memo::new(move |_| scope.with(|s| filtered(ctx, filters, s.as_ref(), PANEL_ROWS)));
    let highlight = Memo::new(move |_| highlight_ids(ctx));
    let connections = Memo::new(move |_| {
        let sel = ctx.selected.get();
        let hl = highlight.get();
        let mut v: Vec<AggEdge> = agg.with(|a| {
            a.edges
                .iter()
                .filter(|e| match &sel {
                    Some(s) => &e.source == s || &e.target == s,
                    None => hl.is_empty() || hl.contains(&e.source) || hl.contains(&e.target),
                })
                .cloned()
                .collect()
        });
        v.sort_by(|a, b| (b.flows_per_sec + b.drops_per_sec).total_cmp(&(a.flows_per_sec + a.drops_per_sec)));
        v.truncate(PANEL_ROWS);
        v
    });

    // Drag the top edge to resize.
    let drag = RwSignal::new(None::<(f64, f64)>);
    let max_height = move || {
        let h = window().inner_height().ok().and_then(|h| h.as_f64()).unwrap_or(800.0);
        (h - 200.0).max(160.0)
    };
    let open = move || ctx.view.with(|v| v.drawer_open);
    let height = move || {
        if open() {
            ctx.view.with(|v| v.drawer_height).clamp(120.0, max_height())
        } else {
            PANEL_HEADER
        }
    };
    let toggle = move || ctx.view.update(|v| v.drawer_open = !v.drawer_open);

    let tab_btn = move |t: PanelTab, label: &'static str, count: Signal<usize>| {
        view! {
            <button
                class="ptab"
                class:active=move || tab.get() == t && open()
                on:click=move |_| {
                    tab.set(t);
                    if !open() {
                        toggle();
                    }
                }
            >
                {label}
                <span class="count">{move || count.get()}</span>
            </button>
        }
    };
    let flow_count = Signal::derive(move || rows.with(Vec::len));
    let conn_count = Signal::derive(move || connections.with(Vec::len));

    view! {
        <section class="panel" style:height=move || format!("{}px", height())>
            <div
                class="panel-grip"
                class:hidden=move || !open()
                on:pointerdown=move |ev| {
                    let el = event_target::<web_sys::Element>(&ev);
                    let _ = el.set_pointer_capture(ev.pointer_id());
                    drag.set(Some((f64::from(ev.client_y()), height())));
                }
                on:pointermove=move |ev| {
                    if let Some((y0, h0)) = drag.get_untracked() {
                        let h = (h0 - (f64::from(ev.client_y()) - y0)).clamp(120.0, max_height());
                        ctx.view.update(|v| v.drawer_height = h);
                    }
                }
                on:pointerup=move |_| drag.set(None)
                on:pointercancel=move |_| drag.set(None)
            ></div>
            <div class="panel-head">
                {tab_btn(PanelTab::Flows, "Flows", flow_count)}
                {tab_btn(PanelTab::Connections, "Connections", conn_count)}
                {move || {
                    scope
                        .get()
                        .map(|sc| {
                            view! {
                                <span class="chip scope">
                                    <b>{sc.label}</b>
                                    {sc
                                        .peer
                                        .map(|(p, _)| {
                                            view! {
                                                " → "
                                                <b>{p}</b>
                                            }
                                        })}
                                    <button
                                        class="x"
                                        title="Show all traffic"
                                        on:click=move |_| {
                                            ctx.selected.set(None);
                                            ctx.highlight.set(None);
                                        }
                                    >
                                        "✕"
                                    </button>
                                </span>
                            }
                        })
                }} <div class="panel-tools" class:hidden=move || !open()>
                    {move || {
                        (tab.get() == PanelTab::Flows)
                            .then(|| {
                                view! {
                                    <FlowFilterControls filters=filters />
                                    <ColumnsMenu />
                                    <PauseButton />
                                }
                            })
                    }}
                </div> <button class="btn icon-btn collapse" title="Show / hide" on:click=move |_| toggle()>
                    {move || if open() { "▾" } else { "▴" }}
                </button>
            </div>
            <div class="panel-body" class:hidden=move || !open()>
                {move || match tab.get() {
                    PanelTab::Flows => {
                        view! {
                            <div class="flows-split" class:has-open=move || filters.open.get().is_some()>
                                <div class="table-wrap">
                                    <FlowTable rows=rows filters=filters />
                                </div>
                                <FlowDetailPanel filters=filters />
                            </div>
                        }
                            .into_any()
                    }
                    PanelTab::Connections => {
                        view! {
                            <div class="table-wrap">
                                <ConnectionsTable rows=connections tab=tab pair=pair />
                            </div>
                        }
                            .into_any()
                    }
                }}
            </div>
        </section>
    }
}

#[component]
fn ConnectionsTable(
    rows: Memo<Vec<AggEdge>>,
    tab: RwSignal<PanelTab>,
    pair: RwSignal<Option<String>>,
) -> impl IntoView {
    let ctx = use_ctx();
    let ep = move |id: String| {
        let ns = namespace_of(&id).to_string();
        let name = short_name(&id).to_string();
        let kind = agg_kind(ctx, &id);
        view! {
            <td class="ep">
                <span class="ep-main">
                    <span class="ico" inner_html=icons::svg(&kind)></span>
                    <span class="name">{name}</span>
                </span>
                {(!ns.is_empty()).then(|| view! { <small>{ns}</small> })}
            </td>
        }
    };
    view! {
        <table class="flow-table conn-table">
            <thead>
                <tr>
                    <th>"Source"</th>
                    <th>"Destination"</th>
                    <th>"Ports"</th>
                    <th class="num">"Flows/s"</th>
                    <th class="num">"Drops/s"</th>
                    <th class="num">"Flows"</th>
                    <th class="num">"Drops"</th>
                    <th class="num">"Last seen"</th>
                </tr>
            </thead>
            <tbody>
                {move || {
                    rows.get()
                        .into_iter()
                        .map(|e| {
                            let (src, dst) = (e.source.clone(), e.target.clone());
                            view! {
                                <tr
                                    class:dropped={e.drops_per_sec > 0.0}
                                    title="Show these flows"
                                    on:click=move |_| {
                                        ctx.selected.set(Some(src.clone()));
                                        let dst = dst.clone();
                                        crate::util::set_timeout(move || pair.set(Some(dst)), 0);
                                        tab.set(PanelTab::Flows);
                                    }
                                >
                                    {ep(e.source.clone())}
                                    {ep(e.target.clone())}
                                    <td class="mono small">{e.ports.join(" ")}</td>
                                    <td class="num mono" class:ok={e.flows_per_sec > 0.0}>
                                        {fmt_rate(e.flows_per_sec)}
                                    </td>
                                    <td class="num mono" class:bad={e.drops_per_sec > 0.0}>
                                        {fmt_rate(e.drops_per_sec)}
                                    </td>
                                    <td class="num mono">{e.total_flows}</td>
                                    <td class="num mono">{e.total_drops}</td>
                                    <td class="num muted">
                                        {if e.idle_secs == 0 {
                                            "now".to_string()
                                        } else {
                                            format!("{}s ago", e.idle_secs)
                                        }}
                                    </td>
                                </tr>
                            }
                        })
                        .collect_view()
                }}
            </tbody>
        </table>
    }
}

/// Icon kind for an aggregated id at the current level.
fn agg_kind(ctx: Ctx, id: &str) -> String {
    if let Some(e) = id.strip_prefix("reserved:") {
        return e.to_string();
    }
    if id.starts_with("ns:") {
        return "Namespace".into();
    }
    let level = ctx.level.get_untracked();
    ctx.graph.with_untracked(|g| {
        g.nodes
            .iter()
            .find(|n| group_id(n, level) == id)
            .map(|n| common::aggregate::kind_of_workload(&n.workload).to_string())
            .unwrap_or_else(|| "Pod".into())
    })
}
