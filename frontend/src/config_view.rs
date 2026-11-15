//! Cilium config: key facts, searchable table, per-node overrides.

use crate::app::{load_config, use_ctx};
use common::ConfigView;
use leptos::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

/// Settings people look for first, with a friendly label.
const KEY_FACTS: &[(&str, &[&str])] = &[
    ("Cluster", &["cluster-name", "cluster-id"]),
    ("Routing", &["routing-mode", "tunnel-protocol", "tunnel"]),
    ("kube-proxy replacement", &["kube-proxy-replacement"]),
    ("IPAM", &["ipam"]),
    ("Encryption", &["enable-wireguard", "enable-ipsec", "encryption-mode"]),
    ("Policy enforcement", &["enable-policy", "policy-audit-mode"]),
    ("Hubble", &["enable-hubble", "hubble-metrics"]),
    ("Load balancing", &["bpf-lb-algorithm", "bpf-lb-mode"]),
];

fn fact(view: &ConfigView, keys: &[&str]) -> Option<String> {
    let vals: Vec<String> = keys
        .iter()
        .filter_map(|k| view.entries.iter().find(|e| e.key == *k))
        .map(|e| format!("{} = {}", e.key, e.value))
        .collect();
    (!vals.is_empty()).then(|| vals.join("\n"))
}

#[component]
pub fn ConfigTab() -> impl IntoView {
    let ctx = use_ctx();
    load_config(ctx);
    let q = RwSignal::new(String::new());
    let category = RwSignal::new(String::new());

    view! {
        <div class="tab-body config-body">
            {move || {
                let Some(cfg) = ctx.config.get() else {
                    return view! { <p class="empty">"Loading config…"</p> }.into_any();
                };
                if cfg.entries.is_empty() {
                    return view! {
                        <p class="empty">
                            "No Cilium config found. Check --cilium-namespace / --cilium-configmap and RBAC."
                        </p>
                    }
                        .into_any();
                }
                let categories: BTreeSet<String> = cfg.entries.iter().map(|e| e.category.clone()).collect();
                let mut overridden: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
                for o in &cfg.overrides {
                    for e in &o.entries {
                        overridden.entry(e.key.clone()).or_default().push((o.name.clone(), e.value.clone()));
                    }
                }
                let facts: Vec<(&str, String)> = KEY_FACTS
                    .iter()
                    .filter_map(|(label, keys)| fact(&cfg, keys).map(|v| (*label, v)))
                    .collect();
                let entries = cfg.entries.clone();
                let overrides = cfg.overrides.clone();
                let overridden2 = overridden.clone();
                // key -> [(override name, value)]

                view! {
                    <div class="toolbar">
                        <input
                            type="search"
                            class="search"
                            placeholder="Search keys and values"
                            prop:value=move || q.get()
                            on:input=move |ev| q.set(event_target_value(&ev))
                        />
                        <select
                            on:change=move |ev| category.set(event_target_value(&ev))
                            prop:value=move || category.get()
                        >
                            <option value="">"All categories"</option>
                            {categories
                                .into_iter()
                                .map(|c| view! { <option value=c.clone()>{c.clone()}</option> })
                                .collect_view()}
                        </select>
                        <button class="btn" on:click=move |_| load_config(ctx)>
                            "Refresh"
                        </button>
                        <span class="muted">{cfg.source.clone()}</span>
                    </div>
                    <div class="facts">
                        {facts
                            .into_iter()
                            .map(|(label, v)| {
                                view! {
                                    <div class="fact">
                                        <span>{label}</span>
                                        <pre>{v}</pre>
                                    </div>
                                }
                            })
                            .collect_view()}
                    </div>
                    <div class="table-wrap">
                        <table class="cfg-table">
                            <thead>
                                <tr>
                                    <th>"Key"</th>
                                    <th>"Value"</th>
                                    <th>"Category"</th>
                                    <th>"Node overrides"</th>
                                </tr>
                            </thead>
                            <tbody>
                                {move || {
                                    let q = q.get().trim().to_lowercase();
                                    let cat = category.get();
                                    entries
                                        .iter()
                                        .filter(|e| cat.is_empty() || e.category == cat)
                                        .filter(|e| {
                                            q.is_empty() || e.key.to_lowercase().contains(&q)
                                                || e.value.to_lowercase().contains(&q)
                                        })
                                        .map(|e| {
                                            let ov = overridden2.get(&e.key).cloned().unwrap_or_default();
                                            let v = e.value.clone();
                                            view! {
                                                <tr>
                                                    <td class="mono key">{e.key.clone()}</td>
                                                    <td
                                                        class="mono val"
                                                        class:on={v == "true"}
                                                        class:off={v == "false"}
                                                    >
                                                        {v.clone()}
                                                    </td>
                                                    <td>
                                                        <span class="tag">{e.category.clone()}</span>
                                                    </td>
                                                    <td>
                                                        {ov
                                                            .into_iter()
                                                            .map(|(n, v)| {
                                                                view! { <span class="badge">{format!("{n}: {v}")}</span> }
                                                            })
                                                            .collect_view()}
                                                    </td>
                                                </tr>
                                            }
                                        })
                                        .collect_view()
                                }}
                            </tbody>
                        </table>
                    </div>
                    {(!overrides.is_empty())
                        .then(|| {
                            view! {
                                <h3>"CiliumNodeConfig overrides"</h3>
                                <div class="overrides">
                                    {overrides
                                        .into_iter()
                                        .map(|o| {
                                            view! {
                                                <div class="card">
                                                    <div class="card-head">
                                                        <span class="pname">{o.name}</span>
                                                        <span class="muted">{o.namespace}</span>
                                                    </div>
                                                    <code class="sel">{format!("nodes: {}", o.node_selector)}</code>
                                                    <table class="cfg-table small">
                                                        {o
                                                            .entries
                                                            .into_iter()
                                                            .map(|e| {
                                                                view! {
                                                                    <tr>
                                                                        <td class="mono key">{e.key}</td>
                                                                        <td class="mono val">{e.value}</td>
                                                                    </tr>
                                                                }
                                                            })
                                                            .collect_view()}
                                                    </table>
                                                </div>
                                            }
                                        })
                                        .collect_view()}
                                </div>
                            }
                        })}
                }
                    .into_any()
            }}
        </div>
    }
}
