//! The cluster's nodes, from `CiliumNode` objects, so flows to and from a
//! node's own addresses name the node.
//!
//! Hubble reports those addresses as Cilium's reserved entities: `host` on
//! the node that saw the flow, `remote-node` on every other one, and
//! `kube-apiserver` for a node the API server runs on, which then also covers
//! its kubelet, node-exporter and the like. A `CiliumNode` lists all of a
//! node's addresses: InternalIP, ExternalIP, and `CiliumInternalIP`, the
//! router address its host-network traffic to pods comes from. So the server
//! looks the IP up and names the node, `node:<name>`.

use crate::endpoints::{objects, Watched};
use crate::state::AppState;
use common::{node_id, EndpointDetail, Node, NodeKind, CLUSTER_GROUP, ROLE_CONTROL_PLANE, ROLE_NODE};
use kube::api::DynamicObject;
use kube::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

/// How often the index is rebuilt from the watched objects.
const REFRESH: Duration = Duration::from_secs(10);
/// Rounds between attempts to start the watch while CiliumNodes can't be
/// listed (say, with the RBAC of an older release): once a minute.
const RETRY_ROUNDS: u32 = 6;

/// Cilium's reserved identity for the node that saw a flow.
const HOST_IDENTITY: u32 = 1;

/// The ids `hubble::endpoint_id` gives a node's addresses.
const NODE_ENTITIES: [&str; 3] = ["reserved:host", "reserved:remote-node", "reserved:kube-apiserver"];

/// Node labels that mark the control plane (`master` on older clusters).
/// Cilium copies a node's labels to its CiliumNode.
const CONTROL_PLANE_LABELS: [&str; 2] = ["node-role.kubernetes.io/control-plane", "node-role.kubernetes.io/master"];

pub async fn refresh_loop(client: Client, state: Arc<AppState>) {
    let mut watched: Option<Watched> = None;
    let mut tick = tokio::time::interval(REFRESH);
    let mut round: u32 = 0;
    loop {
        tick.tick().await;
        if watched.is_none() && round.is_multiple_of(RETRY_ROUNDS) {
            watched = Watched::start(&client, "CiliumNode", &["v2"], slim).await;
        }
        round = round.wrapping_add(1);
        if let Some(objects) = objects(&mut watched) {
            let nodes = objects.iter().filter_map(|o| parse(o)).collect();
            *state.node_index.write().unwrap() = NodeIndex::new(nodes);
        }
    }
}

/// Whether `id` is one of the entities Hubble reports a node's addresses as.
pub fn is_node_entity(id: &str) -> bool {
    NODE_ENTITIES.contains(&id)
}

/// Keeps what [`parse`] reads: the name, labels and addresses. The rest of a
/// CiliumNode can be big, such as the IPAM pool of an ENI or Azure cluster.
fn slim(o: &mut DynamicObject) {
    o.metadata.annotations = None;
    o.metadata.owner_references = None;
    let addresses = o.data.pointer_mut("/spec/addresses").map(Value::take).unwrap_or_default();
    o.data = json!({ "spec": { "addresses": addresses } });
}

/// What the server knows of a node.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeInfo {
    pub name: String,
    /// Every address its CiliumNode lists.
    pub ips: Vec<IpAddr>,
    /// The address the map shows: its InternalIP, IPv4 if it has one.
    pub ip: String,
    pub control_plane: bool,
}

/// Parses a `CiliumNode`.
pub fn parse(o: &DynamicObject) -> Option<NodeInfo> {
    let name = o.metadata.name.clone()?;
    let mut ips = Vec::new();
    let mut internal = Vec::new();
    for a in o.data.pointer("/spec/addresses").and_then(Value::as_array).into_iter().flatten() {
        let Some(ip) = a.get("ip").and_then(Value::as_str).and_then(|s| s.parse::<IpAddr>().ok()) else {
            continue;
        };
        if a.get("type").and_then(Value::as_str) == Some("InternalIP") {
            internal.push(ip);
        }
        ips.push(ip);
    }
    let shown = internal.iter().find(|ip| ip.is_ipv4()).or(internal.first()).or(ips.first());
    let ip = shown.map(ToString::to_string).unwrap_or_default();
    let labels = o.metadata.labels.as_ref();
    let control_plane = labels.is_some_and(|l| CONTROL_PLANE_LABELS.iter().any(|k| l.contains_key(*k)));
    Some(NodeInfo { name, ips, ip, control_plane })
}

