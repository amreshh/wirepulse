//! CiliumNetworkPolicy, CiliumClusterwideNetworkPolicy and NetworkPolicy:
//! listing, summarising, and working out which pods each one selects.

use crate::state::AppState;
use common::selector::LabelSelector;
use common::{PolicyInfo, PolicyKind};
use kube::api::{ApiResource, DynamicObject, GroupVersionKind, ListParams};
use kube::{Api, Client};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

const TYPES: [(PolicyKind, &str, &str, &str); 3] = [
    (PolicyKind::CiliumNetworkPolicy, "cilium.io", "v2", "CiliumNetworkPolicy"),
    (
        PolicyKind::CiliumClusterwideNetworkPolicy,
        "cilium.io",
        "v2",
        "CiliumClusterwideNetworkPolicy",
    ),
    (PolicyKind::NetworkPolicy, "networking.k8s.io", "v1", "NetworkPolicy"),
];

pub async fn refresh_loop(client: Client, state: Arc<AppState>) {
    // Pod selection needs the inventory; give the watchers a moment first.
    for _ in 0..20 {
        if state.status.read().unwrap().k8s_connected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let mut tick = tokio::time::interval(Duration::from_secs(15));
    loop {
        tick.tick().await;
        let objects = load_all(&client).await;
        let policies = {
            let inv = state.inventory.read().unwrap();
            parse_all(&objects, &inv.labels)
        };
        *state.policies.write().unwrap() = policies;
    }
}

/// Lists every policy type. A type whose CRD is missing is skipped; a type
/// that fails (e.g. RBAC) is logged and skipped so the others still show.
pub async fn load_all(client: &Client) -> Vec<(PolicyKind, Value)> {
    let mut out = Vec::new();
    for (kind, group, version, k8s_kind) in TYPES {
        let ar = ApiResource::from_gvk(&GroupVersionKind::gvk(group, version, k8s_kind));
        let api: Api<DynamicObject> = Api::all_with(client.clone(), &ar);
        match api.list(&ListParams::default()).await {
            Ok(list) => {
                for o in list.items {
                    if let Ok(mut v) = serde_json::to_value(&o) {
                        // List items come without apiVersion/kind.
                        v["apiVersion"] = Value::String(ar.api_version.clone());
                        v["kind"] = Value::String(ar.kind.clone());
                        out.push((kind, v));
                    }
                }
            }
            Err(kube::Error::Api(s)) if s.is_not_found() => {}
            Err(e) => warn!(error = %e, kind = k8s_kind, "listing policies failed"),
        }
    }
    out
}

pub fn parse_all(
    objects: &[(PolicyKind, Value)],
    labels: &HashMap<String, BTreeMap<String, String>>,
) -> Vec<PolicyInfo> {
    let mut v: Vec<PolicyInfo> = objects
        .iter()
        .filter_map(|(k, o)| parse(*k, o, labels))
        .collect();
    v.sort_by(|a, b| {
        (a.namespace.as_deref().unwrap_or(""), &a.name).cmp(&(b.namespace.as_deref().unwrap_or(""), &b.name))
    });
    v
}

fn count(spec: &Value, key: &str) -> u32 {
    spec.get(key)
        .and_then(Value::as_array)
        .map_or(0, |a| a.len() as u32)
}

fn selector(v: Option<&Value>) -> Option<LabelSelector> {
    serde_json::from_value(v?.clone()).ok()
}

pub fn parse(
    kind: PolicyKind,
    obj: &Value,
    labels: &HashMap<String, BTreeMap<String, String>>,
) -> Option<PolicyInfo> {
    let meta = obj.get("metadata")?;
    let name = meta.get("name")?.as_str()?.to_string();
    let namespace = match kind {
        PolicyKind::CiliumClusterwideNetworkPolicy => None,
        _ => meta.get("namespace").and_then(Value::as_str).map(String::from),
    };

    let mut endpoint_selectors = Vec::new();
    let mut node_selectors = Vec::new();
    let (mut ing, mut eg, mut ing_deny, mut eg_deny) = (0, 0, 0, 0);

    match kind {
        PolicyKind::NetworkPolicy => {
            let spec = obj.get("spec")?;
            endpoint_selectors.push(selector(spec.get("podSelector")).unwrap_or_default());
            ing = count(spec, "ingress");
            eg = count(spec, "egress");
        }
        _ => {
            // A Cilium policy has `spec`, `specs`, or both.
            let mut rules: Vec<&Value> = Vec::new();
            if let Some(s) = obj.get("spec").filter(|s| s.is_object()) {
                rules.push(s);
            }
            if let Some(a) = obj.get("specs").and_then(Value::as_array) {
                rules.extend(a.iter());
            }
            for r in rules {
                if let Some(s) = selector(r.get("endpointSelector")) {
                    endpoint_selectors.push(s);
                } else if let Some(s) = selector(r.get("nodeSelector")) {
                    node_selectors.push(s);
                }
                ing += count(r, "ingress");
                eg += count(r, "egress");
                ing_deny += count(r, "ingressDeny");
                eg_deny += count(r, "egressDeny");
            }
        }
    }

    let mut selected: Vec<String> = labels
        .iter()
        .filter(|(id, _)| match &namespace {
            Some(ns) => id.split_once('/').is_some_and(|(n, _)| n == ns),
            None => true,
        })
        .filter(|(_, l)| endpoint_selectors.iter().any(|s| s.matches(l)))
        .map(|(id, _)| id.clone())
        .collect();
    selected.sort();

    let mut desc: Vec<String> = endpoint_selectors.iter().map(LabelSelector::describe).collect();
    desc.extend(node_selectors.iter().map(|s| format!("nodes: {}", s.describe())));

    let valid = obj
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .and_then(|c| {
            c.iter()
                .find(|c| c.get("type").and_then(Value::as_str) == Some("Valid"))
        })
        .and_then(|c| c.get("status").and_then(Value::as_str))
        .map(|s| s == "True");

    Some(PolicyInfo {
        kind,
        namespace,
        name,
        ingress_rules: ing,
        egress_rules: eg,
        ingress_deny_rules: ing_deny,
        egress_deny_rules: eg_deny,
        selector: desc.join(" | "),
        selected,
        host_policy: !node_selectors.is_empty(),
        valid,
        created: meta
            .get("creationTimestamp")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        yaml: to_yaml(obj),
    })
}

/// YAML without the noise `kubectl get -o yaml` usually hides.
pub fn to_yaml(obj: &Value) -> String {
    let mut o = obj.clone();
    if let Some(m) = o.get_mut("metadata").and_then(Value::as_object_mut) {
        m.remove("managedFields");
        if let Some(a) = m.get_mut("annotations").and_then(Value::as_object_mut) {
            a.remove("kubectl.kubernetes.io/last-applied-configuration");
            if a.is_empty() {
                m.remove("annotations");
            }
        }
    }
    serde_yaml_ng::to_string(&o).unwrap_or_else(|e| format!("# could not render: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn labels() -> HashMap<String, BTreeMap<String, String>> {
        let mk = |ns: &str, app: &str| -> BTreeMap<String, String> {
            [
                ("app".to_string(), app.to_string()),
                ("io.kubernetes.pod.namespace".to_string(), ns.to_string()),
            ]
            .into()
        };
        [
            ("shop/web-1".to_string(), mk("shop", "web")),
            ("shop/db-0".to_string(), mk("shop", "db")),
            ("pay/web-1".to_string(), mk("pay", "web")),
        ]
        .into()
    }

    #[test]
    fn cnp_selects_only_its_namespace() {
        let o = json!({
            "metadata": {"name": "web", "namespace": "shop", "managedFields": [{}]},
            "spec": {"endpointSelector": {"matchLabels": {"k8s:app": "web"}},
                     "ingress": [{}, {}], "egressDeny": [{}]},
            "status": {"conditions": [{"type": "Valid", "status": "True"}]}
        });
        let p = parse(PolicyKind::CiliumNetworkPolicy, &o, &labels()).unwrap();
        assert_eq!(p.selected, vec!["shop/web-1"]);
        assert_eq!((p.ingress_rules, p.egress_deny_rules), (2, 1));
        assert_eq!(p.valid, Some(true));
        assert!(!p.yaml.contains("managedFields"));
    }

    #[test]
    fn ccnp_spans_namespaces_and_specs() {
        let o = json!({
            "metadata": {"name": "all-web"},
            "specs": [
                {"endpointSelector": {"matchLabels": {"app": "web"}}, "egress": [{}]},
                {"nodeSelector": {}, "ingress": [{}]}
            ]
        });
        let p = parse(PolicyKind::CiliumClusterwideNetworkPolicy, &o, &labels()).unwrap();
        assert_eq!(p.selected, vec!["pay/web-1", "shop/web-1"]);
        assert!(p.host_policy);
        assert_eq!(p.namespace, None);
        assert_eq!(p.selector, "app=web | nodes: (all)");
    }

    #[test]
    fn netpol_empty_selector_selects_namespace() {
        let o = json!({
            "metadata": {"name": "deny", "namespace": "shop"},
            "spec": {"podSelector": {}, "policyTypes": ["Ingress"]}
        });
        let p = parse(PolicyKind::NetworkPolicy, &o, &labels()).unwrap();
        assert_eq!(p.selected, vec!["shop/db-0", "shop/web-1"]);
    }
}
