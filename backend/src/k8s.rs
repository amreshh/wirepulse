//! Live inventory of namespaces and pods, kept in memory by kube-rs reflectors.

use crate::state::{AppState, Inventory};
use common::{Node, NodeKind};
use futures::{Stream, StreamExt};
use k8s_openapi::api::core::v1::{Namespace, Pod};
use kube::runtime::{reflector, watcher, WatchStreamExt};
use kube::{Api, Client, Resource, ResourceExt};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

pub async fn run_inventory(client: Client, state: Arc<AppState>) {
    let (pods, pods_w) = reflector::store::<Pod>();
    let (nss, nss_w) = reflector::store::<Namespace>();

    let pod_stream = watcher(Api::<Pod>::all(client.clone()), watcher::Config::default())
        .default_backoff()
        .reflect(pods_w)
        .applied_objects();
    let ns_stream = watcher(Api::<Namespace>::all(client), watcher::Config::default())
        .default_backoff()
        .reflect(nss_w)
        .applied_objects();
    tokio::spawn(drain(pod_stream, "pods", state.clone()));
    tokio::spawn(drain(ns_stream, "namespaces", state.clone()));

    if pods.wait_until_ready().await.is_err() || nss.wait_until_ready().await.is_err() {
        warn!("kubernetes watchers stopped before becoming ready");
        return;
    }
    info!(pods = pods.state().len(), "kubernetes inventory ready");
    state.update_status(|s| s.k8s_connected = true);

    let mut tick = tokio::time::interval(Duration::from_secs(2));
    loop {
        tick.tick().await;
        let inv = build_inventory(&pods.state(), &nss.state());
        *state.inventory.write().unwrap() = inv;
    }
}

async fn drain<K: Debug, S>(stream: S, what: &'static str, state: Arc<AppState>)
where
    S: Stream<Item = Result<K, watcher::Error>>,
{
    let mut stream = std::pin::pin!(stream);
    while let Some(item) = stream.next().await {
        match item {
            Ok(_) => state.update_status(|s| s.k8s_connected = true),
            Err(e) => {
                warn!(error = %e, what, "kubernetes watch error");
                state.update_status(|s| {
                    s.k8s_connected = false;
                    s.message = format!("watching {what}: {e}");
                });
            }
        }
    }
}

pub fn build_inventory(pods: &[Arc<Pod>], namespaces: &[Arc<Namespace>]) -> Inventory {
    let ns_labels: HashMap<String, BTreeMap<String, String>> =
        namespaces.iter().map(|n| (n.name_any(), n.labels().clone())).collect();
    let mut inv = Inventory::default();
    for p in pods {
        let Some(node) = pod_node(p) else { continue };
        inv.labels.insert(node.id.clone(), policy_labels(p, ns_labels.get(&node.namespace)));
        inv.nodes.push(node);
    }
    inv.nodes.sort_by(|a, b| a.id.cmp(&b.id));
    inv
}

fn pod_node(p: &Pod) -> Option<Node> {
    let ns = p.namespace()?;
    let name = p.name_any();
    let status = p.status.as_ref();
    let phase = status.and_then(|s| s.phase.clone()).unwrap_or_default();
    // Completed job pods would clutter the map.
    if phase == "Succeeded" || phase == "Failed" {
        return None;
    }
    Some(Node {
        id: format!("{ns}/{name}"),
        kind: NodeKind::Pod,
        workload: workload_of(p),
        namespace: ns,
        name,
        node_name: p.spec.as_ref().and_then(|s| s.node_name.clone()).unwrap_or_default(),
        ip: status.and_then(|s| s.pod_ip.clone()).unwrap_or_default(),
        phase,
    })
}

/// `Deployment/api`, `StatefulSet/db`, `DaemonSet/cilium`, `CronJob/backup`, `Pod/x`.
pub fn workload_of(p: &Pod) -> String {
    let Some(owner) = p.owner_references().iter().find(|o| o.controller == Some(true)) else {
        return format!("Pod/{}", p.name_any());
    };
    match owner.kind.as_str() {
        "ReplicaSet" => {
            if let Some(hash) = p.labels().get("pod-template-hash") {
                if let Some(base) = owner.name.strip_suffix(&format!("-{hash}")) {
                    return format!("Deployment/{base}");
                }
            }
            format!("ReplicaSet/{}", owner.name)
        }
        "Job" => match owner.name.rsplit_once('-') {
            // CronJobs name their jobs `<cronjob>-<minutes since epoch>`.
            Some((base, ts)) if ts.len() >= 8 && ts.chars().all(|c| c.is_ascii_digit()) => {
                format!("CronJob/{base}")
            }
            _ => format!("Job/{}", owner.name),
        },
        kind => format!("{kind}/{}", owner.name),
    }
}

/// Pod labels plus the synthetic labels Cilium adds to every endpoint, so
/// selectors such as `k8s:io.kubernetes.pod.namespace: shop` match.
pub fn policy_labels(p: &Pod, ns_labels: Option<&BTreeMap<String, String>>) -> BTreeMap<String, String> {
    let mut l = p.labels().clone();
    let ns = p.meta().namespace.clone().unwrap_or_default();
    let sa = p.spec.as_ref().and_then(|s| s.service_account_name.clone()).unwrap_or_else(|| "default".into());
    l.insert("io.kubernetes.pod.namespace".into(), ns);
    l.insert("io.cilium.k8s.policy.serviceaccount".into(), sa);
    if let Some(nl) = ns_labels {
        for (k, v) in nl {
            l.insert(format!("io.cilium.k8s.namespace.labels.{k}"), v.clone());
        }
    }
    l
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

    fn pod(name: &str, owner_kind: &str, owner: &str, hash: Option<&str>) -> Pod {
        let mut labels = BTreeMap::new();
        if let Some(h) = hash {
            labels.insert("pod-template-hash".to_string(), h.to_string());
        }
        Pod {
            metadata: ObjectMeta {
                name: Some(name.into()),
                namespace: Some("shop".into()),
                labels: Some(labels),
                owner_references: Some(vec![OwnerReference {
                    kind: owner_kind.into(),
                    name: owner.into(),
                    controller: Some(true),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn workloads() {
        assert_eq!(workload_of(&pod("api-5d8f-abcde", "ReplicaSet", "api-5d8f", Some("5d8f"))), "Deployment/api");
        assert_eq!(workload_of(&pod("db-0", "StatefulSet", "db", None)), "StatefulSet/db");
        assert_eq!(workload_of(&pod("backup-29000000-xyz", "Job", "backup-29000000", None)), "CronJob/backup");
    }

    #[test]
    fn synthetic_labels() {
        let p = pod("db-0", "StatefulSet", "db", None);
        let nl: BTreeMap<_, _> = [("team".to_string(), "core".to_string())].into();
        let l = policy_labels(&p, Some(&nl));
        assert_eq!(l["io.kubernetes.pod.namespace"], "shop");
        assert_eq!(l["io.cilium.k8s.namespace.labels.team"], "core");
        assert_eq!(l["io.cilium.k8s.policy.serviceaccount"], "default");
    }
}
