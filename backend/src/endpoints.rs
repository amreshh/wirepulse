//! Cilium endpoints: `CiliumEndpoint` objects, or `CiliumEndpointSlice`
//! entries when Cilium batches endpoints into slices.

use crate::state::{AppState, Inventory};
use common::{EndpointInfo, EndpointsView};
use kube::api::{ApiResource, DynamicObject, GroupVersionKind, ListParams};
use kube::{Api, Client};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

pub async fn refresh_loop(client: Client, state: Arc<AppState>) {
    // Workload names and fallback labels come from the pod inventory.
    for _ in 0..20 {
        if state.status.read().unwrap().k8s_connected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    loop {
        tick.tick().await;
        let (source, raw) = load(&client).await;
        let parsed: Vec<(EndpointInfo, Value)> = raw
            .into_iter()
            .flat_map(|(kind, v)| match kind {
                Source::Endpoint => parse_cep(&v).map(|e| (e, v)).into_iter().collect::<Vec<_>>(),
                Source::Slice => parse_ces(&v),
            })
            .collect();
        store(&state, source, parsed);
    }
}

#[derive(Clone, Copy)]
enum Source {
    Endpoint,
    Slice,
}

async fn list(client: &Client, version: &str, kind: &str) -> Result<Vec<Value>, kube::Error> {
    let ar = ApiResource::from_gvk(&GroupVersionKind::gvk("cilium.io", version, kind));
    let api: Api<DynamicObject> = Api::all_with(client.clone(), &ar);
    let list = api.list(&ListParams::default()).await?;
    Ok(list
        .items
        .iter()
        .filter_map(|o| serde_json::to_value(o).ok())
        .collect())
}

async fn load(client: &Client) -> (&'static str, Vec<(Source, Value)>) {
    match list(client, "v2", "CiliumEndpoint").await {
        Ok(items) if !items.is_empty() => {
            return ("CiliumEndpoint", items.into_iter().map(|v| (Source::Endpoint, v)).collect())
        }
        Ok(_) => {}
        Err(kube::Error::Api(s)) if s.is_not_found() => {}
        Err(e) => warn!(error = %e, "listing CiliumEndpoints failed"),
    }
    for version in ["v2", "v2alpha1"] {
        match list(client, version, "CiliumEndpointSlice").await {
            Ok(items) => return ("CiliumEndpointSlice", items.into_iter().map(|v| (Source::Slice, v)).collect()),
            Err(kube::Error::Api(s)) if s.is_not_found() => continue,
            Err(e) => {
                warn!(error = %e, "listing CiliumEndpointSlices failed");
                break;
            }
        }
    }
    ("", Vec::new())
}

/// Saves the endpoints, filling in workloads and fallback labels from the
/// pod inventory.
pub fn store(state: &AppState, source: &str, parsed: Vec<(EndpointInfo, Value)>) {
    let view = {
        let inv = state.inventory.read().unwrap();
        build_view(source, parsed.iter().map(|(e, _)| e.clone()).collect(), &inv)
    };
    let raw: HashMap<String, Value> = parsed.into_iter().map(|(e, v)| (e.id, v)).collect();
    *state.endpoints.write().unwrap() = view;
    *state.endpoint_raw.write().unwrap() = raw;
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
    EndpointsView {
        source: source.to_string(),
        endpoints,
        pod_labels,
    }
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

/// Parses a `CiliumEndpoint` object.
pub fn parse_cep(v: &Value) -> Option<EndpointInfo> {
    let namespace = s(v, "/metadata/namespace");
    let name = v.pointer("/metadata/name")?.as_str()?.to_string();
    let st = v.get("status");
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
/// `CiliumEndpoint`: no endpoint id, state or labels).
pub fn parse_ces(v: &Value) -> Vec<(EndpointInfo, Value)> {
    let namespace = s(v, "/namespace");
    v.get("endpoints")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| {
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
            Some((info, e.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_cilium_endpoint() {
        let v = json!({
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
        });
        let e = parse_cep(&v).unwrap();
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
        let v = json!({
            "metadata": {"name": "ces-abc"},
            "namespace": "shop",
            "endpoints": [
                {"name": "web-1", "id": 777, "networking": {"addressing": [{"ipv4": "10.0.1.5"}], "node": "n1"}}
            ]
        });
        let parsed = parse_ces(&v);
        assert_eq!(parsed.len(), 1);
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
}
