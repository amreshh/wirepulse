//! Flows: filters, a configurable table and a flow details panel. Used by the
//! Flows tab and by the traffic panel under the map.

use crate::app::{use_ctx, Ctx, EpIndex};
use crate::filter_bar::FilterBar;
use crate::icons;
use crate::settings::Col;
use crate::store::{Flow, FlowStore, KEEP_CHOICES};
use crate::util::{fmt_count, fmt_thousands, fmt_time, get_json, namespace_of, short_name};
use crate::widgets::{grip, Dropdown, SidePanel};
use common::aggregate::kind_of_workload;
use common::filter::{EndpointRef, Filter, FlowRef};
use common::{is_unresolved, EndpointDetail, FlowDetail, FlowEvent, Verdict};
use leptos::prelude::*;
use leptos::task::spawn_local;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VerdictFilter {
    All,
    Forwarded,
    Dropped,
    L7,
}

/// Table-local state (the text filter is global, in `Ctx`); every field is
/// a signal or a number, so this is `Copy`.
#[derive(Clone, Copy)]
pub struct FlowFilters {
    pub verdict: RwSignal<VerdictFilter>,
    pub hide_replies: RwSignal<bool>,
    /// Flow whose details are open.
    pub open: RwSignal<Option<Flow>>,
    /// Rows shown; "Show more" adds another `page`.
    pub shown: RwSignal<usize>,
    pub page: usize,
}

impl FlowFilters {
    pub fn new(page: usize) -> Self {
        Self {
            verdict: RwSignal::new(VerdictFilter::All),
            hide_replies: RwSignal::new(true),
            open: RwSignal::new(None),
            shown: RwSignal::new(page),
            page,
        }
    }
}

/// Narrows flows to part of the map: a selected node's pods, or one
/// connection (`ids` -> `peer`, either direction).
#[derive(Clone, Debug, PartialEq)]
pub struct Scope {
    /// Which node, connection or policy this is. The same key with other
    /// pods means the map changed, not the selection.
    pub key: String,
    pub label: String,
    pub ids: HashSet<String>,
    pub peer: Option<(String, HashSet<String>)>,
}

impl Scope {
    pub fn contains(&self, f: &FlowEvent) -> bool {
        match &self.peer {
            Some((_, peer)) => {
                (self.ids.contains(&f.source) && peer.contains(&f.destination))
                    || (peer.contains(&f.source) && self.ids.contains(&f.destination))
            }
            None => self.ids.contains(&f.source) || self.ids.contains(&f.destination),
        }
    }
}

/// Evaluates the global filter against one flow.
pub fn flow_matches(filter: &Filter, ix: &EpIndex, f: &FlowEvent) -> bool {
    if filter.is_empty() {
        return true;
    }
    let (src_l, dst_l) = (ix.labels_for(&f.source), ix.labels_for(&f.destination));
    let r = FlowRef {
        src: EndpointRef {
            id: &f.source,
            workload: &f.src_workload,
            ip: &f.src_ip,
            identity: f.src_identity,
            labels: &src_l,
            dns: "",
        },
        dst: EndpointRef {
            id: &f.destination,
            workload: &f.dst_workload,
            ip: &f.dst_ip,
            identity: f.dst_identity,
            labels: &dst_l,
            dns: &f.dst_dns,
        },
        ports: vec![(f.protocol.as_str(), f.dst_port)],
        forwarded: f.verdict == Verdict::Forwarded,
        dropped: matches!(f.verdict, Verdict::Dropped | Verdict::Error),
        verdict: f.verdict.as_str(),
        node: &f.node,
        policies: &f.policies,
        l7: &f.l7,
    };
    filter.matches_flow(&r)
}

/// What a flows table shows: the newest flows that pass its own filters (at
/// most `shown`), and how many of the kept flows pass them.
#[derive(Clone, Default)]
pub struct Rows {
    pub flows: Vec<Flow>,
    pub total: usize,
}

/// The same rows are the same flows: the store never copies one.
impl PartialEq for Rows {
    fn eq(&self, other: &Self) -> bool {
        self.total == other.total
            && self.flows.len() == other.flows.len()
            && self.flows.iter().zip(&other.flows).all(|(a, b)| Arc::ptr_eq(a, b))
    }
}

