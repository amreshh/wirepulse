//! Cilium agent configuration: the `cilium-config` ConfigMap plus any
//! `CiliumNodeConfig` overrides.

use crate::state::AppState;
use common::selector::LabelSelector;
use common::{config_category, ConfigEntry, ConfigView, NodeConfigOverride};
use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{ApiResource, DynamicObject, GroupVersionKind, ListParams};
use kube::{Api, Client};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

pub async fn refresh_loop(client: Client, state: Arc<AppState>, namespace: String, name: String) {
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    loop {
        tick.tick().await;
        let cms: Api<ConfigMap> = Api::namespaced(client.clone(), &namespace);
        let data = match cms.get(&name).await {
            Ok(cm) => cm.data.unwrap_or_default(),
            Err(e) => {
                warn!(error = %e, %namespace, %name, "reading cilium config failed");
                continue;
            }
        };
        let overrides = load_node_configs(&client).await;
        *state.config.write().unwrap() = build_view(format!("{namespace}/{name}"), &data, overrides);
    }
}

async fn load_node_configs(client: &Client) -> Vec<NodeConfigOverride> {
    // CiliumNodeConfig moved from v2alpha1 to v2; try the newer one first.
    for version in ["v2", "v2alpha1"] {
        let ar = ApiResource::from_gvk(&GroupVersionKind::gvk("cilium.io", version, "CiliumNodeConfig"));
        let api: Api<DynamicObject> = Api::all_with(client.clone(), &ar);
        match api.list(&ListParams::default()).await {
            Ok(list) => {
                return list
                    .items
                    .iter()
                    .filter_map(|o| serde_json::to_value(o).ok())
                    .filter_map(|v| parse_node_config(&v))
                    .collect()
            }
            Err(kube::Error::Api(s)) if s.is_not_found() => continue,
            Err(e) => {
                warn!(error = %e, "listing CiliumNodeConfig failed");
                return Vec::new();
            }
        }
    }
    Vec::new()
}

pub fn build_view(
    source: String,
    data: &BTreeMap<String, String>,
    overrides: Vec<NodeConfigOverride>,
) -> ConfigView {
    ConfigView {
        source,
        entries: entries(data.iter()),
        overrides,
    }
}

fn entries<'a>(it: impl Iterator<Item = (&'a String, &'a String)>) -> Vec<ConfigEntry> {
    let mut v: Vec<ConfigEntry> = it
        .map(|(k, v)| ConfigEntry {
            key: k.clone(),
            value: v.clone(),
            category: config_category(k).into(),
        })
        .collect();
    v.sort_by(|a, b| a.key.cmp(&b.key));
    v
}

pub fn parse_node_config(obj: &Value) -> Option<NodeConfigOverride> {
    let meta = obj.get("metadata")?;
    let spec = obj.get("spec")?;
    let defaults: BTreeMap<String, String> = spec
        .get("defaults")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().map_or_else(|| v.to_string(), String::from)))
                .collect()
        })
        .unwrap_or_default();
    let selector: LabelSelector = spec
        .get("nodeSelector")
        .and_then(|s| serde_json::from_value(s.clone()).ok())
        .unwrap_or_default();
    Some(NodeConfigOverride {
        namespace: meta.get("namespace").and_then(Value::as_str).unwrap_or_default().into(),
        name: meta.get("name")?.as_str()?.into(),
        node_selector: selector.describe(),
        entries: entries(defaults.iter()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn node_config() {
        let o = json!({
            "metadata": {"name": "gpu", "namespace": "kube-system"},
            "spec": {"nodeSelector": {"matchLabels": {"pool": "gpu"}},
                     "defaults": {"enable-bpf-masquerade": "false", "mtu": "9000"}}
        });
        let n = parse_node_config(&o).unwrap();
        assert_eq!(n.node_selector, "pool=gpu");
        assert_eq!(n.entries.len(), 2);
        assert_eq!(n.entries[0].key, "enable-bpf-masquerade");
    }
}
