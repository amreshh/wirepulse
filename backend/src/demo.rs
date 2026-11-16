//! `--demo`: a synthetic cluster, so the UI can be tried without Cilium.
//!
//! Demo data goes through the same code paths as real data: generated flows
//! are ingested like Hubble flows, and demo policies and config are plain
//! Kubernetes JSON objects parsed by `policy::parse_all` and `config::build_view`.

use crate::state::{now_ms, AppState, Inventory};
use crate::{config, endpoints, policy};
use common::{
    EndpointDetail, FlowDetail, FlowEvent, L7Detail, Node, NodeConfigOverride, NodeKind, PolicyKind, Verdict,
};
use kube::api::DynamicObject;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

/// (namespace, kind, name, replicas)
const WORKLOADS: &[(&str, &str, &str, usize)] = &[
    ("frontend", "Deployment", "web", 3),
    ("frontend", "Deployment", "cdn-cache", 2),
    ("shop", "Deployment", "cart", 2),
    ("shop", "Deployment", "catalog", 3),
    ("shop", "Deployment", "checkout", 2),
    ("shop", "StatefulSet", "redis", 1),
    ("payments", "Deployment", "payments-api", 2),
    ("payments", "Deployment", "fraud-scoring", 1),
    ("payments", "StatefulSet", "postgres", 1),
    ("auth", "Deployment", "keycloak", 2),
    ("monitoring", "Deployment", "vmagent", 1),
    ("monitoring", "Deployment", "grafana", 1),
    ("kube-system", "Deployment", "coredns", 2),
    ("kube-system", "Deployment", "hubble-relay", 1),
    ("monitoring", "DaemonSet", "node-exporter", 3),
    ("shop", "CronJob", "catalog-sync", 1),
    ("payments", "Job", "db-migrate", 1),
    ("auth", "Pod", "debug-shell", 1),
];

struct Pattern {
    src: &'static str,
    dst: &'static str,
    proto: &'static str,
    port: u32,
    /// Flows per second.
    rate: f64,
    dropped: bool,
    l7: &'static str,
    dns: &'static str,
    policy: &'static str,
}

const fn p(src: &'static str, dst: &'static str, port: u32, rate: f64, l7: &'static str) -> Pattern {
    Pattern { src, dst, proto: "TCP", port, rate, dropped: false, l7, dns: "", policy: "" }
}

const PATTERNS: &[Pattern] = &[
    p("reserved:world", "frontend/web", 443, 18.0, ""),
    p("frontend/web", "frontend/cdn-cache", 8080, 9.0, "HTTP GET /assets/app.js 200"),
    p("frontend/web", "shop/catalog", 8080, 7.0, "HTTP GET /api/products 200"),
    p("frontend/web", "shop/cart", 8080, 4.0, "HTTP POST /api/cart 201"),
    p("frontend/web", "shop/checkout", 8080, 1.5, "HTTP POST /api/checkout 200"),
    p("frontend/web", "auth/keycloak", 8443, 2.5, ""),
    p("shop/cart", "shop/redis", 6379, 6.0, ""),
    p("shop/catalog", "shop/redis", 6379, 5.0, ""),
    p("shop/checkout", "payments/payments-api", 8443, 1.5, ""),
    p("payments/payments-api", "payments/postgres", 5432, 3.0, ""),
    p("payments/payments-api", "payments/fraud-scoring", 9000, 1.2, "HTTP POST /score 200"),
    Pattern {
        dns: "api.stripe.com",
        policy: "payments/payments-egress",
        ..p("payments/payments-api", "reserved:world", 443, 0.8, "")
    },
    Pattern {
        dropped: true,
        dns: "models.example.com",
        policy: "deny payments/fraud-scoring-no-egress",
        ..p("payments/fraud-scoring", "reserved:world", 443, 0.4, "")
    },
    Pattern { dropped: true, policy: "", ..p("frontend/web", "payments/postgres", 5432, 0.3, "") },
    p("monitoring/vmagent", "shop/catalog", 9090, 0.3, "HTTP GET /metrics 200"),
    p("monitoring/vmagent", "payments/payments-api", 9090, 0.3, "HTTP GET /metrics 200"),
    p("monitoring/vmagent", "frontend/web", 9090, 0.3, "HTTP GET /metrics 200"),
    p("monitoring/grafana", "auth/keycloak", 8443, 0.2, ""),
    p("kube-system/hubble-relay", "reserved:host", 4244, 1.0, ""),
    p("monitoring/vmagent", "monitoring/node-exporter", 9100, 0.6, "HTTP GET /metrics 200"),
    p("shop/catalog-sync", "shop/catalog", 8080, 0.4, "HTTP PUT /api/products/bulk 204"),
    Pattern { dns: "feeds.example.com", ..p("shop/catalog-sync", "reserved:world", 443, 0.2, "") },
    p("payments/db-migrate", "payments/postgres", 5432, 0.3, ""),
    p("kube-system/coredns", "reserved:kube-apiserver", 6443, 0.3, ""),
];

