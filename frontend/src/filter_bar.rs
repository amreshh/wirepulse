//! Hubble-style filter bar: one chip per term, suggestions while typing,
//! syntax help. Edits `Ctx::filter`, which every view reads.

use crate::app::{use_ctx, Ctx};
use crate::widgets::Dropdown;
use common::filter::{Filter, Kind, Side, KINDS};
use leptos::prelude::*;
use std::collections::BTreeSet;
use web_sys::KeyboardEvent;

const MAX_SUGGESTIONS: usize = 10;

/// Splits `!from:label=app=web` into (`!from:`, `label`, Some(`app=web`)).
fn split_draft(d: &str) -> (String, &str, Option<&str>) {
    let (neg, rest) = match d.strip_prefix('!') {
        Some(r) => ("!", r),
        None => ("", d),
    };
    let mut side = "";
    let mut rest = rest;
    for p in ["from:", "to:", "from-", "to-"] {
        if let Some(r) = rest.strip_prefix(p) {
            side = p;
            rest = r;
            break;
        }
    }
    let prefix = format!("{neg}{side}");
    match rest.split_once('=') {
        Some((k, v)) => (prefix, k, Some(v)),
        None => (prefix, rest, None),
    }
}

/// Values seen in the live data for one filter kind.
fn candidates(ctx: Ctx, kind: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    match kind {
        "label" | "labels" => ctx.index.with(|ix| {
            for l in ix.labels.values().flatten() {
                out.insert(l.strip_prefix("k8s:").unwrap_or(l).to_string());
            }
        }),
        "pod" => ctx.graph.with(|g| out.extend(g.nodes.iter().map(|n| n.id.clone()))),
        "ns" | "namespace" => ctx.graph.with(|g| {
            out.extend(g.nodes.iter().filter(|n| n.kind == common::NodeKind::Pod).map(|n| n.namespace.clone()))
        }),
        "workload" | "wl" => ctx.graph.with(|g| {
            out.extend(
                g.nodes
                    .iter()
                    .filter(|n| n.kind == common::NodeKind::Pod)
                    .map(|n| n.workload.rsplit('/').next().unwrap_or("").to_string()),
            )
        }),
        "dns" | "fqdn" => {
            ctx.store.with(|s| out.extend(s.all().iter().filter(|e| !e.dst_dns.is_empty()).map(|e| e.dst_dns.clone())))
        }
        "identity" | "id" => ctx.index.with(|ix| out.extend(ix.identity.values().map(|i| i.to_string()))),
        "ip" => ctx.graph.with(|g| out.extend(g.nodes.iter().filter(|n| !n.ip.is_empty()).map(|n| n.ip.clone()))),
        "port" => ctx.graph.with(|g| {
            for e in &g.edges {
                for p in &e.ports {
                    if let Some((_, port)) = p.split_once('/') {
                        out.insert(port.to_string());
                    }
                }
            }
        }),
        "protocol" | "proto" => out.extend(["TCP", "UDP", "ICMPv4", "ICMPv6", "SCTP"].map(String::from)),
        "verdict" => {
            out.extend(["forwarded", "dropped", "audit", "redirected", "error", "translated"].map(String::from))
        }
        "node" => ctx.store.with(|s| out.extend(s.all().iter().filter(|e| !e.node.is_empty()).map(|e| e.node.clone()))),
        "policy" => ctx.policies.with(|p| {
            out.extend(p.iter().map(|p| match &p.namespace {
                Some(ns) => format!("{ns}/{}", p.name),
                None => p.name.clone(),
            }))
        }),
        _ => {}
    }
    out.remove("");
    out
}

fn suggest(ctx: Ctx, draft: &str) -> Vec<String> {
    let d = draft.trim();
    let (prefix, kind, value) = split_draft(d);
    match value {
        Some(v) => {
            let v = v.to_lowercase();
            let mut c: Vec<String> = candidates(ctx, &kind.to_ascii_lowercase())
                .into_iter()
                .filter(|c| c.to_lowercase().contains(&v))
                .collect();
            // Prefix matches first.
            c.sort_by_key(|x| !x.to_lowercase().starts_with(&v));
            c.into_iter().take(MAX_SUGGESTIONS).map(|c| format!("{prefix}{kind}={c}")).filter(|s| s != d).collect()
        }
        None => {
            let neg = if prefix.starts_with('!') { "!" } else { "" };
            let typed = format!("{}{kind}", prefix.trim_start_matches('!'));
            KINDS.iter().filter(|k| k.starts_with(&typed)).take(MAX_SUGGESTIONS).map(|k| format!("{neg}{k}")).collect()
        }
    }
}

fn chip_class(token: &str) -> String {
    let f = Filter::parse(token);
    let Some(t) = f.terms.first() else {
        return "fchip error".into();
    };
    let kind = match &t.kind {
        Kind::Label { .. } => "label",
        Kind::Pod(_) | Kind::Workload(_) => "pod",
        Kind::Namespace(_) => "ns",
        Kind::Ip(_) | Kind::Dns(_) | Kind::Identity(_) => "net",
        Kind::Text(_) => "text",
        _ => "flow",
    };
    let mut c = format!("fchip k-{kind}");
    if t.negate {
        c.push_str(" neg");
    }
    c
}

