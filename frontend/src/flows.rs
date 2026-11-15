//! Flows: filters, a configurable table and a flow details panel. Used by the
//! Flows tab and by the traffic panel under the map.

use crate::app::{use_ctx, Ctx, EpIndex};
use crate::filter_bar::FilterBar;
use crate::icons;
use crate::settings::Col;
use crate::util::{fmt_time, get_json, namespace_of, short_name};
use common::aggregate::kind_of_workload;
use common::filter::{EndpointRef, Filter, FlowRef};
use common::{EndpointDetail, FlowDetail, FlowEvent, Verdict};
use leptos::prelude::*;
use leptos::task::spawn_local;
use std::collections::HashSet;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VerdictFilter {
    All,
    Forwarded,
    Dropped,
    L7,
}

/// Table-local filter state (the text filter is global, in `Ctx`); every
/// field is a signal, so this is `Copy`.
#[derive(Clone, Copy)]
pub struct FlowFilters {
    pub verdict: RwSignal<VerdictFilter>,
    pub hide_replies: RwSignal<bool>,
    /// Flow whose details are open.
    pub open: RwSignal<Option<u64>>,
}

impl FlowFilters {
    pub fn new() -> Self {
        Self {
            verdict: RwSignal::new(VerdictFilter::All),
            hide_replies: RwSignal::new(true),
            open: RwSignal::new(None),
        }
    }
}

/// Narrows flows to part of the map: a selected node's pods, or one
/// connection (`ids` -> `peer`, either direction).
#[derive(Clone, Debug, PartialEq)]
pub struct Scope {
    pub label: String,
    pub ids: HashSet<String>,
    pub peer: Option<(String, HashSet<String>)>,
}

