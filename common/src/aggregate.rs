//! Collapse the pod-level graph into namespace or workload level.

use crate::model::{entity_group, node_of, Graph, Node, NodeKind, ROLE_NODE};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Level {
    Namespace,
    Workload,
    Pod,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Namespace => "namespace",
            Level::Workload => "workload",
            Level::Pod => "pod",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AggNode {
    pub id: String,
    pub label: String,
    /// Group the node is drawn in: its namespace, or for other nodes
    /// [`CLUSTER_GROUP`](crate::CLUSTER_GROUP) or
    /// [`EXTERNAL_GROUP`](crate::EXTERNAL_GROUP).
    pub group: String,
    pub kind: NodeKind,
    /// What the node is, for its icon: `Deployment`, `StatefulSet`,
    /// `DaemonSet`, `Job`, `CronJob`, `ReplicaSet`, `Pod`, `Namespace`, or the
    /// reserved entity name (`world`, `host`, `kube-apiserver`, ...).
    pub workload_kind: String,
    /// Number of pods folded into this node.
    pub members: u32,
    pub flows_in: f32,
    pub flows_out: f32,
    pub drops: f32,
    /// Traffic that stays inside this node (only at namespace/workload level).
    pub internal_flows: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AggEdge {
    pub source: String,
    pub target: String,
    pub flows_per_sec: f32,
    pub drops_per_sec: f32,
    pub total_flows: u64,
    pub total_drops: u64,
    pub ports: Vec<String>,
    pub idle_secs: u32,
    /// Number of pod-level edges merged into this one.
    pub members: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AggGraph {
    pub nodes: Vec<AggNode>,
    pub edges: Vec<AggEdge>,
}

impl AggGraph {
    pub fn node(&self, id: &str) -> Option<&AggNode> {
        self.nodes.iter().find(|n| n.id == id)
    }
}

/// Id of the aggregated node a pod-level node belongs to.
pub fn group_id(node: &Node, level: Level) -> String {
    if node.kind == NodeKind::External {
        return node.id.clone();
    }
    match level {
        Level::Pod => node.id.clone(),
        Level::Workload => format!("{}/{}", node.namespace, node.workload),
        Level::Namespace => format!("ns:{}", node.namespace),
    }
}

/// Controller kind from a workload string such as `Deployment/web`.
pub fn kind_of_workload(workload: &str) -> &str {
    const KNOWN: [&str; 8] =
        ["Deployment", "StatefulSet", "DaemonSet", "Job", "CronJob", "ReplicaSet", "ReplicationController", "Pod"];
    match workload.split_once('/') {
        Some((k, _)) if KNOWN.contains(&k) => k,
        _ => "Pod",
    }
}

fn kind_for(node: &Node, level: Level) -> String {
    match (node.kind, level) {
        // The entity's name, or a Kubernetes node's role.
        (NodeKind::External, _) if !node.workload.is_empty() => node.workload.clone(),
        (NodeKind::External, _) => node.name.clone(),
        (_, Level::Namespace) => "Namespace".into(),
        _ => kind_of_workload(&node.workload).into(),
    }
}

fn label_for(node: &Node, level: Level) -> String {
    if node.kind == NodeKind::External {
        return node.name.clone();
    }
    match level {
        Level::Pod => node.name.clone(),
        // "Deployment/frontend" -> "frontend"
        Level::Workload => node.workload.rsplit('/').next().unwrap_or(&node.workload).to_string(),
        Level::Namespace => node.namespace.clone(),
    }
}

/// Nodes referenced by edges but missing from `graph.nodes` (e.g. a pod that
/// was deleted while flows still arrive) are added as stand-alone nodes so the
/// edge still renders.
pub fn aggregate(graph: &Graph, level: Level) -> AggGraph {
    let mut nodes: BTreeMap<String, AggNode> = BTreeMap::new();
    let mut member_of: BTreeMap<&str, String> = BTreeMap::new();

    for n in &graph.nodes {
        let gid = group_id(n, level);
        member_of.insert(n.id.as_str(), gid.clone());
        let group = if n.kind == NodeKind::External { entity_group(&n.id).to_string() } else { n.namespace.clone() };
        let e = nodes.entry(gid.clone()).or_insert_with(|| AggNode {
            id: gid,
            label: label_for(n, level),
            group,
            kind: n.kind,
            workload_kind: kind_for(n, level),
            members: 0,
            flows_in: 0.0,
            flows_out: 0.0,
            drops: 0.0,
            internal_flows: 0.0,
        });
        if n.kind == NodeKind::Pod {
            e.members += 1;
        }
    }

    let resolve = |id: &str, nodes: &mut BTreeMap<String, AggNode>| -> String {
        if let Some(g) = member_of.get(id) {
            return g.clone();
        }
        // Unknown endpoint: keep it at its own id, guess the namespace.
        let (group, label, kind) = match id.split_once('/') {
            Some((ns, name)) => (ns.to_string(), name.to_string(), NodeKind::Pod),
            None => (entity_group(id).to_string(), node_of(id).unwrap_or(id).to_string(), NodeKind::External),
        };
        let gid = match (level, kind) {
            (Level::Namespace, NodeKind::Pod) => format!("ns:{group}"),
            _ => id.to_string(),
        };
        nodes.entry(gid.clone()).or_insert_with(|| AggNode {
            id: gid.clone(),
            label: if level == Level::Namespace && kind == NodeKind::Pod { group.clone() } else { label },
            group,
            kind,
            workload_kind: match (kind, level) {
                (NodeKind::External, _) if node_of(id).is_some() => ROLE_NODE.into(),
                (NodeKind::External, _) => id.strip_prefix("reserved:").unwrap_or(id).to_string(),
                (_, Level::Namespace) => "Namespace".into(),
                _ => "Pod".into(),
            },
            members: 0,
            flows_in: 0.0,
            flows_out: 0.0,
            drops: 0.0,
            internal_flows: 0.0,
        });
        gid
    };

    struct Acc {
        flows: f32,
        drops: f32,
        total_flows: u64,
        total_drops: u64,
        ports: BTreeSet<String>,
        idle: u32,
        members: u32,
    }
    let mut edges: BTreeMap<(String, String), Acc> = BTreeMap::new();

    for e in &graph.edges {
        let s = resolve(&e.source, &mut nodes);
        let t = resolve(&e.target, &mut nodes);
        if s == t {
            if let Some(n) = nodes.get_mut(&s) {
                n.internal_flows += e.flows_per_sec;
                n.drops += e.drops_per_sec;
            }
            continue;
        }
        if let Some(n) = nodes.get_mut(&s) {
            n.flows_out += e.flows_per_sec;
        }
        if let Some(n) = nodes.get_mut(&t) {
            n.flows_in += e.flows_per_sec;
            n.drops += e.drops_per_sec;
        }
        let acc = edges.entry((s, t)).or_insert(Acc {
            flows: 0.0,
            drops: 0.0,
            total_flows: 0,
            total_drops: 0,
            ports: BTreeSet::new(),
            idle: u32::MAX,
            members: 0,
        });
        acc.flows += e.flows_per_sec;
        acc.drops += e.drops_per_sec;
        acc.total_flows += e.total_flows;
        acc.total_drops += e.total_drops;
        acc.idle = acc.idle.min(e.idle_secs);
        acc.members += 1;
        for p in &e.ports {
            if acc.ports.len() < 8 {
                acc.ports.insert(p.clone());
            }
        }
    }

    AggGraph {
        nodes: nodes.into_values().collect(),
        edges: edges
            .into_iter()
            .map(|((source, target), a)| AggEdge {
                source,
                target,
                flows_per_sec: a.flows,
                drops_per_sec: a.drops,
                total_flows: a.total_flows,
                total_drops: a.total_drops,
                ports: a.ports.into_iter().collect(),
                idle_secs: a.idle,
                members: a.members,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Edge, CLUSTER_GROUP, EXTERNAL_GROUP};

    fn pod(ns: &str, name: &str, wl: &str) -> Node {
        Node {
            id: format!("{ns}/{name}"),
            kind: NodeKind::Pod,
            namespace: ns.into(),
            name: name.into(),
            workload: wl.into(),
            node_name: String::new(),
            ip: String::new(),
            phase: "Running".into(),
        }
    }

    fn edge(s: &str, t: &str, f: f32) -> Edge {
        Edge {
            source: s.into(),
            target: t.into(),
            flows_per_sec: f,
            drops_per_sec: 0.0,
            total_flows: 10,
            total_drops: 0,
            ports: vec!["TCP/80".into()],
            idle_secs: 1,
        }
    }

    fn sample() -> Graph {
        Graph {
            nodes: vec![
                pod("shop", "fe-1", "Deployment/fe"),
                pod("shop", "fe-2", "Deployment/fe"),
                pod("shop", "db-0", "StatefulSet/db"),
                pod("pay", "api-1", "Deployment/api"),
                Node {
                    id: "reserved:world".into(),
                    kind: NodeKind::External,
                    namespace: EXTERNAL_GROUP.into(),
                    name: "world".into(),
                    workload: "world".into(),
                    node_name: String::new(),
                    ip: String::new(),
                    phase: String::new(),
                },
            ],
            edges: vec![
                edge("shop/fe-1", "shop/db-0", 2.0),
                edge("shop/fe-2", "shop/db-0", 3.0),
                edge("shop/fe-1", "pay/api-1", 1.0),
                edge("pay/api-1", "reserved:world", 0.5),
                edge("gone/old-pod", "shop/db-0", 0.25),
                edge("shop/fe-1", "reserved:kube-apiserver", 0.1),
            ],
            generated_at_ms: 0,
        }
    }

    #[test]
    fn pod_level_keeps_everything() {
        let g = aggregate(&sample(), Level::Pod);
        // 5 known nodes + 2 only referenced by an edge
        assert_eq!(g.nodes.len(), 7);
        assert_eq!(g.edges.len(), 6);
    }

    #[test]
    fn kubernetes_nodes_keep_their_name_and_role() {
        let mut g = sample();
        g.nodes.push(Node {
            id: "node:cp-1".into(),
            kind: NodeKind::External,
            namespace: CLUSTER_GROUP.into(),
            name: "cp-1".into(),
            workload: crate::model::ROLE_CONTROL_PLANE.into(),
            node_name: "cp-1".into(),
            ip: "192.168.10.10".into(),
            phase: String::new(),
        });
        g.edges.push(edge("shop/fe-1", "node:cp-1", 0.2));
        // Only in an edge: the server had no CiliumNode for it.
        g.edges.push(edge("node:worker-9", "shop/fe-1", 0.2));
        for level in [Level::Pod, Level::Workload, Level::Namespace] {
            let a = aggregate(&g, level);
            let cp = a.node("node:cp-1").unwrap();
            assert_eq!((cp.label.as_str(), cp.workload_kind.as_str()), ("cp-1", "control-plane"));
            assert_eq!((cp.group.as_str(), cp.kind), (CLUSTER_GROUP, NodeKind::External));
            let w = a.node("node:worker-9").unwrap();
            assert_eq!(
                (w.label.as_str(), w.workload_kind.as_str(), w.group.as_str()),
                ("worker-9", "node", CLUSTER_GROUP)
            );
        }
    }

    #[test]
    fn only_world_is_grouped_outside_the_cluster() {
        for level in [Level::Pod, Level::Workload, Level::Namespace] {
            let g = aggregate(&sample(), level);
            assert_eq!(g.node("reserved:world").unwrap().group, EXTERNAL_GROUP);
            let api = g.node("reserved:kube-apiserver").unwrap();
            assert_eq!((api.group.as_str(), api.kind), (CLUSTER_GROUP, NodeKind::External));
            assert_eq!(api.workload_kind, "kube-apiserver");
        }
    }

    #[test]
    fn workload_level_merges_replicas() {
        let g = aggregate(&sample(), Level::Workload);
        let fe = g.node("shop/Deployment/fe").unwrap();
        assert_eq!(fe.members, 2);
        assert_eq!(fe.workload_kind, "Deployment");
        assert_eq!(g.node("shop/StatefulSet/db").unwrap().workload_kind, "StatefulSet");
        assert_eq!(g.node("reserved:world").unwrap().workload_kind, "world");
        assert_eq!(fe.label, "fe");
        let e = g.edges.iter().find(|e| e.source == "shop/Deployment/fe" && e.target == "shop/StatefulSet/db").unwrap();
        assert_eq!(e.flows_per_sec, 5.0);
        assert_eq!(e.members, 2);
    }

    #[test]
    fn namespace_level_turns_intra_namespace_into_internal() {
        let g = aggregate(&sample(), Level::Namespace);
        let shop = g.node("ns:shop").unwrap();
        assert_eq!(shop.internal_flows, 5.0);
        assert_eq!(shop.members, 3);
        assert!(g.node("ns:gone").is_some());
        assert!(g.node("reserved:world").is_some());
        assert!(g.edges.iter().all(|e| e.source != e.target));
        assert!(g.edges.iter().any(|e| e.source == "ns:shop" && e.target == "ns:pay" && e.flows_per_sec == 1.0));
    }
}