/// The rows of `pool` (already filtered and scoped by the store) that a
/// table with `f` shows. Only cheap checks are left for here.
pub fn table_rows(pool: &VecDeque<Flow>, f: FlowFilters) -> Rows {
    let (verdict, hide, shown) = (f.verdict.get(), f.hide_replies.get(), f.shown.get());
    let mut rows = Rows::default();
    for e in pool.iter().filter(|e| !(hide && e.is_reply)) {
        let pass = match verdict {
            VerdictFilter::Dropped => matches!(e.verdict, Verdict::Dropped | Verdict::Error),
            VerdictFilter::Forwarded => e.verdict == Verdict::Forwarded,
            VerdictFilter::L7 => !e.l7.is_empty(),
            VerdictFilter::All => true,
        };
        if pass {
            if rows.flows.len() < shown {
                rows.flows.push(e.clone());
            }
            rows.total += 1;
        }
    }
    rows
}

/// Verdict filter and "hide replies".
#[component]
pub fn FlowFilterControls(filters: FlowFilters) -> impl IntoView {
    view! {
        <select
            on:change=move |ev| {
                filters
                    .verdict
                    .set(
                        match event_target_value(&ev).as_str() {
                            "forwarded" => VerdictFilter::Forwarded,
                            "dropped" => VerdictFilter::Dropped,
                            "l7" => VerdictFilter::L7,
                            _ => VerdictFilter::All,
                        },
                    )
            }
            prop:value=move || match filters.verdict.get() {
                VerdictFilter::All => "all",
                VerdictFilter::Forwarded => "forwarded",
                VerdictFilter::Dropped => "dropped",
                VerdictFilter::L7 => "l7",
            }
        >
            <option value="all">"All verdicts"</option>
            <option value="forwarded">"Forwarded"</option>
            <option value="dropped">"Dropped"</option>
            <option value="l7">"With L7 info"</option>
        </select>
        <label class="chk">
            <input
                type="checkbox"
                prop:checked=move || filters.hide_replies.get()
                on:change=move |ev| filters.hide_replies.set(event_target_checked(&ev))
            />
            "Hide replies"
        </label>
    }
}

/// Dropdown to choose which table columns are shown.
#[component]
pub fn ColumnsMenu() -> impl IntoView {
    let ctx = use_ctx();
    let open = RwSignal::new(false);
    view! {
        <Dropdown label="Columns" menu_class="columns-menu" width=230.0 open=open>
            {Col::ALL
                .iter()
                .map(|&c| {
                    view! {
                        <label class="chk">
                            <input
                                type="checkbox"
                                prop:checked=move || ctx.view.with(|v| v.columns.contains(&c))
                                on:change=move |ev| {
                                    let on = event_target_checked(&ev);
                                    ctx.view
                                        .update(|v| {
                                            v.columns.retain(|x| *x != c);
                                            if on {
                                                v.columns.push(c);
                                                v.columns.sort_by_key(|x| Col::ALL.iter().position(|y| y == x));
                                            }
                                        });
                                }
                            />
                            {c.title()}
                        </label>
                    }
                })
                .collect_view()}
            {move || {
                (!ctx.view.with(|v| v.col_widths.is_empty()))
                    .then(|| {
                        view! {
                            <button class="btn" on:click=move |_| ctx.view.update(|v| v.col_widths.clear())>
                                "Default widths"
                            </button>
                        }
                    })
            }}
        </Dropdown>
    }
}

/// How many flows the tables keep (see `store`).
#[component]
pub fn KeepSelect() -> impl IntoView {
    let ctx = use_ctx();
    let title = move || {
        let mut t = "Flows kept in this browser: this many of the newest in the cluster, and as many again that \
                     match the filter or the node selected on the map, so other traffic can't push them out."
            .to_string();
        let server = ctx.info.with(|i| i.flow_history);
        if server > 0 {
            t.push_str(&format!(
                " The server keeps full details of its newest {} (--flow-history).",
                fmt_count(server as usize)
            ));
        }
        t
    };
    view! {
        <select
            class="keep"
            title=title
            prop:value=move || ctx.view.with(|v| v.keep_flows.to_string())
            on:change=move |ev| {
                if let Ok(n) = event_target_value(&ev).parse::<usize>() {
                    ctx.view.update(|v| v.keep_flows = n);
                }
            }
        >
            {KEEP_CHOICES
                .iter()
                .map(|&n| view! { <option value=n.to_string()>{format!("Keep {}", fmt_thousands(n))}</option> })
                .collect_view()}
        </select>
    }
}