impl Scope {
    fn contains(&self, f: &FlowEvent) -> bool {
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

/// Flows that pass the filters and lie in `scope` (if given), newest first.
pub fn filtered(ctx: Ctx, f: FlowFilters, scope: Option<&Scope>, limit: usize) -> Vec<FlowEvent> {
    let v = f.verdict.get();
    let hide = f.hide_replies.get();
    let filter = ctx.parsed.get();
    ctx.index.with(|ix| {
        ctx.flows.with(|fl| {
            fl.iter()
                .filter(|e| !(hide && e.is_reply))
                .filter(|e| match v {
                    VerdictFilter::Dropped => matches!(e.verdict, Verdict::Dropped | Verdict::Error),
                    VerdictFilter::Forwarded => e.verdict == Verdict::Forwarded,
                    VerdictFilter::L7 => !e.l7.is_empty(),
                    VerdictFilter::All => true,
                })
                .filter(|e| scope.is_none_or(|s| s.contains(e)))
                .filter(|e| flow_matches(&filter, ix, e))
                .take(limit)
                .cloned()
                .collect()
        })
    })
}

/// Verdict filter and "hide replies".
#[component]
pub fn FlowFilterControls(filters: FlowFilters) -> impl IntoView {
    view! {
        <select
            on:change=move |ev| filters.verdict.set(match event_target_value(&ev).as_str() {
                "forwarded" => VerdictFilter::Forwarded,
                "dropped" => VerdictFilter::Dropped,
                "l7" => VerdictFilter::L7,
                _ => VerdictFilter::All,
            })
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
        <div class="menu-wrap">
            <button class="btn" class:active=move || open.get() on:click=move |_| open.update(|o| *o = !*o)>
                "Columns"
            </button>
            {move || open.get().then(|| view! {
                <div class="backdrop" on:click=move |_| open.set(false)></div>
                <div class="menu columns-menu">
                    {Col::ALL.iter().map(|&c| view! {
                        <label class="chk">
                            <input
                                type="checkbox"
                                prop:checked=move || ctx.view.with(|v| v.columns.contains(&c))
                                on:change=move |ev| {
                                    let on = event_target_checked(&ev);
                                    ctx.view.update(|v| {
                                        v.columns.retain(|x| *x != c);
                                        if on {
                                            v.columns.push(c);
                                            // Keep the canonical column order.
                                            v.columns.sort_by_key(|x| Col::ALL.iter().position(|y| y == x));
                                        }
                                    });
                                }
                            />
                            {c.title()}
                        </label>
                    }).collect_view()}
                </div>
            })}
        </div>
    }
}

fn kind_for_endpoint(id: &str, workload: &str) -> String {
    if let Some(e) = id.strip_prefix("reserved:") {
        return e.to_string();
    }
    if id.starts_with("identity:") {
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
            <span class="ep-main" on:click=move |ev| {
                ev.stop_propagation();
                ctx.show_on_map(&id);
            }>
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
        Col::Destination => endpoint_cell(ctx, f.destination.clone(), f.dst_workload.clone(), f.dst_dns.clone()).into_any(),
        Col::SourceIp => mono(if f.src_port > 0 { format!("{}:{}", f.src_ip, f.src_port) } else { f.src_ip.clone() }),
        Col::DestinationIp => mono(f.dst_ip.clone()),
        Col::SourceIdentity => muted(id_text(f.src_identity)),
        Col::DestinationIdentity => muted(id_text(f.dst_identity)),
        Col::Port => mono(if f.dst_port > 0 { format!("{}/{}", f.protocol, f.dst_port) } else { f.protocol.clone() }),
        Col::TcpFlags => view! { <td class="mono flags">{f.tcp_flags.clone()}</td> }.into_any(),
        // Clip inside a div: max-width on the cell itself confuses table layout.
        Col::L7 => view! { <td class="mono"><div class="clip l7" title=f.l7.clone()>{f.l7.clone()}</div></td> }.into_any(),
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
        Col::Policy => view! { <td class="mono small"><div class="clip">{f.policies.join(", ")}</div></td> }.into_any(),
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

/// The flows table. Rows are keyed by flow id, so new flows don't redraw
/// the ones already on screen.
#[component]
pub fn FlowTable(#[prop(into)] rows: Signal<Vec<FlowEvent>>, filters: FlowFilters) -> impl IntoView {
    let ctx = use_ctx();
    let columns = Memo::new(move |_| ctx.view.with(|v| v.columns.clone()));
    move || {
        let cols = columns.get();
        let header = cols
            .iter()
            .map(|c| view! { <th style:width=format!("{}px", c.width()) title=c.title()>{c.title()}</th> })
            .collect_view();
        let min_width: u32 = cols.iter().map(|c| c.width()).sum();
        view! {
            <table class="flow-table fixed" style:min-width=format!("{min_width}px")>
                <thead><tr>{header}</tr></thead>
                <tbody>
                    <For
                        each=move || rows.get()
                        key=|f| f.id
                        children=move |f| {
                            let id = f.id;
                            let cells = cols.iter().map(|&c| cell(ctx, c, &f)).collect_view();
                            view! {
                                <tr
                                    class:dropped=matches!(f.verdict, Verdict::Dropped | Verdict::Error)
                                    class:reply=f.is_reply
                                    class:row-sel=move || filters.open.get() == Some(id)
                                    on:click=move |_| filters.open.set(Some(id))
                                >
                                    {cells}
                                </tr>
                            }
                        }
                    />
                </tbody>
            </table>
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
                        <button class="label-chip" title=format!("{l}: click to filter") on:click=move |_| ctx.add_filter(&term)>
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
    let row = |k: &'static str, v: String| (!v.is_empty()).then(|| view! { <span>{k}</span><b>{v}</b> });
    view! {
        <h4>{title}</h4>
        <div class="ep-head">
            <span class="ico" inner_html=icon></span>
            <b>{short_name(&e.id).to_string()}</b>
            <button class="link" on:click=move |_| ctx.show_on_map(&id)>"Show on map"</button>
        </div>
        <div class="kv">
            {row("namespace", e.namespace)}
            {row("workload", e.workload)}
            {row("IP", if e.port > 0 { format!("{}:{}", e.ip, e.port) } else { e.ip })}
            {row("identity", id_text(e.identity))}
            {row("service", e.service)}
            {row("DNS names", e.names.join(", "))}
        </div>
        {(!e.labels.is_empty()).then(|| labels_view(ctx, e.labels, prefix))}
    }
}

/// Full detail of the open flow, fetched from the backend.
#[component]
pub fn FlowDetailPanel(filters: FlowFilters) -> impl IntoView {
    let ctx = use_ctx();
    let detail = RwSignal::new(None::<Result<FlowDetail, String>>);
    Effect::new(move |_| {
        let Some(id) = filters.open.get() else {
            detail.set(None);
            return;
        };
        spawn_local(async move {
            let r = get_json::<FlowDetail>(&format!("/api/flows/{id}")).await.map_err(|_| {
                "This flow is no longer kept by the server (only the most recent ones are).".to_string()
            });
            // Ignore late answers for a flow that is no longer open.
            if filters.open.get_untracked() == Some(id) {
                detail.set(Some(r));
            }
        });
    });

    move || {
        filters.open.get()?;
        let body = match detail.get() {
            None => view! { <p class="muted">"Loading…"</p> }.into_any(),
            Some(Err(e)) => view! { <p class="muted">{e}</p> }.into_any(),
            Some(Ok(d)) => {
                let e = d.event.clone();
                let row = |k: &'static str, v: String| (!v.is_empty()).then(|| view! { <span>{k}</span><b>{v}</b> });
                let l7 = d.l7.clone().map(|l| {
                    let is_dns = l.kind == "DNS";
                    view! {
                        <h4>{format!("L7 · {}", l.kind)}</h4>
                        <div class="kv">
                            {row("type", l.flow_type)}
                            {row("latency", if l.latency_ms > 0.0 { format!("{:.2} ms", l.latency_ms) } else { String::new() })}
                            {row("method", l.http_method)}
                            {row("URL", l.http_url)}
                            {row("status", if l.http_code > 0 { l.http_code.to_string() } else { String::new() })}
                            {row("protocol", l.http_protocol)}
                            {row("query", l.dns_query)}
                            {row("answers", l.dns_ips.join(", "))}
                            {row("query types", l.dns_qtypes.join(", "))}
                            {row("CNAMEs", l.dns_cnames.join(", "))}
                            {is_dns.then(|| view! {
                                <span>"rcode"</span><b>{l.dns_rcode}</b>
                                <span>"TTL"</span><b>{l.dns_ttl}</b>
                            })}
                        </div>
                    }
                });
                view! {
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
                        {row("protocol", if e.dst_port > 0 { format!("{}/{}", e.protocol, e.dst_port) } else { e.protocol.clone() })}
                        {row("TCP flags", e.tcp_flags.clone())}
                        {row("direction", e.direction.clone())}
                        {row("observed at", e.trace_point.clone())}
                        {row("trace reason", d.trace_reason)}
                        {row("IP version", d.ip_version)}
                        {row("interface", d.interface)}
                        {row("node", e.node.clone())}
                        {row("UUID", d.uuid)}
                    </div>
                    {l7}
                    {(!d.allowed_by.is_empty() || !d.denied_by.is_empty() || !e.drop_reason.is_empty()).then(|| view! {
                        <h4>"Policy"</h4>
                        <div class="kv">
                            {row("allowed by", d.allowed_by.join(", "))}
                            {row("denied by", d.denied_by.join(", "))}
                            {row("drop reason", e.drop_reason.to_lowercase().replace('_', " "))}
                        </div>
                    })}
                }
                .into_any()
            }
        };
        Some(view! {
            <aside class="flow-detail">
                <div class="details-head">
                    <div class="title">"Flow details"</div>
                    <button class="x" on:click=move |_| filters.open.set(None)>"✕"</button>
                </div>
                {body}
            </aside>
        })
    }
}

const MAX_ROWS: usize = 300;

#[component]
pub fn FlowsTab() -> impl IntoView {
    let ctx = use_ctx();
    let filters = FlowFilters::new();
    let rows = Memo::new(move |_| filtered(ctx, filters, None, MAX_ROWS));

    view! {
        <div class="tab-body flows-body">
            <div class="toolbar">
                <FilterBar/>
                <FlowFilterControls filters=filters/>
                <ColumnsMenu/>
                <PauseButton/>
                <button class="btn" on:click=move |_| ctx.flows.update(|f| f.clear())>"Clear"</button>
                <span class="muted">{move || format!("{} shown", rows.with(Vec::len))}</span>
            </div>
            <div class="flows-split" class:has-open=move || filters.open.get().is_some()>
                <div class="table-wrap">
                    <FlowTable rows=rows filters=filters/>
                </div>
                <FlowDetailPanel filters=filters/>
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
