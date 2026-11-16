//! Shared in-memory state. Nothing is persisted: the server only keeps the
//! current inventory, edge rates and a short buffer of recent flows.

use crate::endpoints::EndpointObject;
use crate::flows::FlowAggregator;
use common::{
    ConfigView, EndpointsView, FlowDetail, FlowEvent, Graph, Info, Node, NodeKind, PolicyInfo, ServerMsg, Status,
    EXTERNAL_GROUP,
};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

/// Flows kept for clients that connect later.
const RECENT_FLOWS: usize = 1000;
/// Full flow records kept for the details panel (`/api/flows/{id}`).
const DETAILED_FLOWS: usize = 5000;
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
    pub status: RwLock<Status>,
    pub inventory: RwLock<Inventory>,
    pub flows: Mutex<FlowAggregator>,
    recent: Mutex<VecDeque<FlowEvent>>,
    details: Mutex<VecDeque<FlowDetail>>,
    next_id: AtomicU64,
    pending: Mutex<Vec<FlowEvent>>,
    pub policies: RwLock<Vec<PolicyInfo>>,
    pub config: RwLock<ConfigView>,
    pub endpoints: RwLock<EndpointsView>,
    /// The objects behind `endpoints`, by endpoint id, for the YAML view.
    /// They're the watches' cached objects, not copies.
    pub endpoint_objects: RwLock<HashMap<String, EndpointObject>>,
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
    pub fn new(demo: bool) -> Arc<Self> {
        let (tx, _) = broadcast::channel(64);
        Arc::new(Self {
            info: Info {
                version: build_version(),
                commit: build_env("WIREPULSE_COMMIT"),
                demo,
                cluster_name: String::new(),
            },
            status: RwLock::new(Status { demo, ..Default::default() }),
            inventory: RwLock::default(),
            flows: Mutex::default(),
            recent: Mutex::new(VecDeque::with_capacity(RECENT_FLOWS)),
            details: Mutex::new(VecDeque::with_capacity(DETAILED_FLOWS)),
            next_id: AtomicU64::new(1),
            pending: Mutex::default(),
            policies: RwLock::default(),
            config: RwLock::default(),
            endpoints: RwLock::default(),
            endpoint_objects: RwLock::default(),
            latest_graph: RwLock::new(encode(&ServerMsg::Graph(Graph::default()))),
            tx,
        })
    }

    pub fn ingest(&self, mut detail: FlowDetail) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        detail.event.id = id;
        let ev = detail.event.clone();
        {
            let mut d = self.details.lock().unwrap();
            if d.len() == DETAILED_FLOWS {
                d.pop_front();
            }
            d.push_back(detail);
        }
        if now_ms().saturating_sub(ev.time_ms) <= MAX_FLOW_AGE_FOR_RATES_MS {
            self.flows.lock().unwrap().record(&ev);
        }
        {
            let mut r = self.recent.lock().unwrap();
            if r.len() == RECENT_FLOWS {
                r.pop_front();
            }
            r.push_back(ev.clone());
        }
        let mut p = self.pending.lock().unwrap();
        if p.len() < MAX_FLOWS_PER_FLUSH * 4 {
            p.push(ev);
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

    pub fn recent_flows(&self) -> Vec<FlowEvent> {
        self.recent.lock().unwrap().iter().cloned().collect()
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

    pub fn recent_flows_msg(&self) -> Arc<str> {
        encode(&ServerMsg::Flows(self.recent_flows()))
    }

    /// Pods from the inventory plus every endpoint seen in flows. Read-only:
    /// rates only advance in `publish_loop`, however often this is called.
    pub fn build_graph(&self) -> Graph {
        let (mut edges, workloads) = {
            let f = self.flows.lock().unwrap();
            (f.edges(), f.workloads.clone())
        };
        let mut nodes = self.inventory.read().unwrap().nodes.clone();
        let mut known: HashSet<String> = nodes.iter().map(|n| n.id.clone()).collect();
        // Pods that are gone from Kubernetes disappear once their traffic stops,
        // instead of lingering for the full edge TTL.
        if !nodes.is_empty() {
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
            namespace: EXTERNAL_GROUP.into(),
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

    #[test]
    fn details_by_id() {
        let s = AppState::new(true);
        for i in 0..(DETAILED_FLOWS + 10) {
            let mut d = FlowDetail::default();
            d.event.source = format!("ns/p{i}");
            d.event.destination = "ns/x".into();
            s.ingest(d);
        }
        assert!(s.flow_detail(1).is_none(), "evicted");
        let last = (DETAILED_FLOWS + 10) as u64;
        assert_eq!(s.flow_detail(last).unwrap().event.source, format!("ns/p{}", last - 1));
        assert!(s.flow_detail(last + 1).is_none());
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
