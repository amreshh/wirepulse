//! Endpoints tab: every endpoint Cilium manages, with identity, labels,
//! addresses, state, policy enforcement and live traffic.

use crate::app::{load_endpoints, use_ctx, Ctx, Tab};
use crate::filter_bar::FilterBar;
use crate::flows::labels_view;
use crate::icons;
use crate::util::fmt_rate;
use crate::yaml::Yaml;
use common::aggregate::kind_of_workload;
use common::filter::EndpointRef;
use common::EndpointInfo;
use leptos::prelude::*;
use leptos::task::spawn_local;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Name,
    EndpointId,
    Identity,
    Node,
    State,
    Traffic,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Enforcement {
    Any,
    Ingress,
    Egress,
    None,
}

/// Flows/s in, out and dropped per pod id.
type Traffic = HashMap<String, (f32, f32, f32)>;

fn traffic(ctx: Ctx) -> Traffic {
    ctx.graph.with(|g| {
        let mut t: Traffic = HashMap::new();
        for e in &g.edges {
            t.entry(e.source.clone()).or_default().1 += e.flows_per_sec;
            let d = t.entry(e.target.clone()).or_default();
            d.0 += e.flows_per_sec;
            d.2 += e.drops_per_sec;
        }
        t
    })
}

/// Labels worth showing in the table; namespace and cluster are their own
/// columns or the same everywhere.
fn interesting(l: &str) -> bool {
    !(l.contains("io.kubernetes.pod.namespace")
        || l.contains("io.cilium.k8s.policy.cluster")
        || l.contains("io.cilium.k8s.policy.serviceaccount")
        || l.contains("io.cilium.k8s.namespace.labels"))
}

fn enforcement_badge(v: Option<bool>) -> impl IntoView {
    match v {
        Some(true) => view! {
            <span class="badge enf on" title="policy enforced">
                "enforced"
            </span>
        }
        .into_any(),
        Some(false) => view! {
            <span class="badge enf off" title="all traffic allowed">
                "open"
            </span>
        }
        .into_any(),
        None => view! { <span class="muted">"–"</span> }.into_any(),
    }
}

