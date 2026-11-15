//! Deployed policies: list, details, YAML, and "show on map".

use crate::app::{load_policies, use_ctx, Tab};
use crate::util::{namespace_of, short_name};
use common::{PolicyInfo, PolicyKind};
use leptos::prelude::*;

fn rules_summary(p: &PolicyInfo) -> String {
    let mut parts = Vec::new();
    let mut add = |n: u32, what: &str| {
        if n > 0 {
            parts.push(format!("{n} {what}"));
        }
    };
    add(p.ingress_rules, "ingress");
    add(p.egress_rules, "egress");
    add(p.ingress_deny_rules, "ingress deny");
    add(p.egress_deny_rules, "egress deny");
    if parts.is_empty() {
        // An empty rule list with a selector means "deny everything" for
        // NetworkPolicy and enables default-deny for Cilium policies.
        "no allow rules (default deny)".into()
    } else {
        parts.join(" · ")
    }
}

#[component]
pub fn PoliciesTab() -> impl IntoView {
    let ctx = use_ctx();
    let q = RwSignal::new(String::new());
    let kind = RwSignal::new(None::<PolicyKind>);
    let open = RwSignal::new(None::<String>);

    let list = Memo::new(move |_| {
        let q = q.get().trim().to_lowercase();
        let k = kind.get();
        ctx.policies.with(|ps| {
            ps.iter()
                .filter(|p| k.is_none_or(|k| p.kind == k))
                .filter(|p| {
                    q.is_empty()
                        || p.name.to_lowercase().contains(&q)
                        || p.namespace.as_deref().unwrap_or("").to_lowercase().contains(&q)
                        || p.selector.to_lowercase().contains(&q)
                })
                .cloned()
                .collect::<Vec<_>>()
        })
    });

    let kind_btn = move |k: Option<PolicyKind>, label: &'static str| {
        view! {
            <button class:active=move || kind.get() == k on:click=move |_| kind.set(k)>
                {label}
            </button>
        }
    };

    view! {
        <div class="tab-body policies-body">
            <div class="toolbar">
                <input
                    type="search"
                    class="search"
                    placeholder="Search name, namespace or selector"
                    prop:value=move || q.get()
                    on:input=move |ev| q.set(event_target_value(&ev))
                />
                <div class="seg">
                    {kind_btn(None, "All")} {kind_btn(Some(PolicyKind::CiliumNetworkPolicy), "CNP")}
                    {kind_btn(Some(PolicyKind::CiliumClusterwideNetworkPolicy), "CCNP")}
                    {kind_btn(Some(PolicyKind::NetworkPolicy), "NetworkPolicy")}
                </div>
                <button class="btn" on:click=move |_| load_policies(ctx)>
                    "Refresh"
                </button>
                <span class="muted">{move || format!("{} policies", list.with(Vec::len))}</span>
            </div>
            <div class="split" class:has-open=move || open.get().is_some()>
                <div class="list">
                    {move || {
                        let items = list.get();
                        if items.is_empty() {
                            return view! { <p class="empty">"No policies found."</p> }.into_any();
                        }
                        items
                            .into_iter()
                            .map(|p| {
                                let key = p.key();
                                let key_active = key.clone();
                                view! {
                                    <div
                                        class="card"
                                        class:active=move || open.get().as_deref() == Some(key_active.as_str())
                                        on:click=move |_| open.set(Some(key.clone()))
                                    >
                                        <div class="card-head">
                                            <span class=format!(
                                                "kind k-{}",
                                                p.kind.short().to_lowercase(),
                                            )>{p.kind.short()}</span>
                                            <span class="pname">{p.name.clone()}</span>
                                            {p.namespace.clone().map(|n| view! { <span class="muted">{n}</span> })}
                                            {(p.valid == Some(false))
                                                .then(|| view! { <span class="badge bad">"invalid"</span> })}
                                            {p.host_policy.then(|| view! { <span class="badge">"host"</span> })}
                                        </div>
                                        <code class="sel">{p.selector.clone()}</code>
                                        <div class="card-foot">
                                            <span>{rules_summary(&p)}</span>
                                            <span class="muted">{format!("selects {} pods", p.selected.len())}</span>
                                        </div>
                                    </div>
                                }
                            })
                            .collect_view()
                            .into_any()
                    }}
                </div>
                <div class="detail">
                    {move || {
                        let key = open.get()?;
                        let p = ctx.policies.with(|ps| ps.iter().find(|p| p.key() == key).cloned())?;
                        Some(view! { <PolicyDetail p=p on_close=move || open.set(None) /> })
                    }}
                </div>
            </div>
        </div>
    }
}

#[component]
fn PolicyDetail(p: PolicyInfo, on_close: impl Fn() + Send + Sync + 'static) -> impl IntoView {
    let ctx = use_ctx();
    let key = p.key();
    let selected = p.selected.clone();
    view! {
        <div class="detail-head">
            <button class="btn back" on:click=move |_| on_close()>
                "← Back"
            </button>
            <div>
                <div class="title">{p.name.clone()}</div>
                <div class="sub">
                    {format!("{:?}", p.kind)} {p.namespace.clone().map(|n| format!(" · {n}"))}
                    {(!p.created.is_empty()).then(|| format!(" · created {}", p.created))}
                </div>
            </div>
            <button
                class="btn primary"
                on:click=move |_| {
                    ctx.highlight.set(Some(key.clone()));
                    ctx.selected.set(None);
                    ctx.tab.set(Tab::Map);
                }
            >
                "Show on map"
            </button>
        </div>
        <div class="kv">
            <span>"selector"</span>
            <b class="mono">{p.selector.clone()}</b>
            <span>"rules"</span>
            <b>{rules_summary(&p)}</b>
            <span>"status"</span>
            <b>
                {match p.valid {
                    Some(true) => "valid",
                    Some(false) => "invalid",
                    None => "not reported",
                }}
            </b>
        </div>
        <h4>{format!("Selected pods ({})", selected.len())}</h4>
        {if selected.is_empty() {
            view! {
                <p class="muted">
                    {if p.host_policy { "Applies to nodes, not pods." } else { "No running pods match right now." }}
                </p>
            }
                .into_any()
        } else {
            view! {
                <ul class="pods">
                    {selected
                        .into_iter()
                        .map(|id| {
                            let ns = namespace_of(&id).to_string();
                            let name = short_name(&id).to_string();
                            view! { <li on:click=move |_| ctx.show_on_map(&id)>{name} <small>{ns}</small></li> }
                        })
                        .collect_view()}
                </ul>
            }
                .into_any()
        }}
        <h4>"YAML"</h4>
        <pre class="yaml">{p.yaml}</pre>
    }
}