/// The cluster's nodes, by name and by address.
#[derive(Default)]
pub struct NodeIndex {
    /// Sorted by name.
    nodes: Vec<NodeInfo>,
    /// Address -> index in `nodes`.
    by_ip: HashMap<IpAddr, usize>,
}

impl NodeIndex {
    pub fn new(mut nodes: Vec<NodeInfo>) -> Self {
        nodes.sort_by(|a, b| a.name.cmp(&b.name));
        let by_ip = nodes.iter().enumerate().flat_map(|(i, n)| n.ips.iter().map(move |ip| (*ip, i))).collect();
        Self { nodes, by_ip }
    }

    /// Names the node behind `ep` when Hubble reported it as one of
    /// Cilium's node entities: the node with its IP, or for `host`, the node
    /// that saw the flow (`observer`, Hubble's node name, which may start
    /// with the cluster's name). Anything else stays as it is: a node of
    /// another cluster in a mesh, or an API server that isn't on a node.
    /// Returns whether `ep` changed.
    pub fn resolve(&self, ep: &mut EndpointDetail, observer: &str) -> bool {
        if !is_node_entity(&ep.id) {
            return false;
        }
        let by_ip = ep.ip.parse::<IpAddr>().ok().and_then(|ip| self.by_ip.get(&ip));
        let name = match by_ip {
            Some(&i) => self.nodes[i].name.as_str(),
            // `host` is always the node that saw the flow, also at an address
            // its CiliumNode doesn't list (or without CiliumNodes at all).
            None if is_host(ep) => observer.rsplit('/').next().unwrap_or_default(),
            None => "",
        };
        if name.is_empty() {
            return false;
        }
        ep.id = node_id(name);
        true
    }

    /// Every node for the map, also those without traffic (like idle pods).
    pub fn map_nodes(&self) -> impl Iterator<Item = Node> + '_ {
        self.nodes.iter().map(|n| map_node(&n.name, Some(n)))
    }
}

fn is_host(ep: &EndpointDetail) -> bool {
    ep.identity == HOST_IDENTITY || ep.labels.iter().any(|l| l == "reserved:host")
}