/// Every workload also resolves DNS through CoreDNS.
const DNS_RATE: f64 = 0.4;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[(self.next() % v.len() as u64) as usize]
    }
    fn suffix(&mut self, len: usize) -> String {
        const A: &[u8] = b"bcdfghjklmnpqrstvwxz2456789";
        (0..len).map(|_| *self.pick(A) as char).collect()
    }
}

struct Cluster {
    /// workload key `ns/name` -> pod nodes
    pods: BTreeMap<String, Vec<Node>>,
}

impl Cluster {
    fn new(rng: &mut Rng) -> Self {
        let mut pods = BTreeMap::new();
        for (i, (ns, kind, name, replicas)) in WORKLOADS.iter().enumerate() {
            let hash = rng.suffix(10);
            let list = (0..*replicas)
                .map(|r| {
                    let pod = match *kind {
                        "StatefulSet" => format!("{name}-{r}"),
                        "Deployment" => format!("{name}-{hash}-{}", rng.suffix(5)),
                        "CronJob" => format!("{name}-29384{}-{}", 100 + i, rng.suffix(5)),
                        "Pod" => name.to_string(),
                        _ => format!("{name}-{}", rng.suffix(5)),
                    };
                    Node {
                        id: format!("{ns}/{pod}"),
                        kind: NodeKind::Pod,
                        namespace: ns.to_string(),
                        name: pod,
                        workload: format!("{kind}/{name}"),
                        node_name: format!("worker-{}", (i + r) % 3 + 1),
                        ip: format!("10.0.{}.{}", i + 1, r + 10),
                        phase: "Running".into(),
                    }
                })
                .collect();
            pods.insert(format!("{ns}/{name}"), list);
        }
        Self { pods }
    }

    /// Simulates a rolling restart of one replica.
    fn churn(&mut self, rng: &mut Rng) {
        let keys: Vec<String> = self
            .pods
            .iter()
            .filter(|(_, v)| v[0].workload.starts_with("Deployment/"))
            .map(|(k, _)| k.clone())
            .collect();
        let key = rng.pick(&keys).clone();
        let list = self.pods.get_mut(&key).unwrap();
        let i = (rng.next() % list.len() as u64) as usize;
        let old = &list[i];
        let base = old.name.rsplit_once('-').map(|(b, _)| b).unwrap_or(&old.name);
        let name = format!("{base}-{}", rng.suffix(5));
        list[i] = Node { id: format!("{}/{name}", old.namespace), name, ..old.clone() };
    }