fn kind_for_endpoint(id: &str, workload: &str) -> String {
    if let Some(e) = id.strip_prefix("reserved:") {
        return e.to_string();
    }
    // Without a pod; the workload is often still known from its identity.
    if is_unresolved(id) && workload.is_empty() {
        return "unknown".into();
    }
    kind_of_workload(workload).to_string()
}

fn endpoint_cell(ctx: Ctx, id: String, workload: String, extra: String) -> impl IntoView {
    let ns = namespace_of(&id).to_string();
    let name = short_name(&id).to_string();
    let icon = icons::svg(&kind_for_endpoint(&id, &workload));
    view! {
        <td class="ep">
            <span
                class="ep-main"
                on:click=move |ev| {
                    ev.stop_propagation();
                    ctx.show_on_map(&id);
                }
            >
                <span class="ico" inner_html=icon></span>
                <span class="name">{name}</span>
            </span>
            {(!ns.is_empty()).then(|| view! { <small>{ns}</small> })}
            {(!extra.is_empty()).then(|| view! { <small class="dns">{extra}</small> })}
        </td>
    }
}

fn cell(ctx: Ctx, col: Col, f: &FlowEvent) -> AnyView {
    let mono = |s: String| view! { <td class="mono">{s}</td> }.into_any();
    let muted = |s: String| view! { <td class="mono muted">{s}</td> }.into_any();
    match col {
        Col::Time => muted(fmt_time(f.time_ms)),
        Col::Source => endpoint_cell(ctx, f.source.clone(), f.src_workload.clone(), String::new()).into_any(),
        Col::Destination => {
            endpoint_cell(ctx, f.destination.clone(), f.dst_workload.clone(), f.dst_dns.clone()).into_any()
        }
        Col::SourceIp => mono(if f.src_port > 0 { format!("{}:{}", f.src_ip, f.src_port) } else { f.src_ip.clone() }),
        Col::DestinationIp => mono(f.dst_ip.clone()),
        Col::SourceIdentity => muted(id_text(f.src_identity)),
        Col::DestinationIdentity => muted(id_text(f.dst_identity)),
        Col::Port => mono(if f.dst_port > 0 { format!("{}/{}", f.protocol, f.dst_port) } else { f.protocol.clone() }),
        Col::TcpFlags => view! { <td class="mono flags">{f.tcp_flags.clone()}</td> }.into_any(),
        // Clip inside a div: max-width on the cell itself confuses table layout.
        Col::L7 => view! {
            <td class="mono">
                <div class="clip l7" title=f.l7.clone()>
                    {f.l7.clone()}
                </div>
            </td>
        }
        .into_any(),
        Col::Latency => mono(if f.latency_ms > 0.0 { format!("{:.1} ms", f.latency_ms) } else { String::new() }),
        Col::Verdict => {
            let reason = f.drop_reason.to_lowercase().replace('_', " ");
            view! {
                <td>
                    <span class=format!("pill v-{}", f.verdict.as_str())>{f.verdict.as_str()}</span>
                    {(!reason.is_empty()).then(|| view! { <small class="reason">{reason}</small> })}
                </td>
            }
            .into_any()
        }
        Col::Direction => muted(f.direction.clone()),
        Col::TracePoint => muted(f.trace_point.clone()),
        Col::Policy => view! {
            <td class="mono small">
                <div class="clip">{f.policies.join(", ")}</div>
            </td>
        }
        .into_any(),
        Col::Node => view! { <td class="muted small nowrap">{f.node.clone()}</td> }.into_any(),
    }
}

fn id_text(id: u32) -> String {
    if id == 0 {
        String::new()
    } else {
        id.to_string()
    }
}

const MIN_COLUMN: f64 = 48.0;
const MAX_COLUMN: f64 = 1600.0;