/// The map's node for a Kubernetes node; `info` when its CiliumNode is known.
pub fn map_node(name: &str, info: Option<&NodeInfo>) -> Node {
    Node {
        id: node_id(name),
        kind: NodeKind::External,
        namespace: CLUSTER_GROUP.into(),
        name: name.into(),
        workload: if info.is_some_and(|n| n.control_plane) { ROLE_CONTROL_PLANE } else { ROLE_NODE }.into(),
        node_name: String::new(),
        ip: info.map(|n| n.ip.clone()).unwrap_or_default(),
        phase: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cilium_node(name: &str, labels: Value, addresses: Value) -> DynamicObject {
        serde_json::from_value(json!({
            "apiVersion": "cilium.io/v2",
            "kind": "CiliumNode",
            "metadata": {"name": name, "labels": labels, "annotations": {"a": "b"}},
            "spec": {"addresses": addresses, "ipam": {"podCIDRs": ["10.0.1.0/24"]}, "health": {"ipv4": "10.0.1.99"}},
            "status": {"ipam": {"operator-status": {}}}
        }))
        .unwrap()
    }

    fn index() -> NodeIndex {
        let cp = cilium_node(
            "cp-1",
            json!({"kubernetes.io/hostname": "cp-1", "node-role.kubernetes.io/control-plane": ""}),
            json!([
                {"type": "InternalIP", "ip": "fd00::10"},
                {"type": "InternalIP", "ip": "192.168.10.10"},
                {"type": "CiliumInternalIP", "ip": "10.0.0.1"},
            ]),
        );
        let worker = cilium_node(
            "worker-1",
            json!({"kubernetes.io/hostname": "worker-1"}),
            json!([{"type": "InternalIP", "ip": "192.168.10.1"}, {"type": "CiliumInternalIP", "ip": "10.0.1.1"}]),
        );
        NodeIndex::new([worker, cp].iter().filter_map(parse).collect())
    }

    fn ep(id: &str, identity: u32, ip: &str, labels: &[&str]) -> EndpointDetail {
        EndpointDetail {
            id: id.into(),
            identity,
            ip: ip.into(),
            labels: labels.iter().map(|l| l.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn parses_slimmed_cilium_nodes() {
        let mut o = cilium_node(
            "cp-1",
            json!({"node-role.kubernetes.io/master": "true"}),
            json!([{"type": "CiliumInternalIP", "ip": "10.0.0.1"}, {"type": "InternalIP", "ip": "fd00::10"}]),
        );
        slim(&mut o);
        assert_eq!(
            o.data,
            json!({"spec": {"addresses": [
                {"type": "CiliumInternalIP", "ip": "10.0.0.1"}, {"type": "InternalIP", "ip": "fd00::10"}
            ]}})
        );
        assert!(o.metadata.annotations.is_none());
        let n = parse(&o).unwrap();
        // The InternalIP, even an IPv6 one, over the router address.
        assert_eq!((n.ip.as_str(), n.ips.len(), n.control_plane), ("fd00::10", 2, true));
    }

    #[test]
    fn names_the_node_behind_cilium_node_entities() {
        let ix = index();
        // The API server's node, with everything else on it.
        let mut e =
            ep("reserved:kube-apiserver", 7, "192.168.10.10", &["reserved:kube-apiserver", "reserved:remote-node"]);
        assert!(ix.resolve(&mut e, "default/worker-1"));
        assert_eq!(e.id, "node:cp-1");
        // Host-network traffic to pods on other nodes comes from the router address.
        let mut e = ep("reserved:remote-node", 6, "10.0.1.1", &["reserved:remote-node"]);
        assert!(ix.resolve(&mut e, "default/cp-1"));
        assert_eq!(e.id, "node:worker-1");
        // `host` is the node that saw the flow, at whatever address.
        let mut e = ep("reserved:host", 1, "172.17.0.1", &["reserved:host"]);
        assert!(ix.resolve(&mut e, "default/worker-1"));
        assert_eq!(e.id, "node:worker-1");
        let mut e = ep("reserved:host", 1, "172.17.0.1", &["reserved:host"]);
        assert!(!NodeIndex::default().resolve(&mut e, ""));
        // A node of another cluster, an API server on no node, and the world
        // (even at a node's address) stay as they are.
        for (id, identity, ip) in [
            ("reserved:remote-node", 6, "192.168.20.1"),
            ("reserved:kube-apiserver", 7, "172.20.0.1"),
            ("reserved:world", 2, "192.168.10.10"),
        ] {
            let mut e = ep(id, identity, ip, &[]);
            assert!(!ix.resolve(&mut e, "default/worker-1"));
            assert_eq!(e.id, id);
        }
    }

    #[test]
    fn map_nodes_have_their_role_and_address() {
        let nodes: Vec<Node> = index().map_nodes().collect();
        let got: Vec<_> = nodes.iter().map(|n| (n.id.as_str(), n.workload.as_str(), n.ip.as_str())).collect();
        assert_eq!(got, [("node:cp-1", "control-plane", "192.168.10.10"), ("node:worker-1", "node", "192.168.10.1")]);
        assert!(nodes.iter().all(|n| n.kind == NodeKind::External && n.namespace == CLUSTER_GROUP));
    }
}
