//! App shell: shared state, header, tabs.

use crate::config_view::ConfigTab;
use crate::endpoints::EndpointsTab;
use crate::flows::FlowsTab;
use crate::map::MapTab;
use crate::policies::PoliciesTab;
use crate::settings::{self, ViewSettings};
use crate::util::{get_json, set_interval};
use crate::ws;
use common::aggregate::Level;
use common::filter::Filter;
use common::{ConfigView, EndpointsView, FlowEvent, Graph, Info, PolicyInfo, Status};
use leptos::prelude::*;
use leptos::task::spawn_local;
use std::collections::{HashMap, VecDeque};

/// Flows kept in the browser for the table.
pub const MAX_FLOWS: usize = 1000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Map,
    Flows,
    Endpoints,
    Policies,
    Config,
}

/// Per-endpoint data the filters need, keyed by map id (`namespace/pod`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EpIndex {
    pub labels: HashMap<String, Vec<String>>,
    pub identity: HashMap<String, u32>,
}

impl EpIndex {
    /// Labels for an id; reserved entities get their `reserved:` label.
    pub fn labels_for(&self, id: &str) -> Vec<String> {
        if let Some(l) = self.labels.get(id) {
            return l.clone();
        }
        id.strip_prefix("reserved:").map(|e| vec![format!("reserved:{e}")]).unwrap_or_default()
    }
}

/// All shared state. Every field is a signal, so the struct is `Copy` and
/// handed to components through context.
#[derive(Clone, Copy)]
pub struct Ctx {
    pub graph: RwSignal<Graph>,
    pub flows: RwSignal<VecDeque<FlowEvent>>,
    pub status: RwSignal<Status>,
    pub connected: RwSignal<bool>,
    pub policies: RwSignal<Vec<PolicyInfo>>,
    pub config: RwSignal<Option<ConfigView>>,
    pub info: RwSignal<Info>,
    pub tab: RwSignal<Tab>,
    pub level: RwSignal<Level>,
    /// Selected node id on the map (at the current level).
    pub selected: RwSignal<Option<String>>,
    /// Key of the policy whose selected pods are highlighted on the map.
    pub highlight: RwSignal<Option<String>>,
    /// Hubble-style filter (see `common::filter`), shared by the map, the
    /// traffic panel, the Flows and Endpoints tabs.
    pub filter: RwSignal<String>,
    pub parsed: Memo<Filter>,
    pub endpoints: RwSignal<EndpointsView>,
    pub index: Memo<EpIndex>,
    pub paused: RwSignal<bool>,
    /// Map look and panel preferences, saved in the browser.
    pub view: RwSignal<ViewSettings>,
}

pub fn use_ctx() -> Ctx {
    expect_context::<Ctx>()
}

impl Ctx {
    /// Jump to a pod (or reserved entity) on the map.
    pub fn show_on_map(self, id: &str) {
        self.level.set(Level::Pod);
        self.selected.set(Some(id.to_string()));
        self.tab.set(Tab::Map);
    }

    /// Adds a term to the filter unless it is already there.
    pub fn add_filter(self, term: &str) {
        self.filter.update(|f| {
            if !f.split_whitespace().any(|t| t == term) {
                if !f.trim().is_empty() {
                    f.push(' ');
                }
                f.push_str(term);
            }
        });
    }
}

pub fn load_endpoints(ctx: Ctx) {
    spawn_local(async move {
        match get_json::<EndpointsView>("/api/endpoints").await {
            Ok(v) => ctx.endpoints.set(v),
            Err(e) => leptos::logging::warn!("{e}"),
        }
    });
}

pub fn load_policies(ctx: Ctx) {
    spawn_local(async move {
        match get_json::<Vec<PolicyInfo>>("/api/policies").await {
            Ok(p) => ctx.policies.set(p),
            Err(e) => leptos::logging::warn!("{e}"),
        }
    });
}

pub fn load_config(ctx: Ctx) {
    spawn_local(async move {
        match get_json::<ConfigView>("/api/config").await {
            Ok(c) => ctx.config.set(Some(c)),
            Err(e) => leptos::logging::warn!("{e}"),
        }
    });
}