/// The flows table. Rows are keyed by flow id, so new flows don't redraw
/// the ones already on screen. Columns resize by dragging their right edge;
/// an empty last column takes up the room left over.
#[component]
pub fn FlowTable(rows: Memo<Rows>, filters: FlowFilters) -> impl IntoView {
    let ctx = use_ctx();
    let columns = Memo::new(move |_| ctx.view.with(|v| v.columns.clone()));
    let width = move |c: Col| ctx.view.with(|v| v.col_width(c));
    let more = move || {
        let (shown, total) = rows.with(|r| (r.flows.len(), r.total));
        (total > shown).then(|| {
            view! {
                <div class="table-more">
                    <span class="muted">{format!("Newest {} of {}", fmt_count(shown), fmt_count(total))}</span>
                    <button class="btn" on:click=move |_| filters.shown.update(|n| *n += filters.page)>
                        {format!("Show {} more", fmt_count(filters.page.min(total - shown)))}
                    </button>
                </div>
            }
        })
    };
    let empty = move || {
        rows.with(|r| r.total == 0).then(|| {
            let keep = ctx.view.with(|v| v.keep_flows);
            view! {
                <p class="empty">
                    {format!(
                        "No flows here yet. They appear as they happen, and the newest {} that match are kept.",
                        fmt_count(keep),
                    )}
                </p>
            }
        })
    };
    move || {
        let cols = columns.get();
        let header = cols
            .iter()
            .map(|&c| {
                view! {
                    <th style:width=move || format!("{}px", width(c)) title=c.title()>
                        {c.title()}
                        {grip(
                            "col-grip",
                            1.0,
                            MIN_COLUMN,
                            || MAX_COLUMN,
                            move |v| f64::from(v.col_width(c)),
                            move |v, w| v.set_col_width(c, w),
                        )}
                    </th>
                }
            })
            .collect_view();
        let all = cols.clone();
        let min_width = move || all.iter().map(|&c| width(c)).sum::<u32>();
        view! {
            <table class="flow-table fixed" style:min-width=move || format!("{}px", min_width())>
                <thead>
                    <tr>{header} <th class="fill"></th></tr>
                </thead>
                <tbody>
                    <For
                        each=move || rows.with(|r| r.flows.clone())
                        key=|f| f.id
                        children=move |f| {
                            let id = f.id;
                            let cells = cols.iter().map(|&c| cell(ctx, c, &f)).collect_view();
                            let dropped = matches!(f.verdict, Verdict::Dropped | Verdict::Error);
                            let reply = f.is_reply;
                            view! {
                                <tr
                                    class:dropped=dropped
                                    class:reply=reply
                                    class:row-sel=move || filters.open.with(|o| o.as_ref().is_some_and(|o| o.id == id))
                                    on:click=move |_| filters.open.set(Some(f.clone()))
                                >
                                    {cells}
                                    <td class="fill"></td>
                                </tr>
                            }
                        }
                    />
                </tbody>
            </table>
            {empty}
            {more}
        }
    }
}

/// Label chips; clicking one adds it to the filter (`prefix` is `from:`,
/// `to:` or empty).
pub fn labels_view(ctx: Ctx, labels: Vec<String>, prefix: &'static str) -> impl IntoView {
    view! {
        <div class="labels">
            {labels
                .into_iter()
                .map(|l| {
                    let short = l.strip_prefix("k8s:").unwrap_or(&l).to_string();
                    let term = format!("{prefix}label={short}");
                    view! {
                        <button
                            class="label-chip"
                            title=format!("{l}: click to filter")
                            on:click=move |_| ctx.add_filter(&term)
                        >
                            {short}
                        </button>
                    }
                })
                .collect_view()}
        </div>
    }
}

fn endpoint_section(ctx: Ctx, title: &'static str, e: EndpointDetail, prefix: &'static str) -> impl IntoView {
    let id = e.id.clone();
    let kind = kind_for_endpoint(&e.id, &e.workload);
    let icon = icons::svg(&kind);
    let row = |k: &'static str, v: String| {
        (!v.is_empty()).then(|| {
            view! {
                <span>{k}</span>
                <b>{v}</b>
            }
        })
    };
    view! {
        <h4>{title}</h4>
        <div class="ep-head">
            <span class="ico" inner_html=icon></span>
            <b>{short_name(&e.id).to_string()}</b>
            <button class="link" on:click=move |_| ctx.show_on_map(&id)>
                "Show on map"
            </button>
        </div>
        <div class="kv">
            {row("namespace", e.namespace)} {row("workload", e.workload)}
            {row("IP", if e.port > 0 { format!("{}:{}", e.ip, e.port) } else { e.ip })}
            {row("identity", id_text(e.identity))} {row("service", e.service)} {row("DNS names", e.names.join(", "))}
        </div>
        {(!e.labels.is_empty()).then(|| labels_view(ctx, e.labels, prefix))}
    }
}