    fn inventory(&self) -> Inventory {
        let mut inv = Inventory::default();
        for (key, pods) in &self.pods {
            let (ns, app) = key.split_once('/').unwrap();
            for pod in pods {
                inv.labels.insert(
                    pod.id.clone(),
                    [
                        ("app".to_string(), app.to_string()),
                        ("io.kubernetes.pod.namespace".to_string(), ns.to_string()),
                        ("io.cilium.k8s.policy.serviceaccount".to_string(), app.to_string()),
                    ]
                    .into(),
                );
                inv.nodes.push(pod.clone());
            }
        }
        inv.nodes.sort_by(|a, b| a.id.cmp(&b.id));
        inv
    }

    fn endpoint(&self, key: &str, rng: &mut Rng) -> EndpointDetail {
        match self.pods.get(key) {
            Some(pods) => {
                let p = rng.pick(pods);
                let app = key.split_once('/').map_or(key, |(_, a)| a);
                EndpointDetail {
                    id: p.id.clone(),
                    namespace: p.namespace.clone(),
                    pod: p.name.clone(),
                    workload: p.workload.clone(),
                    identity: identity(key),
                    ip: p.ip.clone(),
                    port: 0,
                    labels: vec![
                        format!("k8s:app={app}"),
                        format!("k8s:io.kubernetes.pod.namespace={}", p.namespace),
                        format!("k8s:io.cilium.k8s.policy.serviceaccount={app}"),
                        "k8s:io.cilium.k8s.policy.cluster=demo".into(),
                    ],
                    service: format!("{}/{app}", p.namespace),
                    names: vec![],
                }
            }
            None => {
                let entity = key.strip_prefix("reserved:").unwrap_or(key);
                let (identity, ip) = match entity {
                    "host" => (1, "10.0.0.11".to_string()),
                    "kube-apiserver" => (7, "10.0.0.1".to_string()),
                    _ => (2, format!("203.0.113.{}", 10 + rng.next() % 200)),
                };
                EndpointDetail {
                    id: key.to_string(),
                    identity,
                    ip,
                    labels: vec![format!("reserved:{entity}")],
                    ..Default::default()
                }
            }
        }
    }
}

/// Stable fake security identity per workload.
fn identity(key: &str) -> u32 {
    10_000 + key.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b))) % 50_000
}

/// Parses demo L7 strings such as `HTTP GET /x 200` into structured detail.
fn l7_detail(l7: &str, rng: &mut Rng) -> Option<L7Detail> {
    let mut parts = l7.split_whitespace();
    match parts.next()? {
        "HTTP" => Some(L7Detail {
            kind: "HTTP".into(),
            flow_type: "response".into(),
            latency_ms: 2.0 + rng.f() * 60.0,
            http_method: parts.next().unwrap_or_default().into(),
            http_url: parts.next().unwrap_or_default().into(),
            http_code: parts.next().and_then(|c| c.parse().ok()).unwrap_or(0),
            http_protocol: "HTTP/1.1".into(),
            ..Default::default()
        }),
        "DNS" => {
            let q = parts.next().unwrap_or_default().to_string();
            Some(L7Detail {
                kind: "DNS".into(),
                flow_type: "response".into(),
                latency_ms: 0.2 + rng.f() * 3.0,
                dns_ips: vec![format!("10.96.{}.{}", rng.next() % 200, 1 + rng.next() % 250)],
                dns_qtypes: vec!["A".into()],
                dns_ttl: 30,
                dns_query: q,
                ..Default::default()
            })
        }
        _ => None,
    }
}