#[component]
pub fn EndpointsTab() -> impl IntoView {
    let ctx = use_ctx();
    load_endpoints(ctx);
    let state_filter = RwSignal::new(String::new());
    let enforcement = RwSignal::new(Enforcement::Any);
    let sort = RwSignal::new((SortKey::Name, true));
    let open = RwSignal::new(None::<String>);
    let traffic = Memo::new(move |_| traffic(ctx));

    let rows = Memo::new(move |_| {
        let filter = ctx.parsed.get();
        let st = state_filter.get();
        let enf = enforcement.get();
        let (key, asc) = sort.get();
        let tr = traffic.get();
        let mut v: Vec<EndpointInfo> = ctx.endpoints.with(|view| {
            view.endpoints
                .iter()
                .filter(|e| match st.as_str() {
                    "" => true,
                    "not-ready" => e.state != "ready",
                    s => e.state == s,
                })
                .filter(|e| match enf {
                    Enforcement::Any => true,
                    Enforcement::Ingress => e.ingress_enforcing == Some(true),
                    Enforcement::Egress => e.egress_enforcing == Some(true),
                    Enforcement::None => e.ingress_enforcing != Some(true) && e.egress_enforcing != Some(true),
                })
                .filter(|e| {
                    filter.matches_endpoint(&EndpointRef {
                        id: &e.id,
                        workload: &e.workload,
                        ip: &e.ipv4,
                        identity: e.identity as u32,
                        labels: &e.labels,
                        dns: "",
                    })
                })
                .cloned()
                .collect()
        });
        let rate = |e: &EndpointInfo| tr.get(&e.id).map_or(0.0, |t| t.0 + t.1 + t.2);
        v.sort_by(|a, b| {
            let o = match key {
                SortKey::Name => a.id.cmp(&b.id),
                SortKey::EndpointId => a.endpoint_id.cmp(&b.endpoint_id),
                SortKey::Identity => a.identity.cmp(&b.identity),
                SortKey::Node => (&a.node, &a.id).cmp(&(&b.node, &b.id)),
                SortKey::State => (&a.state, &a.id).cmp(&(&b.state, &b.id)),
                SortKey::Traffic => rate(a).total_cmp(&rate(b)),
            };
            if asc {
                o
            } else {
                o.reverse()
            }
        });
        v
    });

    let th = move |label: &'static str, key: SortKey| {
        view! {
            <th
                class="sortable"
                on:click=move |_| {
                    sort.update(|(k, asc)| {
                        if *k == key {
                            *asc = !*asc;
                        } else {
                            *k = key;
                            *asc = key != SortKey::Traffic;
                        }
                    })
                }
            >
                {label}
                {move || {
                    let (k, asc) = sort.get();
                    (k == key).then_some(if asc { " ▲" } else { " ▼" })
                }}
            </th>
        }
    };

    view! {
        <div class="tab-body endpoints-body">
            <div class="toolbar">
                <FilterBar />
                <select
                    on:change=move |ev| state_filter.set(event_target_value(&ev))
                    prop:value=move || state_filter.get()
                >
                    <option value="">"Any state"</option>
                    <option value="ready">"Ready"</option>
                    <option value="not-ready">"Not ready"</option>
                </select>
                <select on:change=move |ev| {
                    enforcement
                        .set(
                            match event_target_value(&ev).as_str() {
                                "ingress" => Enforcement::Ingress,
                                "egress" => Enforcement::Egress,
                                "none" => Enforcement::None,
                                _ => Enforcement::Any,
                            },
                        )
                }>
                    <option value="any">"Any enforcement"</option>
                    <option value="ingress">"Ingress enforced"</option>
                    <option value="egress">"Egress enforced"</option>
                    <option value="none">"Not enforced"</option>
                </select>
                <span class="muted">
                    {move || {
                        let total = ctx.endpoints.with(|v| v.endpoints.len());
                        let src = ctx.endpoints.with(|v| v.source.clone());
                        let shown = rows.with(Vec::len);
                        let from = if src.is_empty() { String::new() } else { format!(" · from {src}") };
                        if shown == total {
                            format!("{total} endpoints{from}")
                        } else {
                            format!("{shown} of {total} endpoints{from}")
                        }
                    }}
                </span>
            </div>
            <div class="flows-split" class:has-open=move || open.get().is_some()>
                <div class="table-wrap">
                    <table class="flow-table ep-table">
                        <thead>
                            <tr>
                                {th("Endpoint", SortKey::Name)} {th("ID", SortKey::EndpointId)}
                                {th("Identity", SortKey::Identity)} <th>"Labels"</th> <th>"IPs"</th>
                                {th("Node", SortKey::Node)} {th("State", SortKey::State)} <th>"Ingress"</th>
                                <th>"Egress"</th> {th("Traffic", SortKey::Traffic)}
                            </tr>
                        </thead>
                        <tbody>
                            {move || {
                                let tr = traffic.get();
                                if rows.with(Vec::is_empty) {
                                    let msg = if ctx.endpoints.with(|v| v.endpoints.is_empty()) {
                                        "No Cilium endpoints found yet. They come from CiliumEndpoint objects (or CiliumEndpointSlices)."
                                    } else {
                                        "No endpoints match the filter."
                                    };
                                    return view! {
                                        <tr>
                                            <td colspan="10" class="empty">
                                                {msg}
                                            </td>
                                        </tr>
                                    }
                                        .into_any();
                                }
                                rows.get()
                                    .into_iter()
                                    .map(|e| {
                                        let id = e.id.clone();
                                        let id_sel = e.id.clone();
                                        let (tin, tout, tdrop) = tr.get(&e.id).copied().unwrap_or_default();
                                        let shown: Vec<String> = e
                                            .labels
                                            .iter()
                                            .filter(|l| interesting(l))
                                            .cloned()
                                            .collect();
                                        let more = shown.len().saturating_sub(3);
                                        let state_ok = e.state == "ready";
                                        view! {
                                            <tr
                                                class:row-sel=move || open.get().as_deref() == Some(id_sel.as_str())
                                                on:click=move |_| open.set(Some(id.clone()))
                                            >
                                                <td class="ep">
                                                    <span class="ep-main">
                                                        <span
                                                            class="ico"
                                                            inner_html=icons::svg(kind_of_workload(&e.workload))
                                                        ></span>
                                                        <span class="name">{e.name.clone()}</span>
                                                    </span>
                                                    <small>{e.namespace.clone()}</small>
                                                </td>
                                                <td class="mono muted">
                                                    {if e.endpoint_id > 0 {
                                                        e.endpoint_id.to_string()
                                                    } else {
                                                        "–".into()
                                                    }}
                                                </td>
                                                <td class="mono">{e.identity}</td>
                                                <td class="lbls">
                                                    {shown
                                                        .into_iter()
                                                        .take(3)
                                                        .map(|l| {
                                                            let short = l
                                                                .strip_prefix("k8s:")
                                                                .unwrap_or(&l)
                                                                .to_string();
                                                            view! { <span class="label-chip">{short}</span> }
                                                        })
                                                        .collect_view()}
                                                    {(more > 0)
                                                        .then(|| {
                                                            view! {
                                                                <span class="muted small">{format!(" +{more}")}</span>
                                                            }
                                                        })}
                                                </td>
                                                <td class="mono">
                                                    {e.ipv4.clone()}
                                                    {(!e.ipv6.is_empty())
                                                        .then(|| {
                                                            view! { <small class="muted">{e.ipv6.clone()}</small> }
                                                        })}
                                                </td>
                                                <td class="mono muted nowrap">{e.node.clone()}</td>
                                                <td>
                                                    <span
                                                        class="pill"
                                                        class:v-forwarded=state_ok
                                                        class:v-audit=!state_ok
                                                    >
                                                        {e.state.clone()}
                                                    </span>
                                                </td>
                                                <td>{enforcement_badge(e.ingress_enforcing)}</td>
                                                <td>{enforcement_badge(e.egress_enforcing)}</td>
                                                <td class="mono nowrap">
                                                    {format!("↓{} ↑{}", fmt_rate(tin), fmt_rate(tout))}
                                                    {(tdrop > 0.0)
                                                        .then(|| {
                                                            view! {
                                                                <small class="reason">
                                                                    {format!("{} dropped", fmt_rate(tdrop))}
                                                                </small>
                                                            }
                                                        })}
                                                </td>
                                            </tr>
                                        }
                                    })
                                    .collect_view()
                                    .into_any()
                            }}
                        </tbody>
                    </table>
                </div>
                <EndpointDetail open=open traffic=traffic />
            </div>
        </div>
    }
}

