//! Shared in-memory state. Nothing is persisted: the server only keeps the
//! current inventory, edge rates and a buffer of recent flows.

use crate::endpoints::{EndpointIndex, EndpointObject};
use crate::flows::FlowAggregator;
use crate::nodes::{self, NodeIndex};
use common::{
    entity_group, is_unresolved, node_of, ConfigView, EndpointsView, FlowDetail, FlowEvent, Graph, Info, Node,
    NodeKind, PolicyInfo, ServerMsg, Status,
};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

/// Flows a page starts with when it doesn't say how many it wants.
pub const DEFAULT_SNAPSHOT: usize = 1000;
/// Flows kept in full unless `--flow-history` says otherwise.
pub const DEFAULT_FLOW_HISTORY: u32 = 5000;
/// Flows forwarded to the UI per flush; the rest still count towards rates.
const MAX_FLOWS_PER_FLUSH: usize = 400;
/// Older flows (e.g. Hubble's history on connect) go to the table only.
const MAX_FLOW_AGE_FOR_RATES_MS: i64 = 10_000;
/// Idle edges to deleted pods are hidden after this long.
const STALE_POD_SECS: u32 = 30;

#[derive(Default)]
pub struct Inventory {
    pub nodes: Vec<Node>,
    /// Labels per pod id, including Cilium's synthetic labels
    /// (`io.kubernetes.pod.namespace`, namespace labels, service account).
    pub labels: HashMap<String, BTreeMap<String, String>>,
}