fn event(cluster: &Cluster, rng: &mut Rng, pat: &Pattern, dns_query: &str) -> Vec<FlowDetail> {
    let mut src = cluster.endpoint(pat.src, rng);
    let mut dst = cluster.endpoint(pat.dst, rng);
    src.port = 32768 + (rng.next() % 28000) as u32;
    dst.port = pat.port;
    if !pat.dns.is_empty() {
        dst.names = vec![pat.dns.to_string()];
    }
    let node = cluster
        .pods
        .get(pat.src)
        .or_else(|| cluster.pods.get(pat.dst))
        .map(|p| p[0].node_name.clone())
        .unwrap_or_default();
    let l7_text = if pat.l7 == "DNS" { format!("DNS {dns_query}") } else { pat.l7.to_string() };
    let l7 = l7_detail(&l7_text, rng);
    let tcp = pat.proto == "TCP";
    let flags = if !tcp {
        ""
    } else if pat.dropped {
        "SYN"
    } else {
        *rng.pick(&["SYN", "ACK PSH", "ACK", "ACK PSH", "ACK FIN"])
    };
    let to_world = pat.dst.starts_with("reserved:");
    let event = FlowEvent {
        id: 0,
        time_ms: now_ms(),
        node,
        source: src.id.clone(),
        destination: dst.id.clone(),
        src_workload: src.workload.clone(),
        dst_workload: dst.workload.clone(),
        protocol: pat.proto.into(),
        dst_port: pat.port,
        verdict: if pat.dropped { Verdict::Dropped } else { Verdict::Forwarded },
        drop_reason: if pat.dropped { "POLICY_DENIED".into() } else { String::new() },
        direction: if to_world { "egress" } else { "ingress" }.into(),
        l7: if pat.l7 == "DNS" {
            l7.as_ref().map(|d| format!("DNS {} → {}", d.dns_query, d.dns_ips.join(", "))).unwrap_or_default()
        } else {
            l7_text.clone()
        },
        policies: if pat.policy.is_empty() { vec![] } else { vec![pat.policy.into()] },
        is_reply: false,
        dst_dns: pat.dns.into(),
        src_ip: src.ip.clone(),
        dst_ip: dst.ip.clone(),
        src_port: src.port,
        src_identity: src.identity,
        dst_identity: dst.identity,
        tcp_flags: flags.into(),
        latency_ms: l7.as_ref().map_or(0.0, |d| d.latency_ms as f32),
        trace_point: if pat.dropped {
            String::new()
        } else if to_world {
            "to-stack".into()
        } else {
            "to-endpoint".into()
        },
    };
    let (allowed_by, denied_by) = match pat.policy.strip_prefix("deny ") {
        Some(p) => (vec![], vec![p.to_string()]),
        None if pat.policy.is_empty() => (vec![], vec![]),
        None => (vec![pat.policy.to_string()], vec![]),
    };
    let base = FlowDetail {
        uuid: format!(
            "{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}",
            rng.next() as u32,
            rng.next() as u16,
            rng.next() % 4096,
            rng.next() % 4096,
            rng.next() % (1 << 48)
        ),
        source: src,
        destination: dst,
        ip_version: "IPv4".into(),
        encrypted: !to_world && !pat.src.starts_with("reserved:"),
        trace_reason: if pat.dropped { String::new() } else { "new".into() },
        interface: String::new(),
        l7,
        allowed_by,
        denied_by,
        event,
    };
    if pat.dropped || rng.f() < 0.5 {
        return vec![base];
    }
    let mut reply = base.clone();
    std::mem::swap(&mut reply.source, &mut reply.destination);
    let e = &mut reply.event;
    e.source = reply.source.id.clone();
    e.destination = reply.destination.id.clone();
    e.src_workload = reply.source.workload.clone();
    e.dst_workload = reply.destination.workload.clone();
    e.src_ip = reply.source.ip.clone();
    e.dst_ip = reply.destination.ip.clone();
    e.src_port = reply.source.port;
    e.dst_port = reply.destination.port;
    e.src_identity = reply.source.identity;
    e.dst_identity = reply.destination.identity;
    e.is_reply = true;
    e.l7 = String::new();
    e.latency_ms = 0.0;
    e.dst_dns = String::new();
    e.tcp_flags = if tcp { "ACK".into() } else { String::new() };
    e.direction = if to_world { "ingress" } else { "egress" }.into();
    e.trace_point = "to-endpoint".into();
    reply.l7 = None;
    reply.trace_reason = "reply".into();
    vec![base, reply]
}