/// What the table row knows about a flow, for when the server no longer
/// has the full record. Labels are the endpoints' current ones.
fn partial_detail(e: &FlowEvent, ix: &EpIndex) -> FlowDetail {
    let side = |id: &str, workload: &str, ip: &str, port: u32, identity: u32, dns: &str| EndpointDetail {
        id: id.into(),
        namespace: namespace_of(id).into(),
        pod: if namespace_of(id).is_empty() { String::new() } else { short_name(id).into() },
        workload: workload.into(),
        identity,
        ip: ip.into(),
        port,
        labels: ix.labels_for(id).into_owned(),
        service: String::new(),
        names: if dns.is_empty() { Vec::new() } else { vec![dns.into()] },
    };
    let (denied, allowed): (Vec<String>, Vec<String>) =
        e.policies.iter().cloned().partition(|p| p.starts_with("deny "));
    FlowDetail {
        event: e.clone(),
        source: side(&e.source, &e.src_workload, &e.src_ip, e.src_port, e.src_identity, ""),
        destination: side(&e.destination, &e.dst_workload, &e.dst_ip, e.dst_port, e.dst_identity, &e.dst_dns),
        allowed_by: allowed,
        denied_by: denied.iter().map(|p| p.trim_start_matches("deny ").to_string()).collect(),
        ..FlowDetail::default()
    }
}

/// The details of one flow. `partial`: only what the table row had.
fn detail_view(ctx: Ctx, d: FlowDetail, partial: bool) -> impl IntoView {
    let e = d.event.clone();
    let row = |k: &'static str, v: String| {
        (!v.is_empty()).then(|| {
            view! {
                <span>{k}</span>
                <b>{v}</b>
            }
        })
    };
    let l7 = d.l7.clone().map(|l| {
        let is_dns = l.kind == "DNS";
        view! {
            <h4>{format!("L7 · {}", l.kind)}</h4>
            <div class="kv">
                {row("type", l.flow_type)}
                {row("latency", if l.latency_ms > 0.0 { format!("{:.2} ms", l.latency_ms) } else { String::new() })}
                {row("method", l.http_method)} {row("URL", l.http_url)}
                {row("status", if l.http_code > 0 { l.http_code.to_string() } else { String::new() })}
                {row("protocol", l.http_protocol)} {row("query", l.dns_query)} {row("answers", l.dns_ips.join(", "))}
                {row("query types", l.dns_qtypes.join(", "))} {row("CNAMEs", l.dns_cnames.join(", "))}
                {is_dns
                    .then(|| {
                        view! {
                            <span>"rcode"</span>
                            <b>{l.dns_rcode}</b>
                            <span>"TTL"</span>
                            <b>{l.dns_ttl}</b>
                        }
                    })}
            </div>
        }
    });
    // The row's one-line L7 summary, when the full record (and its L7 part) is gone.
    let l7_summary = if d.l7.is_none() { e.l7.clone() } else { String::new() };
    let latency = if partial && e.latency_ms > 0.0 { format!("{:.1} ms", e.latency_ms) } else { String::new() };
    let note = partial.then(|| {
        let kept = ctx.info.with(|i| i.flow_history);
        let kept = if kept > 0 { format!("its newest {}", fmt_count(kept as usize)) } else { "only its newest".into() };
        view! {
            <p class="note">
                {format!(
                    "The server no longer has this flow's full record: it keeps {kept} (raise --flow-history to keep more). This is what the table has.",
                )}
            </p>
        }
    });
    view! {
        {note}
        <div class="fd-summary">
            <span class=format!("pill v-{}", e.verdict.as_str())>{e.verdict.as_str()}</span>
            <span class="mono muted">{fmt_time(e.time_ms)}</span>
            {e.is_reply.then(|| view! { <span class="badge">"reply"</span> })}
            {d.encrypted.then(|| view! { <span class="badge">"encrypted"</span> })}
        </div>
        {endpoint_section(ctx, "Source", d.source, "from:")}
        {endpoint_section(ctx, "Destination", d.destination, "to:")}
        <h4>"Connection"</h4>
        <div class="kv">
            {row(
                "protocol",
                if e.dst_port > 0 { format!("{}/{}", e.protocol, e.dst_port) } else { e.protocol.clone() },
            )} {row("TCP flags", e.tcp_flags.clone())} {row("direction", e.direction.clone())}
            {row("observed at", e.trace_point.clone())} {row("trace reason", d.trace_reason)}
            {row("IP version", d.ip_version)} {row("interface", d.interface)} {row("node", e.node.clone())}
            {row("L7", l7_summary)} {row("latency", latency)} {row("UUID", d.uuid)}
        </div>
        {l7}
        {(!d.allowed_by.is_empty() || !d.denied_by.is_empty() || !e.drop_reason.is_empty())
            .then(|| {
                view! {
                    <h4>"Policy"</h4>
                    <div class="kv">
                        {row("allowed by", d.allowed_by.join(", "))} {row("denied by", d.denied_by.join(", "))}
                        {row("drop reason", e.drop_reason.to_lowercase().replace('_', " "))}
                    </div>
                }
            })}
    }
}