pub struct AppState {
    pub info: Info,
    /// Changes every time the server starts (see `Status::instance`).
    pub instance: u64,
    pub status: RwLock<Status>,
    pub inventory: RwLock<Inventory>,
    pub flows: Mutex<FlowAggregator>,
    /// The newest flows in full, oldest first: for the details panel
    /// (`/api/flows/{id}`) and the flows a page starts with.
    details: Mutex<VecDeque<FlowDetail>>,
    /// How many flows `details` keeps (`--flow-history`).
    history: usize,
    next_id: AtomicU64,
    pending: Mutex<Vec<FlowEvent>>,
    pub policies: RwLock<Vec<PolicyInfo>>,
    pub config: RwLock<ConfigView>,
    pub endpoints: RwLock<EndpointsView>,
    /// The objects behind `endpoints`, by endpoint id, for the YAML view.
    /// They're the watches' cached objects, not copies.
    pub endpoint_objects: RwLock<HashMap<String, EndpointObject>>,
    /// `endpoints` by IP and identity, for flows Hubble sends without a pod.
    pub endpoint_index: RwLock<EndpointIndex>,
    /// The cluster's nodes by address, to name the node behind Cilium's
    /// `host`, `remote-node` and `kube-apiserver` entities.
    pub node_index: RwLock<NodeIndex>,
    latest_graph: RwLock<Arc<str>>,
    pub tx: broadcast::Sender<Arc<str>>,
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Which build this is, from settings the Dockerfile sets (WIREPULSE_VERSION and
/// WIREPULSE_COMMIT) rather than compiled in: a new commit then rebuilds nothing, and
/// CI can release an image it already built and tested by changing only these.
fn build_env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

/// The release version, or the crate's version for builds that aren't a release.
fn build_version() -> String {
    Some(build_env("WIREPULSE_VERSION")).filter(|v| !v.is_empty()).unwrap_or_else(|| env!("CARGO_PKG_VERSION").into())
}

fn encode(msg: &ServerMsg) -> Arc<str> {
    serde_json::to_string(msg).unwrap_or_else(|_| "{}".into()).into()
}

impl AppState {
    /// `history`: how many flows to keep in full (`--flow-history`).
    pub fn new(demo: bool, history: u32) -> Arc<Self> {
        let (tx, _) = broadcast::channel(64);
        let history = history.max(1);
        // Start times differ between runs, which is all an instance needs.
        let instance = now_ms().unsigned_abs();
        Arc::new(Self {
            info: Info {
                version: build_version(),
                commit: build_env("WIREPULSE_COMMIT"),
                demo,
                cluster_name: String::new(),
                flow_history: history,
            },
            instance,
            status: RwLock::new(Status { demo, instance, ..Default::default() }),
            inventory: RwLock::default(),
            flows: Mutex::default(),
            // It fills up over time; don't reserve a big history up front.
            details: Mutex::new(VecDeque::with_capacity((history as usize).min(DEFAULT_SNAPSHOT))),
            history: history as usize,
            next_id: AtomicU64::new(1),
            pending: Mutex::default(),
            policies: RwLock::default(),
            config: RwLock::default(),
            endpoints: RwLock::default(),
            endpoint_objects: RwLock::default(),
            endpoint_index: RwLock::default(),
            node_index: RwLock::default(),
            latest_graph: RwLock::new(encode(&ServerMsg::Graph(Graph::default()))),
            tx,
        })
    }

    pub fn ingest(&self, mut detail: FlowDetail) {
        self.resolve_endpoints(&mut detail);
        let ev = {
            // The id is handed out, and the flow queued for the pages, under
            // the lock: `details` stays in id order (`flow_detail` relies on
            // it), and so do the pages' batches (they drop anything at or
            // below the newest id they have as a repeat).
            let mut d = self.details.lock().unwrap();
            detail.event.id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let ev = detail.event.clone();
            if d.len() >= self.history {
                d.pop_front();
            }
            d.push_back(detail);
            let mut p = self.pending.lock().unwrap();
            if p.len() < MAX_FLOWS_PER_FLUSH * 4 {
                p.push(ev.clone());
            }
            ev
        };
        if now_ms().saturating_sub(ev.time_ms) <= MAX_FLOW_AGE_FOR_RATES_MS {
            self.flows.lock().unwrap().record(&ev);
        }
    }

    /// Names the pods Hubble left out of a flow (see `EndpointIndex::resolve`),
    /// so they show up in their namespace, not as "pod not known"; and the
    /// nodes behind Cilium's node entities (see `NodeIndex::resolve`).
    fn resolve_endpoints(&self, d: &mut FlowDetail) {
        if is_unresolved(&d.source.id) || is_unresolved(&d.destination.id) {
            let ix = self.endpoint_index.read().unwrap();
            if ix.resolve(&mut d.source) {
                d.event.source = d.source.id.clone();
                d.event.src_workload = d.source.workload.clone();
                d.event.src_identity = d.source.identity;
            }
            if ix.resolve(&mut d.destination) {
                d.event.destination = d.destination.id.clone();
                d.event.dst_workload = d.destination.workload.clone();
                d.event.dst_identity = d.destination.identity;
            }
        }
        if nodes::is_node_entity(&d.source.id) || nodes::is_node_entity(&d.destination.id) {
            let ix = self.node_index.read().unwrap();
            if ix.resolve(&mut d.source, &d.event.node) {
                d.event.source = d.source.id.clone();
            }
            if ix.resolve(&mut d.destination, &d.event.node) {
                d.event.destination = d.destination.id.clone();
            }
        }
    }

    /// Full record of a recent flow; ids are sequential, so this is an index.
    pub fn flow_detail(&self, id: u64) -> Option<FlowDetail> {
        let d = self.details.lock().unwrap();
        let first = d.front()?.event.id;
        let idx = id.checked_sub(first)? as usize;
        d.get(idx).filter(|f| f.event.id == id).cloned()
    }

    /// Time of the newest kept flow and the UUIDs of all kept flows, so a
    /// reconnect to hubble-relay can resume without repeating any.
    pub fn seen_flows(&self) -> (Option<i64>, HashSet<String>) {
        let d = self.details.lock().unwrap();
        let newest = d.iter().map(|f| f.event.time_ms).max();
        let uuids = d.iter().filter(|f| !f.uuid.is_empty()).map(|f| f.uuid.clone()).collect();
        (newest, uuids)
    }

    /// The newest flows, oldest first: at most `limit` (and at most what the
    /// server keeps), and with `after`, only those with a higher id.
    pub fn recent_flows(&self, limit: usize, after: Option<u64>) -> Vec<FlowEvent> {
        let d = self.details.lock().unwrap();
        let mut v: Vec<FlowEvent> = d
            .iter()
            .rev()
            .take_while(|f| after.is_none_or(|a| f.event.id > a))
            .take(limit)
            .map(|f| f.event.clone())
            .collect();
        v.reverse();
        v
    }

    pub fn update_status(&self, f: impl FnOnce(&mut Status)) {
        let msg = {
            let mut s = self.status.write().unwrap();
            let before = s.clone();
            f(&mut s);
            if *s == before {
                return;
            }
            ServerMsg::Status(s.clone())
        };
        let _ = self.tx.send(encode(&msg));
    }

    pub fn status_msg(&self) -> Arc<str> {
        encode(&ServerMsg::Status(self.status.read().unwrap().clone()))
    }

    pub fn latest_graph_msg(&self) -> Arc<str> {
        self.latest_graph.read().unwrap().clone()
    }

    /// The flows a page gets when it connects (see `recent_flows`). A page
    /// that reconnects to the same run passes the newest id it has as
    /// `after`, and gets only the flows it missed.
    pub fn snapshot_msg(&self, limit: usize, after: Option<u64>) -> Arc<str> {
        encode(&ServerMsg::Flows(self.recent_flows(limit, after)))
    }

    /// Pods from the inventory plus every endpoint seen in flows. Read-only:
    /// rates only advance in `publish_loop`, however often this is called.
    pub fn build_graph(&self) -> Graph {
        let (mut edges, workloads) = {
            let f = self.flows.lock().unwrap();
            (f.edges(), f.workloads.clone())
        };
        let mut nodes = self.inventory.read().unwrap().nodes.clone();
        let have_pods = !nodes.is_empty();
        nodes.extend(self.node_index.read().unwrap().map_nodes());
        let mut known: HashSet<String> = nodes.iter().map(|n| n.id.clone()).collect();
        // Pods that are gone from Kubernetes disappear once their traffic stops,
        // instead of lingering for the full edge TTL.
        if have_pods {
            let gone = |id: &str| id.contains('/') && !known.contains(id);
            edges.retain(|e| e.idle_secs < STALE_POD_SECS || !(gone(&e.source) || gone(&e.target)));
        }
        for e in &edges {
            for id in [&e.source, &e.target] {
                if known.insert(id.clone()) {
                    nodes.push(endpoint_node(id, workloads.get(id)));
                }
            }
        }
        Graph { nodes, edges, generated_at_ms: now_ms() as u64 }
    }

    /// Runs forever: flushes flows every 500 ms and the graph every 2 s.
    pub async fn publish_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        let mut n: u64 = 0;
        loop {
            tick.tick().await;
            n += 1;
            let batch = {
                let mut p = self.pending.lock().unwrap();
                let mut b = std::mem::take(&mut *p);
                if b.len() > MAX_FLOWS_PER_FLUSH {
                    // Keep the newest; the table is a live sample, not a log.
                    b.drain(..b.len() - MAX_FLOWS_PER_FLUSH);
                }
                b
            };
            if !batch.is_empty() {
                let _ = self.tx.send(encode(&ServerMsg::Flows(batch)));
            }
            if n.is_multiple_of(4) {
                self.flows.lock().unwrap().tick();
                let msg = encode(&ServerMsg::Graph(self.build_graph()));
                *self.latest_graph.write().unwrap() = msg.clone();
                let _ = self.tx.send(msg);
            }
        }
    }
}

/// Node for an endpoint that only appears in flows (reserved identities,
/// pods the inventory has not seen, or when Kubernetes is unavailable).
fn endpoint_node(id: &str, workload: Option<&String>) -> Node {
    if let Some(name) = node_of(id) {
        // Named after the node that saw the flow, without a CiliumNode.
        return nodes::map_node(name, None);
    }
    if let Some((ns, pod)) = id.split_once('/') {
        Node {
            id: id.into(),
            kind: NodeKind::Pod,
            namespace: ns.into(),
            name: pod.into(),
            workload: workload.cloned().unwrap_or_else(|| guess_workload(pod)),
            node_name: String::new(),
            ip: String::new(),
            phase: "Unknown".into(),
        }
    } else {
        let name = id.strip_prefix("reserved:").unwrap_or(id).to_string();
        Node {
            id: id.into(),
            kind: NodeKind::External,
            namespace: entity_group(id).into(),
            workload: name.clone(),
            name,
            node_name: String::new(),
            ip: String::new(),
            phase: String::new(),
        }
    }
}

/// Best guess from a pod name alone:
/// `api-7d9f8b6c5d-x2k4q` -> `Workload/api`, `db-0` -> `Workload/db`.
pub fn guess_workload(pod: &str) -> String {
    let parts: Vec<&str> = pod.split('-').collect();
    // Kubernetes name suffixes use an alphabet without vowels, so words such
    // as "service" are not mistaken for hashes.
    const ALPHABET: &str = "bcdfghjklmnpqrstvwxz2456789";
    let is_hash = |s: &str| (5..=10).contains(&s.len()) && s.chars().all(|c| ALPHABET.contains(c));
    let base = if parts.len() >= 3 && is_hash(parts[parts.len() - 1]) && is_hash(parts[parts.len() - 2]) {
        parts[..parts.len() - 2].join("-")
    } else if parts.len() >= 2 && (is_hash(parts[parts.len() - 1]) || parts[parts.len() - 1].parse::<u32>().is_ok()) {
        parts[..parts.len() - 1].join("-")
    } else {
        pod.to_string()
    };
    format!("Workload/{base}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ingest_n(s: &AppState, n: usize) {
        for i in 0..n {
            let mut d = FlowDetail::default();
            d.event.source = format!("ns/p{i}");
            d.event.destination = "ns/x".into();
            s.ingest(d);
        }
    }

    #[test]
    fn details_by_id() {
        let s = AppState::new(true, 200);
        ingest_n(&s, 210);
        assert!(s.flow_detail(10).is_none(), "evicted");
        assert_eq!(s.flow_detail(11).unwrap().event.source, "ns/p10");
        assert_eq!(s.flow_detail(210).unwrap().event.source, "ns/p209");
        assert!(s.flow_detail(211).is_none());
    }

    #[test]
    fn snapshots() {
        let s = AppState::new(true, 200);
        ingest_n(&s, 210);
        let ids = |v: Vec<FlowEvent>| v.iter().map(|f| f.id).collect::<Vec<_>>();
        // The newest, oldest first, never more than the server keeps.
        assert_eq!(ids(s.recent_flows(3, None)), vec![208, 209, 210]);
        assert_eq!(s.recent_flows(10_000, None).len(), 200);
        // A page that has everything up to 205 only gets what came after.
        assert_eq!(ids(s.recent_flows(1000, Some(205))), vec![206, 207, 208, 209, 210]);
        assert_eq!(ids(s.recent_flows(2, Some(205))), vec![209, 210]);
        assert!(s.recent_flows(1000, Some(210)).is_empty());
        // Each run has its own instance, and pages see it in the status.
        assert_eq!(s.status.read().unwrap().instance, s.instance);
        assert_eq!(s.info.flow_history, 200);
    }

    #[test]
    fn names_pods_hubble_left_out() {
        let s = AppState::new(true, 200);
        let helm = common::EndpointInfo {
            id: "flux-system/helm-controller-5d8f-x2k4q".into(),
            namespace: "flux-system".into(),
            identity: 65478,
            ipv4: "10.244.0.245".into(),
            workload: "Deployment/helm-controller".into(),
            ..Default::default()
        };
        *s.endpoint_index.write().unwrap() = EndpointIndex::new(&[helm]);
        let flow = |src: &str, ip: &str| FlowDetail {
            source: common::EndpointDetail {
                id: src.into(),
                namespace: "flux-system".into(),
                identity: 65478,
                ip: ip.into(),
                ..Default::default()
            },
            destination: common::EndpointDetail { id: "reserved:kube-apiserver".into(), ..Default::default() },
            event: FlowEvent {
                source: src.into(),
                destination: "reserved:kube-apiserver".into(),
                time_ms: now_ms(),
                ..Default::default()
            },
            ..Default::default()
        };
        // Its IP and identity name the pod.
        s.ingest(flow("flux-system/identity:65478", "10.244.0.245"));
        // An IP the index doesn't know: at least the workload.
        s.ingest(flow("flux-system/identity:65478", "10.244.0.99"));
        let flows = s.recent_flows(10, None);
        assert_eq!(flows[0].source, "flux-system/helm-controller-5d8f-x2k4q");
        assert_eq!(flows[0].src_workload, "Deployment/helm-controller");
        assert_eq!(s.flow_detail(flows[0].id).unwrap().source.pod, "helm-controller-5d8f-x2k4q");
        assert_eq!(flows[1].source, "flux-system/identity:65478");
        assert_eq!(flows[1].src_workload, "Deployment/helm-controller");
        // On the map they're pods of the workload, not "pod not known".
        s.flows.lock().unwrap().tick();
        let g = s.build_graph();
        for id in ["flux-system/helm-controller-5d8f-x2k4q", "flux-system/identity:65478"] {
            let n = g.nodes.iter().find(|n| n.id == id).unwrap();
            assert_eq!((n.kind, n.workload.as_str()), (NodeKind::Pod, "Deployment/helm-controller"));
        }
    }

    #[test]
    fn names_nodes_behind_cilium_node_entities() {
        let s = AppState::new(true, 200);
        let node = |name: &str, ip: &str, control_plane: bool| nodes::NodeInfo {
            name: name.into(),
            ips: vec![ip.parse().unwrap()],
            ip: ip.into(),
            control_plane,
        };
        *s.node_index.write().unwrap() =
            NodeIndex::new(vec![node("cp-1", "192.168.10.10", true), node("worker-1", "192.168.10.1", false)]);
        let ep = |id: &str, identity: u32, ip: &str| common::EndpointDetail {
            id: id.into(),
            identity,
            ip: ip.into(),
            ..Default::default()
        };
        let flow = |src: common::EndpointDetail, dst: common::EndpointDetail| FlowDetail {
            event: FlowEvent {
                node: "default/worker-2".into(),
                source: src.id.clone(),
                destination: dst.id.clone(),
                time_ms: now_ms(),
                ..Default::default()
            },
            source: src,
            destination: dst,
            ..Default::default()
        };
        let pod = || ep("monitoring/vmagent-x2k4q", 40000, "10.0.2.5");
        s.ingest(flow(pod(), ep("reserved:kube-apiserver", 7, "192.168.10.10")));
        // The node that saw it, which has no CiliumNode yet.
        s.ingest(flow(ep("reserved:host", 1, "10.0.2.1"), pod()));
        s.ingest(flow(pod(), ep("reserved:remote-node", 6, "192.168.20.1")));
        let flows = s.recent_flows(10, None);
        let ids: Vec<_> = flows.iter().map(|f| (f.source.as_str(), f.destination.as_str())).collect();
        assert_eq!(
            ids,
            [
                ("monitoring/vmagent-x2k4q", "node:cp-1"),
                ("node:worker-2", "monitoring/vmagent-x2k4q"),
                ("monitoring/vmagent-x2k4q", "reserved:remote-node"),
            ]
        );
        // The full record keeps what Hubble said: identity and address.
        let d = s.flow_detail(flows[0].id).unwrap();
        assert_eq!((d.destination.id.as_str(), d.destination.identity), ("node:cp-1", 7));

        s.flows.lock().unwrap().tick();
        let g = s.build_graph();
        let node = |id: &str| g.nodes.iter().find(|n| n.id == id).unwrap();
        assert_eq!(
            (node("node:cp-1").workload.as_str(), node("node:cp-1").ip.as_str()),
            ("control-plane", "192.168.10.10")
        );
        // Known nodes are on the map without traffic too.
        assert_eq!(node("node:worker-1").namespace, common::CLUSTER_GROUP);
        assert_eq!(node("node:worker-2").workload, "node");
        assert_eq!(node("reserved:remote-node").namespace, common::CLUSTER_GROUP);
    }

    #[test]
    fn guesses_workloads() {
        assert_eq!(guess_workload("api-7d9f8b6c5d-x2k4q"), "Workload/api");
        assert_eq!(guess_workload("db-0"), "Workload/db");
        assert_eq!(guess_workload("cilium-x8k2p"), "Workload/cilium");
        assert_eq!(guess_workload("standalone"), "Workload/standalone");
        assert_eq!(guess_workload("my-service"), "Workload/my-service");
    }
}