fn dns_pattern(src: &'static str) -> Pattern {
    Pattern { proto: "UDP", l7: "DNS", ..p(src, "kube-system/coredns", 53, DNS_RATE, "") }
}

pub async fn run(state: Arc<AppState>) {
    let seed = now_ms() as u64 | 1;
    let mut rng = Rng(seed);
    let mut cluster = Cluster::new(&mut rng);

    let objects = policies();
    let refresh = |state: &AppState, cluster: &Cluster| {
        let inv = cluster.inventory();
        let policies = policy::parse_all(&objects, &inv.labels);
        let eps = demo_endpoints(cluster, &policies);
        *state.policies.write().unwrap() = policies;
        *state.inventory.write().unwrap() = inv;
        endpoints::store(state, "CiliumEndpoint (demo)", eps);
    };
    refresh(&state, &cluster);
    *state.config.write().unwrap() = demo_config();
    state.update_status(|s| {
        s.hubble_connected = true;
        s.k8s_connected = true;
        s.message = "demo mode: synthetic cluster".into();
    });

    // Leak the per-workload DNS patterns once; they live for the whole process.
    let dns: Vec<Pattern> = WORKLOADS
        .iter()
        .filter(|w| w.2 != "coredns")
        .map(|(ns, _, name, _)| {
            let key: &'static str = Box::leak(format!("{ns}/{name}").into_boxed_str());
            dns_pattern(key)
        })
        .collect();
    let dns_names: HashMap<&str, &str> = [
        ("frontend/web", "keycloak.auth.svc.cluster.local"),
        ("payments/payments-api", "api.stripe.com"),
        ("payments/fraud-scoring", "models.example.com"),
    ]
    .into();

    let tick = Duration::from_millis(100);
    let mut interval = tokio::time::interval(tick);
    let mut n: u64 = 0;
    loop {
        interval.tick().await;
        n += 1;
        for pat in PATTERNS.iter().chain(dns.iter()) {
            // Expected count this tick, rounded randomly; a little jitter on top.
            let jitter = 0.7 + 0.6 * rng.f();
            let expected = pat.rate * tick.as_secs_f64() * jitter;
            let count = expected.floor() as usize + usize::from(rng.f() < expected.fract());
            for _ in 0..count {
                let q = dns_names.get(pat.src).copied().unwrap_or("kubernetes.default.svc.cluster.local");
                for ev in event(&cluster, &mut rng, pat, q) {
                    state.ingest(ev);
                }
            }
        }
        // Rolling restart every 45 s.
        if n.is_multiple_of(450) {
            cluster.churn(&mut rng);
            refresh(&state, &cluster);
        }
    }
}