/// Full detail of the open flow, fetched from the backend.
#[component]
pub fn FlowDetailPanel(filters: FlowFilters) -> impl IntoView {
    let ctx = use_ctx();
    // The flow the answer is for, and the record (`None`: the server no longer has it).
    let detail = RwSignal::new(None::<(Flow, Option<FlowDetail>)>);
    Effect::new(move |_| {
        let Some(f) = filters.open.get() else {
            detail.set(None);
            return;
        };
        spawn_local(async move {
            // The server only keeps its newest flows, and after it restarts
            // an id can name another flow: check that it's this one.
            let d = get_json::<FlowDetail>(&format!("/api/flows/{}", f.id)).await.ok().filter(|d| {
                d.event.time_ms == f.time_ms && d.event.source == f.source && d.event.destination == f.destination
            });
            // Ignore late answers for a flow that is no longer open.
            if filters.open.with_untracked(|o| o.as_ref().is_some_and(|o| Arc::ptr_eq(o, &f))) {
                detail.set(Some((f, d)));
            }
        });
    });

    move || {
        let open = filters.open.get()?;
        let body = match detail.get() {
            Some((f, Some(d))) if Arc::ptr_eq(&f, &open) => detail_view(ctx, d, false).into_any(),
            Some((f, None)) if Arc::ptr_eq(&f, &open) => {
                let d = ctx.index.with_untracked(|ix| partial_detail(&f, ix));
                detail_view(ctx, d, true).into_any()
            }
            _ => view! { <p class="muted">"Loading…"</p> }.into_any(),
        };
        Some(view! {
            <SidePanel>
                <div class="details-head">
                    <div class="title">"Flow details"</div>
                    <button class="x" on:click=move |_| filters.open.set(None)>
                        "✕"
                    </button>
                </div>
                {body}
            </SidePanel>
        })
    }
}

/// Rows a table shows at first, and adds with each "Show more".
const PAGE_ROWS: usize = 300;

#[component]
pub fn FlowsTab() -> impl IntoView {
    let ctx = use_ctx();
    let filters = FlowFilters::new(PAGE_ROWS);
    let rows = Memo::new(move |_| ctx.store.with(|s| table_rows(s.filtered(), filters)));

    view! {
        <div class="tab-body flows-body">
            <div class="toolbar">
                <FilterBar />
                <FlowFilterControls filters=filters />
                <ColumnsMenu />
                <KeepSelect />
                <PauseButton />
                <button
                    class="btn"
                    title="Forget the flows kept so far"
                    on:click=move |_| ctx.store.update(FlowStore::clear)
                >
                    "Clear"
                </button>
                <span class="muted">
                    {move || {
                        rows.with(|r| {
                            if r.total > r.flows.len() {
                                format!("{} of {} shown", fmt_count(r.flows.len()), fmt_count(r.total))
                            } else {
                                format!("{} shown", fmt_count(r.total))
                            }
                        })
                    }}
                </span>
            </div>
            <div class="flows-split" class:has-open=move || filters.open.with(Option::is_some)>
                <div class="table-wrap">
                    <FlowTable rows=rows filters=filters />
                </div>
                <FlowDetailPanel filters=filters />
            </div>
        </div>
    }
}

#[component]
pub fn PauseButton() -> impl IntoView {
    let ctx = use_ctx();
    view! {
        <button
            class="btn"
            class:active=move || ctx.paused.get()
            title="Freeze the table; the map keeps updating"
            on:click=move |_| ctx.paused.update(|p| *p = !*p)
        >
            {move || if ctx.paused.get() { "Resume" } else { "Pause" }}
        </button>
    }
}
