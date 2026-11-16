//! Cilium endpoints: `CiliumEndpoint` objects, or `CiliumEndpointSlice`
//! entries when Cilium batches endpoints into slices.
//!
//! Both are watched: a kube-rs reflector lists them once, then applies the
//! changes the API server sends, so a big cluster's endpoints aren't all sent
//! again every few seconds. The view is rebuilt from that cache.

use crate::state::{AppState, Inventory};
use common::{EndpointInfo, EndpointsView};
use futures::{FutureExt, StreamExt};
use kube::api::{ApiResource, DynamicObject, GroupVersionKind, ListParams, TypeMeta};
use kube::runtime::reflector::{self, Store};
use kube::runtime::{watcher, WatchStreamExt};
use kube::{Api, Client};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

/// How often the view is rebuilt from the watched objects. It also picks up
/// workload names and labels from the pod inventory.
const REFRESH: Duration = Duration::from_secs(10);
/// How long a new watch's first list may take before the loop goes on.
const FIRST_LIST_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn refresh_loop(client: Client, state: Arc<AppState>) {
    // Workload names and fallback labels come from the pod inventory.
    for _ in 0..20 {
        if state.status.read().unwrap().k8s_connected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let mut endpoints: Option<Watched> = None;
    // Only watched while there are no CiliumEndpoints.
    let mut slices: Option<Watched> = None;
    let mut tick = tokio::time::interval(REFRESH);
    loop {
        tick.tick().await;
        if endpoints.is_none() {
            endpoints = Watched::start(&client, "CiliumEndpoint", &["v2"]).await;
        }
        let watching = endpoints.is_some();
        match objects(&mut endpoints) {
            // CiliumEndpoints have the most detail: used whenever there are any.
            Some(objects) if !objects.is_empty() => {
                slices = None;
                let parsed =
                    objects.into_iter().filter_map(|o| Some((parse_cep(&o)?, EndpointObject::Endpoint(o)))).collect();
                store(&state, "CiliumEndpoint", parsed);
                continue;
            }
            // Not listed yet: wait for them rather than show the slices meanwhile.
            None if watching => continue,
            _ => {}
        }
        if slices.is_none() {
            slices = Watched::start(&client, "CiliumEndpointSlice", &["v2", "v2alpha1"]).await;
        }
        let watching = slices.is_some();
        match objects(&mut slices) {
            Some(objects) => {
                let parsed = objects
                    .into_iter()
                    .flat_map(|o| {
                        parse_ces(&o).into_iter().map(move |(e, i)| (e, EndpointObject::SliceEntry(o.clone(), i)))
                    })
                    .collect();
                store(&state, "CiliumEndpointSlice", parsed);
            }
            None if watching => {}
            None => store(&state, "", Vec::new()),
        }
    }
}

/// One kind of object, kept in memory and up to date by a kube-rs reflector.
struct Watched {
    store: Store<DynamicObject>,
    /// Set when the watch stopped on an error (e.g. RBAC doesn't allow
    /// `watch`). The next round starts a new one, which lists everything again.
    failed: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Watched {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Watched {
    /// Watches `kind` (group `cilium.io`) in the first of `versions` the API
    /// server has. `None` when it has none of them, or they can't be listed.
    async fn start(client: &Client, kind: &'static str, versions: &[&str]) -> Option<Self> {
        for version in versions {
            let ar = ApiResource::from_gvk(&GroupVersionKind::gvk("cilium.io", version, kind));
            let api: Api<DynamicObject> = Api::all_with(client.clone(), &ar);
            // Is it there, and may we read it? One object answers both.
            match api.list(&ListParams::default().limit(1)).await {
                Ok(_) => {}
                Err(kube::Error::Api(s)) if s.is_not_found() => continue,
                Err(e) => {
                    warn!(error = %e, kind, "listing failed");
                    return None;
                }
            }
            // Listed objects come without apiVersion and kind, which the YAML view shows.
            let types = TypeMeta { api_version: ar.api_version.clone(), kind: ar.kind.clone() };
            let writer = reflector::store::Writer::new(ar);
            let store = writer.as_reader();
            // The first list may come from the API server's cache: the watch
            // brings it up to date.
            let events = watcher(api, watcher::Config::default().any_semantic())
                .modify(move |o: &mut DynamicObject| {
                    o.types = Some(types.clone());
                    o.metadata.managed_fields = None;
                })
                .reflect(writer);
            let failed = Arc::new(AtomicBool::new(false));
            let task = tokio::spawn({
                let failed = failed.clone();
                async move {
                    let mut events = std::pin::pin!(events);
                    while let Some(event) = events.next().await {
                        if let Err(e) = event {
                            warn!(error = %e, kind, "watch failed; listing again");
                            failed.store(true, Ordering::Relaxed);
                            return;
                        }
                    }
                }
            });
            let watched = Self { store, failed, task };
            // So the caller can tell "there are none" from "not listed yet".
            let _ = tokio::time::timeout(FIRST_LIST_TIMEOUT, watched.store.wait_until_ready()).await;
            return Some(watched);
        }
        None
    }
}

/// The objects in `slot`'s watch, once its first list has arrived. A watch
/// that failed gets this last look, then is dropped so the next round starts
/// a new one.
fn objects(slot: &mut Option<Watched>) -> Option<Vec<Arc<DynamicObject>>> {
    let w = slot.as_ref()?;
    let listed = w.store.wait_until_ready().now_or_never().is_some_and(|r| r.is_ok());
    let objects = listed.then(|| w.store.state());
    if w.failed.load(Ordering::Relaxed) {
        *slot = None;
    }
    objects
}

/// The object behind an endpoint, for its YAML: the watch's cached object
/// itself, not a copy.
#[derive(Clone)]
pub enum EndpointObject {
    /// A `CiliumEndpoint`.
    Endpoint(Arc<DynamicObject>),
    /// The entry at this index of a `CiliumEndpointSlice`'s `endpoints`.
    SliceEntry(Arc<DynamicObject>, usize),
}

impl EndpointObject {
    pub fn to_value(&self) -> Option<Value> {
        match self {
            Self::Endpoint(o) => serde_json::to_value(&**o).ok(),
            Self::SliceEntry(o, i) => o.data.get("endpoints")?.get(*i).cloned(),
        }
    }
}

/// Saves the endpoints, filling in workloads and fallback labels from the
/// pod inventory.
pub fn store(state: &AppState, source: &str, parsed: Vec<(EndpointInfo, EndpointObject)>) {
    let mut objects = HashMap::with_capacity(parsed.len());
    let mut endpoints = Vec::with_capacity(parsed.len());
    for (e, o) in parsed {
        objects.insert(e.id.clone(), o);
        endpoints.push(e);
    }
    let view = build_view(source, endpoints, &state.inventory.read().unwrap());
    *state.endpoints.write().unwrap() = view;
    *state.endpoint_objects.write().unwrap() = objects;
}

pub fn build_view(source: &str, mut endpoints: Vec<EndpointInfo>, inv: &Inventory) -> EndpointsView {
    let workloads: HashMap<&str, &str> = inv.nodes.iter().map(|n| (n.id.as_str(), n.workload.as_str())).collect();
    for e in &mut endpoints {
        if let Some(w) = workloads.get(e.id.as_str()) {
            e.workload = w.to_string();
        }
        // Slices carry no labels; use the pod's.
        if e.labels.is_empty() {
            if let Some(l) = inv.labels.get(&e.id) {
                e.labels = k8s_labels(l);
            }
        }
    }
    endpoints.sort_by(|a, b| a.id.cmp(&b.id));
    let have: std::collections::HashSet<&str> = endpoints.iter().map(|e| e.id.as_str()).collect();
    let pod_labels = inv
        .labels
        .iter()
        .filter(|(id, _)| !have.contains(id.as_str()))
        .map(|(id, l)| (id.clone(), k8s_labels(l)))
        .collect();
    EndpointsView { source: source.to_string(), endpoints, pod_labels }
}

fn k8s_labels(l: &BTreeMap<String, String>) -> Vec<String> {
    l.iter().map(|(k, v)| format!("k8s:{k}={v}")).collect()
}

fn s(v: &Value, ptr: &str) -> String {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn addressing(net: Option<&Value>) -> (String, String) {
    let mut v4 = String::new();
    let mut v6 = String::new();
    for a in net.and_then(|n| n.get("addressing")).and_then(Value::as_array).into_iter().flatten() {
        if v4.is_empty() {
            v4 = a.get("ipv4").and_then(Value::as_str).unwrap_or_default().to_string();
        }
        if v6.is_empty() {
            v6 = a.get("ipv6").and_then(Value::as_str).unwrap_or_default().to_string();
        }
    }
    (v4, v6)
}

fn named_ports(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|p| {
            format!(
                "{}/{}/{}",
                p.get("name").and_then(Value::as_str).unwrap_or("-"),
                p.get("port").and_then(Value::as_u64).unwrap_or(0),
                p.get("protocol").and_then(Value::as_str).unwrap_or("TCP")
            )
        })
        .collect()
}

/// Parses a `CiliumEndpoint`.
pub fn parse_cep(o: &DynamicObject) -> Option<EndpointInfo> {
    let name = o.metadata.name.clone()?;
    let namespace = o.metadata.namespace.clone().unwrap_or_default();
    let st = o.data.get("status");
    let get = |p: &str| st.and_then(|s| s.pointer(p));
    let (ipv4, ipv6) = addressing(get("/networking"));
    Some(EndpointInfo {
        id: format!("{namespace}/{name}"),
        namespace,
        name,
        endpoint_id: get("/id").and_then(Value::as_u64).unwrap_or(0),
        identity: get("/identity/id").and_then(Value::as_u64).unwrap_or(0),
        labels: get("/identity/labels")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|l| l.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        ipv4,
        ipv6,
        node: get("/networking/node").and_then(Value::as_str).unwrap_or_default().into(),
        state: get("/state").and_then(Value::as_str).unwrap_or_default().into(),
        ingress_enforcing: get("/policy/ingress/enforcing").and_then(Value::as_bool),
        egress_enforcing: get("/policy/egress/enforcing").and_then(Value::as_bool),
        service_account: get("/service-account").and_then(Value::as_str).unwrap_or_default().into(),
        named_ports: named_ports(get("/named-ports")),
        workload: String::new(),
    })
}

/// Parses the entries of a `CiliumEndpointSlice` (fewer details than a
/// `CiliumEndpoint`: no endpoint id, state or labels), each with its index.
pub fn parse_ces(o: &DynamicObject) -> Vec<(EndpointInfo, usize)> {
    let namespace = s(&o.data, "/namespace");
    o.data
        .get("endpoints")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(i, e)| {
            let name = e.get("name")?.as_str()?.to_string();
            let (ipv4, ipv6) = addressing(e.get("networking"));
            let info = EndpointInfo {
                id: format!("{namespace}/{name}"),
                namespace: namespace.clone(),
                name,
                identity: e.get("id").and_then(Value::as_u64).unwrap_or(0),
                ipv4,
                ipv6,
                node: e.pointer("/networking/node").and_then(Value::as_str).unwrap_or_default().into(),
                named_ports: named_ports(e.get("named-ports")),
                ..Default::default()
            };
            Some((info, i))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(v: Value) -> DynamicObject {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn parses_cilium_endpoint() {
        let o = object(json!({
            "metadata": {"name": "web-1", "namespace": "shop"},
            "status": {
                "id": 1234,
                "identity": {"id": 56789, "labels": ["k8s:app=web", "k8s:io.kubernetes.pod.namespace=shop"]},
                "networking": {"addressing": [{"ipv4": "10.0.1.5"}, {"ipv6": "fd00::5"}], "node": "192.168.0.11"},
                "policy": {"ingress": {"enforcing": true}, "egress": {"enforcing": false}},
                "state": "ready",
                "service-account": "web",
                "named-ports": [{"name": "http", "port": 8080, "protocol": "TCP"}]
            }
        }));
        let e = parse_cep(&o).unwrap();
        assert_eq!(e.id, "shop/web-1");
        assert_eq!((e.endpoint_id, e.identity), (1234, 56789));
        assert_eq!((e.ipv4.as_str(), e.ipv6.as_str()), ("10.0.1.5", "fd00::5"));
        assert_eq!(e.node, "192.168.0.11");
        assert_eq!((e.ingress_enforcing, e.egress_enforcing), (Some(true), Some(false)));
        assert_eq!(e.state, "ready");
        assert_eq!(e.named_ports, vec!["http/8080/TCP"]);
    }

    #[test]
    fn parses_slice_and_fills_from_inventory() {
        let o = object(json!({
            "metadata": {"name": "ces-abc"},
            "namespace": "shop",
            "endpoints": [
                {"name": "web-1", "id": 777, "networking": {"addressing": [{"ipv4": "10.0.1.5"}], "node": "n1"}}
            ]
        }));
        let parsed = parse_ces(&o);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].1, 0, "index of the entry");
        let mut inv = Inventory::default();
        inv.nodes.push(common::Node {
            id: "shop/web-1".into(),
            kind: common::NodeKind::Pod,
            namespace: "shop".into(),
            name: "web-1".into(),
            workload: "Deployment/web".into(),
            node_name: String::new(),
            ip: String::new(),
            phase: String::new(),
        });
        inv.labels.insert("shop/web-1".into(), [("app".to_string(), "web".to_string())].into());
        inv.labels.insert("shop/hostnet".into(), [("app".to_string(), "agent".to_string())].into());
        let view = build_view("CiliumEndpointSlice", parsed.into_iter().map(|(e, _)| e).collect(), &inv);
        let e = &view.endpoints[0];
        assert_eq!(e.identity, 777);
        assert_eq!(e.workload, "Deployment/web");
        assert_eq!(e.labels, vec!["k8s:app=web"]);
        assert_eq!(view.pod_labels["shop/hostnet"], vec!["k8s:app=agent"]);
        assert!(!view.pod_labels.contains_key("shop/web-1"));
    }

    #[test]
    fn yaml_is_the_endpoint_or_its_slice_entry() {
        let cep = object(json!({
            "apiVersion": "cilium.io/v2", "kind": "CiliumEndpoint",
            "metadata": {"name": "web-1", "namespace": "shop"},
            "status": {"id": 1234}
        }));
        let v = EndpointObject::Endpoint(Arc::new(cep)).to_value().unwrap();
        assert_eq!(v["kind"], "CiliumEndpoint");
        assert_eq!(v["metadata"]["name"], "web-1");
        assert_eq!(v["status"]["id"], 1234);

        let slice = object(json!({
            "apiVersion": "cilium.io/v2", "kind": "CiliumEndpointSlice",
            "metadata": {"name": "ces-abc"},
            "namespace": "shop",
            "endpoints": [{"name": "web-1", "id": 777}, {"name": "web-2", "id": 778}]
        }));
        let entry = EndpointObject::SliceEntry(Arc::new(slice), 1).to_value().unwrap();
        assert_eq!(entry, json!({"name": "web-2", "id": 778}));
    }
}
