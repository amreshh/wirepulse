use serde::{Deserialize, Serialize};

/// Pseudo-namespace used to group endpoints that are not pods
/// (world, host, remote-node, kube-apiserver, ...).
pub const EXTERNAL_GROUP: &str = "(external)";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeKind {
    Pod,
    /// Cilium reserved identity or anything outside the cluster.
    External,
}

/// One endpoint on the map: a pod, or a reserved entity such as `world`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    /// `namespace/pod` for pods, `reserved:<entity>` for reserved identities.
    pub id: String,
    pub kind: NodeKind,
    /// Kubernetes namespace; [`EXTERNAL_GROUP`] for external nodes.
    pub namespace: String,
    pub name: String,
    /// Owning workload, e.g. `Deployment/frontend`. Falls back to the pod name.
    pub workload: String,
    #[serde(default)]
    pub node_name: String,
    #[serde(default)]
    pub ip: String,
    #[serde(default)]
    pub phase: String,
}

/// Directed traffic between two nodes, aggregated from Hubble flows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub source: String,
    pub target: String,
    /// Smoothed rate of forwarded flows per second.
    pub flows_per_sec: f32,
    /// Smoothed rate of dropped flows per second.
    pub drops_per_sec: f32,
    pub total_flows: u64,
    pub total_drops: u64,
    /// Up to a handful of destination ports, e.g. `TCP/8080`.
    pub ports: Vec<String>,
    /// Seconds since the last flow on this edge.
    pub idle_secs: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub generated_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Verdict {
    Forwarded,
    Dropped,
    Error,
    Audit,
    Redirected,
    Traced,
    Translated,
    #[default]
    Unknown,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Forwarded => "forwarded",
            Verdict::Dropped => "dropped",
            Verdict::Error => "error",
            Verdict::Audit => "audit",
            Verdict::Redirected => "redirected",
            Verdict::Traced => "traced",
            Verdict::Translated => "translated",
            Verdict::Unknown => "unknown",
        }
    }
}

/// A single Hubble flow, flattened for the flows table. The full record
/// ([`FlowDetail`]) is fetched on demand from `/api/flows/{id}`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FlowEvent {
    /// Backend sequence number; key for `/api/flows/{id}`.
    #[serde(default)]
    pub id: u64,
    pub time_ms: i64,
    /// Kubernetes node that observed the flow.
    pub node: String,
    pub source: String,
    pub destination: String,
    /// Workload Hubble reports for the endpoint, e.g. `Deployment/api`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub src_workload: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dst_workload: String,
    /// `TCP`, `UDP`, `ICMPv4`, ...
    pub protocol: String,
    pub dst_port: u32,
    pub verdict: Verdict,
    #[serde(default)]
    pub drop_reason: String,
    /// `ingress` / `egress` / empty.
    #[serde(default)]
    pub direction: String,
    /// Short L7 summary, e.g. `HTTP GET /api 200` or `DNS example.com`.
    #[serde(default)]
    pub l7: String,
    /// Policies that allowed or denied this flow, when Cilium reports them.
    #[serde(default)]
    pub policies: Vec<String>,
    #[serde(default)]
    pub is_reply: bool,
    /// DNS name of the destination when Cilium knows it (FQDN policies).
    #[serde(default)]
    pub dst_dns: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub src_ip: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dst_ip: String,
    #[serde(default)]
    pub src_port: u32,
    /// Cilium security identities.
    #[serde(default)]
    pub src_identity: u32,
    #[serde(default)]
    pub dst_identity: u32,
    /// e.g. `SYN ACK`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tcp_flags: String,
    /// L7 response latency in milliseconds (0 when not known).
    #[serde(default)]
    pub latency_ms: f32,
    /// Where in the datapath Hubble saw the flow, e.g. `to-endpoint`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub trace_point: String,
}

/// One side of a flow, with everything Hubble reports about it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EndpointDetail {
    pub id: String,
    pub namespace: String,
    pub pod: String,
    pub workload: String,
    pub identity: u32,
    pub ip: String,
    pub port: u32,
    pub labels: Vec<String>,
    /// Kubernetes service, `namespace/name`.
    pub service: String,
    /// DNS names Cilium associated with the IP.
    pub names: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct L7Detail {
    /// `HTTP`, `DNS` or `Kafka`.
    pub kind: String,
    /// `request`, `response` or `sample`.
    pub flow_type: String,
    pub latency_ms: f64,
    pub http_method: String,
    pub http_url: String,
    pub http_code: u32,
    pub http_protocol: String,
    pub dns_query: String,
    pub dns_ips: Vec<String>,
    pub dns_rcode: u32,
    pub dns_qtypes: Vec<String>,
    pub dns_cnames: Vec<String>,
    pub dns_ttl: u32,
}

/// Full detail of one flow, for the flow details panel.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FlowDetail {
    pub event: FlowEvent,
    pub uuid: String,
    pub source: EndpointDetail,
    pub destination: EndpointDetail,
    pub ip_version: String,
    pub encrypted: bool,
    pub trace_reason: String,
    pub interface: String,
    pub l7: Option<L7Detail>,
    pub allowed_by: Vec<String>,
    pub denied_by: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub demo: bool,
    pub hubble_connected: bool,
    pub k8s_connected: bool,
    #[serde(default)]
    pub message: String,
}