/// `CiliumEndpoint` objects for the demo pods, parsed like real ones.
/// Ingress/egress enforcement follows the demo policies, as Cilium would.
fn demo_endpoints(
    cluster: &Cluster,
    policies: &[common::PolicyInfo],
) -> Vec<(common::EndpointInfo, endpoints::EndpointObject)> {
    let mut out = Vec::new();
    for (key, pods) in &cluster.pods {
        let (ns, app) = key.split_once('/').unwrap();
        for p in pods {
            let selected = |ingress: bool| {
                policies.iter().any(|pol| {
                    pol.selected.contains(&p.id)
                        && if ingress {
                            pol.ingress_rules + pol.ingress_deny_rules > 0
                        } else {
                            pol.egress_rules + pol.egress_deny_rules > 0
                        }
                })
            };
            let worker = p.node_name.trim_start_matches("worker-");
            let mut named_ports: Vec<Value> = Vec::new();
            for pat in PATTERNS.iter().filter(|pat| pat.dst == key.as_str()) {
                let port = json!({"name": if pat.l7.starts_with("HTTP") { "http" } else { "tcp" }, "port": pat.port, "protocol": pat.proto});
                if !named_ports.contains(&port) {
                    named_ports.push(port);
                }
            }
            let v = json!({
                "apiVersion": "cilium.io/v2",
                "kind": "CiliumEndpoint",
                "metadata": {"name": p.name, "namespace": ns},
                "status": {
                    "id": 100 + (identity(&p.id) % 3900),
                    "identity": {"id": identity(key), "labels": [
                        format!("k8s:app={app}"),
                        "k8s:io.cilium.k8s.policy.cluster=demo".to_string(),
                        format!("k8s:io.cilium.k8s.policy.serviceaccount={app}"),
                        format!("k8s:io.kubernetes.pod.namespace={ns}"),
                    ]},
                    "networking": {"addressing": [{"ipv4": p.ip}], "node": format!("192.168.10.{worker}")},
                    "policy": {"ingress": {"enforcing": selected(true)}, "egress": {"enforcing": selected(false)}},
                    "state": "ready",
                    "service-account": app,
                    "named-ports": named_ports,
                    "encryption": {"key": 1}
                }
            });
            let Ok(object) = serde_json::from_value::<DynamicObject>(v) else { continue };
            if let Some(info) = endpoints::parse_cep(&object) {
                out.push((info, endpoints::EndpointObject::Endpoint(Arc::new(object))));
            }
        }
    }
    out
}

fn cnp(ns: &str, name: &str, spec: Value) -> (PolicyKind, Value) {
    (
        PolicyKind::CiliumNetworkPolicy,
        json!({
            "apiVersion": "cilium.io/v2", "kind": "CiliumNetworkPolicy",
            "metadata": {"name": name, "namespace": ns, "creationTimestamp": "2026-10-02T09:14:00Z"},
            "spec": spec,
            "status": {"conditions": [{"type": "Valid", "status": "True"}]}
        }),
    )
}

fn policies() -> Vec<(PolicyKind, Value)> {
    vec![
        cnp(
            "shop",
            "redis-from-shop",
            json!({
                "endpointSelector": {"matchLabels": {"app": "redis"}},
                "ingress": [{
                    "fromEndpoints": [{"matchExpressions": [{"key": "app", "operator": "In", "values": ["cart", "catalog"]}]}],
                    "toPorts": [{"ports": [{"port": "6379", "protocol": "TCP"}]}]
                }]
            }),
        ),
        cnp(
            "payments",
            "postgres-lockdown",
            json!({
                "endpointSelector": {"matchLabels": {"app": "postgres"}},
                "ingress": [{
                    "fromEndpoints": [{"matchLabels": {"app": "payments-api"}}],
                    "toPorts": [{"ports": [{"port": "5432", "protocol": "TCP"}]}]
                }]
            }),
        ),
        cnp(
            "payments",
            "payments-egress",
            json!({
                "endpointSelector": {"matchLabels": {"app": "payments-api"}},
                "egress": [
                    {"toFQDNs": [{"matchName": "api.stripe.com"}],
                     "toPorts": [{"ports": [{"port": "443", "protocol": "TCP"}]}]},
                    {"toEndpoints": [{"matchLabels": {"k8s:io.kubernetes.pod.namespace": "kube-system", "k8s-app": "kube-dns"}}],
                     "toPorts": [{"ports": [{"port": "53", "protocol": "ANY"}], "rules": {"dns": [{"matchPattern": "*"}]}}]},
                    {"toEndpoints": [{"matchLabels": {"app": "postgres"}}, {"matchLabels": {"app": "fraud-scoring"}}]}
                ]
            }),
        ),
        cnp(
            "payments",
            "fraud-scoring-no-egress",
            json!({
                "endpointSelector": {"matchLabels": {"app": "fraud-scoring"}},
                "egressDeny": [{"toEntities": ["world"]}]
            }),
        ),
        (
            PolicyKind::CiliumClusterwideNetworkPolicy,
            json!({
                "apiVersion": "cilium.io/v2", "kind": "CiliumClusterwideNetworkPolicy",
                "metadata": {"name": "allow-dns", "creationTimestamp": "2026-09-12T08:00:00Z"},
                "spec": {
                    "endpointSelector": {},
                    "egress": [{
                        "toEndpoints": [{"matchLabels": {"k8s:io.kubernetes.pod.namespace": "kube-system", "k8s-app": "kube-dns"}}],
                        "toPorts": [{"ports": [{"port": "53", "protocol": "ANY"}], "rules": {"dns": [{"matchPattern": "*"}]}}]
                    }]
                },
                "status": {"conditions": [{"type": "Valid", "status": "True"}]}
            }),
        ),
        (
            PolicyKind::CiliumClusterwideNetworkPolicy,
            json!({
                "apiVersion": "cilium.io/v2", "kind": "CiliumClusterwideNetworkPolicy",
                "metadata": {"name": "host-firewall", "creationTimestamp": "2026-09-12T08:00:00Z"},
                "spec": {
                    "nodeSelector": {"matchLabels": {"node-role.kubernetes.io/worker": ""}},
                    "ingress": [
                        {"fromEntities": ["cluster"]},
                        {"fromEntities": ["world"], "toPorts": [{"ports": [{"port": "443", "protocol": "TCP"}]}]}
                    ]
                }
            }),
        ),
        (
            PolicyKind::NetworkPolicy,
            json!({
                "apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
                "metadata": {"name": "web-ingress", "namespace": "frontend", "creationTimestamp": "2026-08-30T16:40:00Z"},
                "spec": {
                    "podSelector": {"matchLabels": {"app": "web"}},
                    "policyTypes": ["Ingress"],
                    "ingress": [{"ports": [{"port": 443, "protocol": "TCP"}]}]
                }
            }),
        ),
    ]
}