#[component]
fn EndpointDetail(open: RwSignal<Option<String>>, traffic: Memo<Traffic>) -> impl IntoView {
    let ctx = use_ctx();
    let yaml = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        let Some(id) = open.get() else { return };
        yaml.set(None);
        let Some((ns, name)) = id.split_once('/').map(|(a, b)| (a.to_string(), b.to_string())) else { return };
        spawn_local(async move {
            let r = gloo_net::http::Request::get(&format!("/api/endpoints/{ns}/{name}")).send().await;
            let text = match r {
                Ok(r) if r.ok() => r.text().await.unwrap_or_default(),
                _ => "# not available".into(),
            };
            if open.get_untracked() == Some(id) {
                yaml.set(Some(text));
            }
        });
    });

    // The open endpoint. The list is fetched again every 10 s; a memo changes
    // (and redraws the panel) only when this endpoint does.
    let endpoint = Memo::new(move |_| {
        let id = open.get()?;
        ctx.endpoints.with(|v| v.endpoints.iter().find(|e| e.id == id).cloned())
    });

    move || {
        let e = endpoint.get()?;
        let row = |k: &'static str, v: String| {
            (!v.is_empty()).then(|| {
                view! {
                    <span>{k}</span>
                    <b>{v}</b>
                }
            })
        };
        let enf = |v: Option<bool>| match v {
            Some(true) => "enforced".to_string(),
            Some(false) => "not enforced (all allowed)".to_string(),
            None => String::new(),
        };
        let (id_map, id_flows, id_traffic) = (e.id.clone(), e.id.clone(), e.id.clone());
        Some(view! {
            <aside class="flow-detail">
                <div class="details-head">
                    <div class="head-main">
                        <span class="ico big" inner_html=icons::svg(kind_of_workload(&e.workload))></span>
                        <div>
                            <div class="title">{e.name.clone()}</div>
                            <div class="sub">
                                {format!("{} · namespace {}", kind_of_workload(&e.workload), e.namespace)}
                            </div>
                        </div>
                    </div>
                    <button class="x" on:click=move |_| open.set(None)>
                        "✕"
                    </button>
                </div>
                <div class="ep-actions">
                    <button class="btn" on:click=move |_| ctx.show_on_map(&id_map)>
                        "Show on map"
                    </button>
                    <button
                        class="btn"
                        on:click=move |_| {
                            ctx.filter.set(format!("pod={id_flows}"));
                            ctx.tab.set(Tab::Flows);
                        }
                    >
                        "Show flows"
                    </button>
                </div>
                <div class="kv">
                    {row("endpoint ID", if e.endpoint_id > 0 { e.endpoint_id.to_string() } else { String::new() })}
                    {row("identity", e.identity.to_string())} {row("IPv4", e.ipv4.clone())}
                    {row("IPv6", e.ipv6.clone())} {row("node", e.node.clone())} {row("state", e.state.clone())}
                    {row("ingress", enf(e.ingress_enforcing))} {row("egress", enf(e.egress_enforcing))}
                    {row("workload", e.workload.clone())} {row("service account", e.service_account.clone())}
                    {row("named ports", e.named_ports.join(", "))} <span>"traffic"</span>
                    // The rates change every 2 s, and only this text follows them: the rest of the
                    // panel isn't built again, so text selected in its YAML stays selected.
                    <b>
                        {move || {
                            let (tin, tout, tdrop) = traffic.with(|t| t.get(&id_traffic).copied().unwrap_or_default());
                            format!("in {} · out {} · dropped {}", fmt_rate(tin), fmt_rate(tout), fmt_rate(tdrop))
                        }}
                    </b>
                </div>
                <h4>"Identity labels"</h4>
                <p class="muted small">"Click a label to filter by it."</p>
                {labels_view(ctx, e.labels.clone(), "")}
                <h4>"Object"</h4>
                {move || match yaml.get() {
                    Some(text) => view! { <Yaml text=text /> }.into_any(),
                    None => view! { <pre class="yaml">"Loading…"</pre> }.into_any(),
                }}
            </aside>
        })
    }
}