/// Everything the backend pushes over the WebSocket.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ServerMsg {
    Graph(Graph),
    Flows(Vec<FlowEvent>),
    Status(Status),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PolicyKind {
    CiliumNetworkPolicy,
    CiliumClusterwideNetworkPolicy,
    NetworkPolicy,
}

impl PolicyKind {
    pub fn short(self) -> &'static str {
        match self {
            PolicyKind::CiliumNetworkPolicy => "CNP",
            PolicyKind::CiliumClusterwideNetworkPolicy => "CCNP",
            PolicyKind::NetworkPolicy => "NetPol",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolicyInfo {
    pub kind: PolicyKind,
    /// `None` for cluster-wide policies.
    pub namespace: Option<String>,
    pub name: String,
    pub ingress_rules: u32,
    pub egress_rules: u32,
    pub ingress_deny_rules: u32,
    pub egress_deny_rules: u32,
    /// Human-readable endpoint selector, e.g. `app=frontend, tier in (web)`.
    pub selector: String,
    /// Node ids (`namespace/pod`) the policy currently selects.
    pub selected: Vec<String>,
    /// Policy selects nodes (host policy) rather than pods.
    #[serde(default)]
    pub host_policy: bool,
    /// From `status.conditions[type=Valid]` when Cilium reports it.
    #[serde(default)]
    pub valid: Option<bool>,
    #[serde(default)]
    pub created: String,
    /// Full object as YAML, without managedFields.
    pub yaml: String,
}

impl PolicyInfo {
    pub fn key(&self) -> String {
        format!("{}/{}/{}", self.kind.short(), self.namespace.as_deref().unwrap_or("-"), self.name)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConfigEntry {
    pub key: String,
    pub value: String,
    pub category: String,
}

/// A `CiliumNodeConfig` that overrides agent config on selected nodes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeConfigOverride {
    pub namespace: String,
    pub name: String,
    pub node_selector: String,
    pub entries: Vec<ConfigEntry>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigView {
    /// Where the base config came from, e.g. `kube-system/cilium-config`.
    pub source: String,
    pub entries: Vec<ConfigEntry>,
    pub overrides: Vec<NodeConfigOverride>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Info {
    pub version: String,
    pub demo: bool,
    pub cluster_name: String,
}

/// Groups cilium-config keys so the config view can be scanned quickly.
pub fn config_category(key: &str) -> &'static str {
    let k = key.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| k.contains(n));
    if has(&["hubble"]) {
        "Hubble"
    } else if has(&["encrypt", "wireguard", "ipsec"]) {
        "Encryption"
    } else if has(&["policy", "identity", "l7-proxy", "envoy", "fqdn", "tofqdns"]) {
        "Policy"
    } else if has(&["ipam", "cidr", "pool", "ipv4-native", "ipv6-native"]) {
        "IPAM"
    } else if has(&[
        "kube-proxy", "nodeport", "loadbalancer", "lb-", "bgp", "service", "maglev", "dsr", "gateway-api", "ingress",
        "l2-announce",
    ]) {
        "Load balancing"
    } else if has(&[
        "routing",
        "tunnel",
        "vxlan",
        "geneve",
        "masquerade",
        "mtu",
        "devices",
        "endpoint-routes",
        "auto-direct",
        "bandwidth",
        "ipv4",
        "ipv6",
        "cluster-id",
        "cluster-name",
        "clustermesh",
    ]) {
        "Networking"
    } else if has(&["bpf", "ct-", "nat-", "map"]) {
        "BPF"
    } else if has(&["prometheus", "metrics", "debug", "log", "monitor", "pprof"]) {
        "Observability"
    } else {
        "Other"
    }
}

/// One Cilium endpoint (from a `CiliumEndpoint`, or a `CiliumEndpointSlice`
/// entry when endpoint slices are enabled).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EndpointInfo {
    /// `namespace/pod`, the same id the map uses.
    pub id: String,
    pub namespace: String,
    pub name: String,
    /// Cilium's numeric endpoint id on its node (0 when unknown).
    pub endpoint_id: u64,
    /// Security identity.
    pub identity: u64,
    /// Identity labels, e.g. `k8s:app=web`.
    pub labels: Vec<String>,
    pub ipv4: String,
    pub ipv6: String,
    /// Node the endpoint runs on (Cilium reports the node IP).
    pub node: String,
    /// `ready`, `waiting-for-identity`, `regenerating`, ...
    pub state: String,
    pub ingress_enforcing: Option<bool>,
    pub egress_enforcing: Option<bool>,
    pub service_account: String,
    /// `name/port/protocol`.
    pub named_ports: Vec<String>,
    /// Owning workload from the pod inventory, e.g. `Deployment/web`.
    pub workload: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EndpointsView {
    /// `CiliumEndpoint`, `CiliumEndpointSlice`, `demo` or empty.
    pub source: String,
    pub endpoints: Vec<EndpointInfo>,
    /// Labels for pods without a Cilium endpoint (e.g. host-network pods),
    /// so label filters still work for them.
    pub pod_labels: std::collections::BTreeMap<String, Vec<String>>,
}