fn side_label(token: &str) -> Option<&'static str> {
    Filter::parse(token).terms.first().and_then(|t| match t.side {
        Side::From => Some("from"),
        Side::To => Some("to"),
        Side::Either => None,
    })
}

const EXAMPLES: &[(&str, &str)] = &[
    ("ns=payments verdict=dropped", "drops in or out of payments"),
    ("from:ns=frontend to:ns=shop", "frontend calling shop"),
    ("!ns=kube-system", "everything except kube-system"),
    ("to:dns=*.stripe.com", "traffic to Stripe"),
    ("label=app=postgres port=5432", "Postgres on its port"),
];

#[component]
pub fn FilterBar() -> impl IntoView {
    let ctx = use_ctx();
    let draft = RwSignal::new(String::new());
    let focused = RwSignal::new(false);
    let active = RwSignal::new(None::<usize>);
    let help = RwSignal::new(false);
    let input = NodeRef::<leptos::html::Input>::new();

    let suggestions = Memo::new(move |_| {
        if !focused.get() || draft.with(|d| d.trim().is_empty()) {
            return Vec::new();
        }
        draft.with(|d| suggest(ctx, d))
    });

    let commit = move |text: String| {
        let t = text.trim().to_string();
        if !t.is_empty() {
            ctx.add_filter(&t);
        }
        draft.set(String::new());
        active.set(None);
    };
    // A kind ("label=") goes into the box to be completed; a value commits.
    let apply = move |s: String| {
        if s.ends_with('=') {
            draft.set(s);
            active.set(None);
        } else {
            commit(s);
        }
        if let Some(i) = input.get_untracked() {
            let _ = i.focus();
        }
    };
    let remove = move |token: String| {
        ctx.filter.update(|f| *f = f.split_whitespace().filter(|t| *t != token).collect::<Vec<_>>().join(" "));
    };

    let on_key = move |ev: KeyboardEvent| {
        let n = suggestions.with_untracked(Vec::len);
        match ev.key().as_str() {
            "ArrowDown" if n > 0 => {
                ev.prevent_default();
                active.update(|a| *a = Some(a.map_or(0, |i| (i + 1) % n)));
            }
            "ArrowUp" if n > 0 => {
                ev.prevent_default();
                active.update(|a| *a = Some(a.map_or(n - 1, |i| (i + n - 1) % n)));
            }
            "Tab" if n > 0 && !draft.get_untracked().is_empty() => {
                ev.prevent_default();
                // Suggestions follow live data and can shrink under a
                // highlighted entry; fall back to the first one.
                let i = active.get_untracked().filter(|&i| i < n).unwrap_or(0);
                if let Some(s) = suggestions.with_untracked(|s| s.get(i).cloned()) {
                    apply(s);
                }
            }
            "Enter" => {
                ev.prevent_default();
                match active.get_untracked().and_then(|i| suggestions.with_untracked(|s| s.get(i).cloned())) {
                    Some(s) => apply(s),
                    None => commit(draft.get_untracked()),
                }
            }
            "Backspace" if draft.with_untracked(String::is_empty) => {
                ctx.filter.update(|f| {
                    let mut t: Vec<&str> = f.split_whitespace().collect();
                    t.pop();
                    *f = t.join(" ");
                });
            }
            "Escape" => {
                draft.set(String::new());
                active.set(None);
            }
            _ => {}
        }
    };

    view! {
        <div class="filter-wrap">
            <div
                class="filterbar"
                class:focused=move || focused.get()
                on:click=move |_| {
                    if let Some(i) = input.get() {
                        let _ = i.focus();
                    }
                }
            >
                <span class="ficon">"⌕"</span>
                {move || {
                    ctx.filter
                        .get()
                        .split_whitespace()
                        .map(|tok| {
                            let tok = tok.to_string();
                            let err = Filter::parse(&tok).errors.first().map(|e| e.message.clone());
                            let side = side_label(&tok);
                            let shown = tok
                                .trim_start_matches('!')
                                .trim_start_matches("from:")
                                .trim_start_matches("to:")
                                .to_string();
                            let neg = tok.starts_with('!');
                            let t2 = tok.clone();
                            view! {
                                <span class=chip_class(&tok) title=err.unwrap_or_default()>
                                    {neg.then(|| view! { <b class="fneg">"not"</b> })}
                                    {side.map(|s| view! { <b class="fside">{s}</b> })}
                                    {shown}
                                    <button
                                        class="fx"
                                        on:click=move |ev| {
                                            ev.stop_propagation();
                                            remove(t2.clone());
                                        }
                                    >
                                        "✕"
                                    </button>
                                </span>
                            }
                        })
                        .collect_view()
                }}
                <input
                    node_ref=input
                    class="finput"
                    placeholder=move || {
                        if ctx.filter.with(String::is_empty) {
                            "Filter: label=app=web  from:ns=shop  to:port=5432  !verdict=forwarded"
                        } else {
                            ""
                        }
                    }
                    prop:value=move || draft.get()
                    on:input=move |ev| {
                        draft.set(event_target_value(&ev));
                        active.set(None);
                    }
                    on:keydown=on_key
                    on:focus=move |_| focused.set(true)
                    on:blur=move |_| {
                        focused.set(false);
                        let d = draft.get_untracked();
                        if !d.trim().is_empty() && !d.ends_with('=') {
                            commit(d);
                        }
                    }
                />
                {move || {
                    let list = suggestions.get();
                    (!list.is_empty())
                        .then(|| {
                            view! {
                                <div class="suggest">
                                    {list
                                        .into_iter()
                                        .enumerate()
                                        .map(|(i, s)| {
                                            let s2 = s.clone();
                                            let label = s.clone();
                                            view! {
                                                <div
                                                    class="sitem"
                                                    class:active=move || active.get() == Some(i)
                                                    on:mousedown=move |ev| {
                                                        ev.prevent_default();
                                                        apply(s2.clone());
                                                    }
                                                >
                                                    {label}
                                                </div>
                                            }
                                        })
                                        .collect_view()}
                                </div>
                            }
                        })
                }}
            </div>
            {move || {
                (!ctx.filter.with(String::is_empty))
                    .then(|| {
                        view! {
                            <button class="btn" title="Clear filter" on:click=move |_| ctx.filter.set(String::new())>
                                "Clear"
                            </button>
                        }
                    })
            }}
            <Dropdown
                label="?"
                button_class="btn icon-btn"
                title="Filter syntax"
                menu_class="help-menu"
                width=470.0
                open=help
            >
                <div class="menu-title">"Filter syntax"</div>
                <table class="syntax">
                    <tr>
                        <td>
                            <code>"label=app=web"</code>
                        </td>
                        <td>"label value; "<code>"label=app"</code>" = key exists; " <code>"k8s:"</code>" optional"</td>
                    </tr>
                    <tr>
                        <td>
                            <code>"pod=web"</code>
                            " "
                            <code>"ns=shop"</code>
                        </td>
                        <td>"pod name prefix ("<code>"pod=ns/name"</code>" too), namespace"</td>
                    </tr>
                    <tr>
                        <td>
                            <code>"pod=node:cp-1"</code>
                        </td>
                        <td>"a node by its whole name, "<code>"*"</code>" wildcards"</td>
                    </tr>
                    <tr>
                        <td>
                            <code>"workload=web"</code>
                        </td>
                        <td>"Deployment, StatefulSet, … name"</td>
                    </tr>
                    <tr>
                        <td>
                            <code>"ip=10.0.0.0/8"</code>
                        </td>
                        <td>"address or CIDR"</td>
                    </tr>
                    <tr>
                        <td>
                            <code>"dns=*.stripe.com"</code>
                        </td>
                        <td>"DNS name, "<code>"*"</code>" wildcards"</td>
                    </tr>
                    <tr>
                        <td>
                            <code>"identity=12345"</code>
                        </td>
                        <td>"Cilium security identity"</td>
                    </tr>
                    <tr>
                        <td>
                            <code>"port=5432"</code>
                            " "
                            <code>"protocol=udp"</code>
                        </td>
                        <td>"destination port, protocol"</td>
                    </tr>
                    <tr>
                        <td>
                            <code>"verdict=dropped"</code>
                        </td>
                        <td>"forwarded, dropped, audit, …"</td>
                    </tr>
                    <tr>
                        <td>
                            <code>"node=worker-1"</code>
                            " "
                            <code>"policy=lockdown"</code>
                        </td>
                        <td>"Kubernetes node, allowing/denying policy"</td>
                    </tr>
                    <tr>
                        <td>
                            <code>"from:…"</code>
                            " "
                            <code>"to:…"</code>
                        </td>
                        <td>"only the source / destination side"</td>
                    </tr>
                    <tr>
                        <td>
                            <code>"!…"</code>
                        </td>
                        <td>"exclude"</td>
                    </tr>
                    <tr>
                        <td>
                            <code>"word"</code>
                        </td>
                        <td>"search names, IPs, DNS names, HTTP paths"</td>
                    </tr>
                </table>
                <p class="muted small">
                    "Several terms of the same kind match any of them; different kinds must all match."
                </p>
                <div class="menu-title">"Examples"</div>
                {EXAMPLES
                    .iter()
                    .map(|(f, what)| {
                        view! {
                            <div
                                class="example"
                                on:click=move |_| {
                                    ctx.filter.set(f.to_string());
                                    help.set(false);
                                }
                            >
                                <code>{*f}</code>
                                <span class="muted">{*what}</span>
                            </div>
                        }
                    })
                    .collect_view()}
                <label class="chk">
                    <input
                        type="checkbox"
                        prop:checked=move || ctx.view.with(|v| v.filter_map)
                        on:change=move |ev| {
                            let on = event_target_checked(&ev);
                            ctx.view.update(|v| v.filter_map = on);
                        }
                    />
                    "Also filter the map"
                </label>
            </Dropdown>
        </div>
    }
}