fn demo_config() -> common::ConfigView {
    let data: BTreeMap<String, String> = [
        ("cluster-name", "demo"),
        ("cluster-id", "1"),
        ("routing-mode", "native"),
        ("ipv4-native-routing-cidr", "10.0.0.0/16"),
        ("auto-direct-node-routes", "true"),
        ("tunnel-protocol", "vxlan"),
        ("kube-proxy-replacement", "true"),
        ("bpf-lb-algorithm", "maglev"),
        ("bpf-lb-mode", "dsr"),
        ("enable-bpf-masquerade", "true"),
        ("enable-ipv4", "true"),
        ("enable-ipv6", "false"),
        ("ipam", "kubernetes"),
        ("enable-policy", "default"),
        ("policy-audit-mode", "false"),
        ("enable-l7-proxy", "true"),
        ("identity-allocation-mode", "crd"),
        ("enable-wireguard", "true"),
        ("enable-hubble", "true"),
        ("hubble-listen-address", ":4244"),
        ("hubble-disable-tls", "false"),
        ("hubble-metrics", "dns drop tcp flow port-distribution icmp httpV2"),
        ("enable-bandwidth-manager", "true"),
        ("enable-gateway-api", "true"),
        ("enable-l2-announcements", "false"),
        ("bpf-map-dynamic-size-ratio", "0.0025"),
        ("monitor-aggregation", "medium"),
        ("prometheus-serve-addr", ":9962"),
        ("debug", "false"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let overrides: Vec<NodeConfigOverride> = [json!({
        "metadata": {"name": "gpu-pool", "namespace": "kube-system"},
        "spec": {
            "nodeSelector": {"matchLabels": {"pool": "gpu"}},
            "defaults": {"enable-bandwidth-manager": "false", "mtu": "9000"}
        }
    })]
    .iter()
    .filter_map(config::parse_node_config)
    .collect();
    config::build_view("kube-system/cilium-config (demo)".into(), &data, overrides)
}