#[component]
pub fn App() -> impl IntoView {
    let filter = RwSignal::new(String::new());
    let parsed = Memo::new(move |_| filter.with(|f| Filter::parse(f)));
    let endpoints = RwSignal::new(EndpointsView::default());
    let index = Memo::new(move |_| {
        endpoints.with(|v| {
            let mut ix = EpIndex::default();
            for e in &v.endpoints {
                ix.labels.insert(e.id.clone(), e.labels.clone());
                ix.identity.insert(e.id.clone(), e.identity as u32);
            }
            for (id, l) in &v.pod_labels {
                ix.labels.insert(id.clone(), l.clone());
            }
            ix
        })
    });
    let ctx = Ctx {
        graph: RwSignal::new(Graph::default()),
        flows: RwSignal::new(VecDeque::with_capacity(MAX_FLOWS)),
        status: RwSignal::new(Status::default()),
        connected: RwSignal::new(false),
        policies: RwSignal::new(Vec::new()),
        config: RwSignal::new(None),
        info: RwSignal::new(Info::default()),
        tab: RwSignal::new(Tab::Map),
        level: RwSignal::new(Level::Workload),
        selected: RwSignal::new(None),
        highlight: RwSignal::new(None),
        filter,
        parsed,
        endpoints,
        index,
        paused: RwSignal::new(false),
        view: RwSignal::new(settings::load()),
    };
    provide_context(ctx);
    Effect::new(move |_| ctx.view.with(settings::save));

    ws::connect(ctx);
    load_policies(ctx);
    set_interval(move || load_policies(ctx), 15_000);
    load_endpoints(ctx);
    set_interval(move || load_endpoints(ctx), 10_000);
    spawn_local(async move {
        if let Ok(i) = get_json::<Info>("/api/info").await {
            ctx.info.set(i);
        }
    });

    let tab = move |t: Tab, label: &'static str| {
        view! {
            <button class="tab" class:active=move || ctx.tab.get() == t on:click=move |_| ctx.tab.set(t)>
                {label}
                {move || {
                    match t {
                        Tab::Policies => Some(ctx.policies.with(Vec::len)),
                        Tab::Endpoints => Some(ctx.endpoints.with(|e| e.endpoints.len())),
                        _ => None,
                    }
                        .map(|n| view! { <span class="count">{n}</span> })
                }}
            </button>
        }
    };

    view! {
        <div class="app">
            <header class="top">
                <div class="brand">
                    <img class="logo" src="/assets/logo-mark.svg" alt="" />
                    <span class="name">"Wire"<span class="name-accent">"Pulse"</span></span>
                    {move || {
                        let c = ctx.info.with(|i| i.cluster_name.clone());
                        (!c.is_empty()).then(|| view! { <span class="cluster">{c}</span> })
                    }}
                </div>
                <nav class="tabs">
                    {tab(Tab::Map, "Map")} {tab(Tab::Flows, "Flows")} {tab(Tab::Endpoints, "Endpoints")}
                    {tab(Tab::Policies, "Policies")} {tab(Tab::Config, "Config")}
                </nav>
                <StatusBar />
            </header>
            <main>
                // The map stays mounted so its zoom and position survive tab switches.
                <div class="tab-body map-body" class:hidden=move || ctx.tab.get() != Tab::Map>
                    <MapTab />
                </div>
                {move || match ctx.tab.get() {
                    Tab::Map => ().into_any(),
                    Tab::Flows => view! { <FlowsTab /> }.into_any(),
                    Tab::Endpoints => view! { <EndpointsTab /> }.into_any(),
                    Tab::Policies => view! { <PoliciesTab /> }.into_any(),
                    Tab::Config => view! { <ConfigTab /> }.into_any(),
                }}
            </main>
        </div>
    }
}

#[component]
fn StatusBar() -> impl IntoView {
    let ctx = use_ctx();
    let dot = move |label: &'static str, ok: Signal<bool>| {
        view! {
            <span class="status-item" class:ok=move || ok.get()>
                <span class="dot"></span>
                {label}
            </span>
        }
    };
    let ws_ok = Signal::derive(move || ctx.connected.get());
    let hubble_ok = Signal::derive(move || ctx.connected.get() && ctx.status.with(|s| s.hubble_connected));
    let k8s_ok = Signal::derive(move || ctx.connected.get() && ctx.status.with(|s| s.k8s_connected));
    view! {
        <div class="status" title=move || ctx.status.with(|s| s.message.clone())>
            {move || ctx.status.with(|s| s.demo).then(|| view! { <span class="badge demo">"demo"</span> })}
            {dot("server", ws_ok)}
            {dot("hubble", hubble_ok)}
            {dot("k8s", k8s_ok)}
        </div>
    }
}
