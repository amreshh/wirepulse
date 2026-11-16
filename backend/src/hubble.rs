//! Streams flows from hubble-relay (Observer.GetFlows, follow mode).

use crate::proto::flow::{self, layer4, layer7, Endpoint};
use crate::proto::observer::{get_flows_response::ResponseTypes, observer_client::ObserverClient, GetFlowsRequest};
use crate::state::{now_ms, AppState};
use common::{EndpointDetail, FlowDetail, FlowEvent, L7Detail, Verdict};
use std::sync::Arc;
use std::time::Duration;
use tonic::transport::Endpoint as GrpcEndpoint;
use tracing::{info, warn};

/// Flows requested from Hubble's ring buffer on connect, to fill the table.
const BACKFILL: u64 = 500;
/// A reconnect asks for the flows since the newest one we have, if that is
/// at most this old; after a longer outage it takes a fresh `BACKFILL`.
const MAX_RESUME_AGE_MS: i64 = 60_000;
/// Resume a little early: nodes report with some delay, so flows stamped
/// just before the newest one we have may not have reached us yet. Repeats
/// are dropped by UUID.
const RESUME_OVERLAP_MS: i64 = 2_000;

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Longest piece of text kept from a flow (an HTTP URL, a DNS name, ...).
/// Flows can come from any workload in the cluster, and one sending huge URLs
/// would otherwise fill the server's buffers, and every open browser, with them.
const MAX_TEXT: usize = 2048;
/// Longest one-line L7 summary: it goes to the browsers with every flow.
const MAX_SUMMARY: usize = 512;
/// Most entries kept from a list in a flow (DNS answers, the names of an IP).
const MAX_ITEMS: usize = 32;

pub async fn run(addr: String, state: Arc<AppState>) {
    let mut backoff = MIN_BACKOFF;
    loop {
        let mut connected = false;
        match stream(&addr, &state, &mut connected).await {
            Ok(()) => {
                info!("hubble stream ended, reconnecting");
                state.update_status(|s| s.hubble_connected = false);
            }
            Err(e) => {
                warn!(error = format!("{e:#}"), %addr, "hubble-relay unavailable");
                state.update_status(|s| {
                    s.hubble_connected = false;
                    s.message = format!("hubble-relay ({addr}): {e:#}");
                });
            }
        }
        // Back off while the relay is unreachable, but not after a stream
        // that worked: otherwise every later restart waits the maximum.
        if connected {
            backoff = MIN_BACKOFF;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Follows the flow stream until it ends. `connected` is set once the relay
/// has accepted the request.
async fn stream(addr: &str, state: &AppState, connected: &mut bool) -> anyhow::Result<()> {
    let channel = GrpcEndpoint::from_shared(addr.to_string())?
        .connect_timeout(Duration::from_secs(5))
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .http2_keep_alive_interval(Duration::from_secs(30))
        .connect()
        .await?;
    let mut client = ObserverClient::new(channel).max_decoding_message_size(16 * 1024 * 1024);
    let (newest, seen) = state.seen_flows();
    let mut stream = client.get_flows(request(newest, now_ms())).await?.into_inner();
    *connected = true;
    info!(%addr, "connected to hubble-relay");
    state.update_status(|s| {
        s.hubble_connected = true;
        s.message.clear();
    });
    while let Some(resp) = stream.message().await? {
        match resp.response_types {
            // After a reconnect the relay repeats flows we already have.
            Some(ResponseTypes::Flow(f)) if !f.uuid.is_empty() && seen.contains(&f.uuid) => {}
            Some(ResponseTypes::Flow(f)) => state.ingest(convert(&f)),
            Some(ResponseTypes::LostEvents(l)) => {
                tracing::debug!(lost = l.num_events_lost, "hubble lost events");
            }
            _ => {}
        }
    }
    Ok(())
}

/// The first connection fills the table with the last `BACKFILL` flows; a
/// reconnect picks up where the newest flow we have (`newest`, ms) left off.
fn request(newest: Option<i64>, now: i64) -> GetFlowsRequest {
    match newest {
        Some(t) if now.saturating_sub(t) <= MAX_RESUME_AGE_MS => {
            // A node clock ahead of ours must not make us skip ahead.
            let since = t.min(now).saturating_sub(RESUME_OVERLAP_MS);
            GetFlowsRequest {
                since: Some(prost_types::Timestamp {
                    seconds: since.div_euclid(1000),
                    nanos: (since.rem_euclid(1000) * 1_000_000) as i32,
                }),
                follow: true,
                ..Default::default()
            }
        }
        _ => GetFlowsRequest { number: BACKFILL, follow: true, ..Default::default() },
    }
}

/// Reserved identity numbers, used when an endpoint carries no labels.
fn reserved_name(identity: u32) -> Option<&'static str> {
    Some(match identity {
        1 => "host",
        2 | 9 | 10 => "world",
        3 => "unmanaged",
        4 => "health",
        5 => "init",
        6 => "remote-node",
        7 => "kube-apiserver",
        8 => "ingress",
        _ => return None,
    })
}

pub fn endpoint_id(ep: &Endpoint) -> String {
    if !ep.pod_name.is_empty() {
        return format!("{}/{}", ep.namespace, ep.pod_name);
    }
    // kube-apiserver endpoints also carry reserved:remote-node/host;
    // prefer the most specific label.
    for preferred in ["reserved:kube-apiserver", "reserved:ingress"] {
        if ep.labels.iter().any(|l| l == preferred) {
            return preferred.to_string();
        }
    }
    if let Some(l) = ep.labels.iter().find(|l| l.starts_with("reserved:")) {
        // world-ipv4 / world-ipv6 in dual-stack clusters.
        if l.starts_with("reserved:world") {
            return "reserved:world".into();
        }
        return l.clone();
    }
    if ep.labels.iter().any(|l| l.starts_with("cidr:") || l.starts_with("fqdn:")) {
        return "reserved:world".into();
    }
    match reserved_name(ep.identity) {
        Some(n) => format!("reserved:{n}"),
        None => format!("identity:{}", ep.identity),
    }
}

fn workload(ep: &Endpoint) -> String {
    ep.workloads.first().map(|w| format!("{}/{}", w.kind, w.name)).unwrap_or_default()
}

fn verdict(v: i32) -> Verdict {
    match flow::Verdict::try_from(v) {
        Ok(flow::Verdict::Forwarded) => Verdict::Forwarded,
        Ok(flow::Verdict::Dropped) => Verdict::Dropped,
        Ok(flow::Verdict::Error) => Verdict::Error,
        Ok(flow::Verdict::Audit) => Verdict::Audit,
        Ok(flow::Verdict::Redirected) => Verdict::Redirected,
        Ok(flow::Verdict::Traced) => Verdict::Traced,
        Ok(flow::Verdict::Translated) => Verdict::Translated,
        _ => Verdict::Unknown,
    }
}

fn policy_names(ps: &[flow::Policy]) -> Vec<String> {
    ps.iter()
        .map(|p| if p.namespace.is_empty() { p.name.clone() } else { format!("{}/{}", p.namespace, p.name) })
        .collect()
}

/// `TO_ENDPOINT` -> `to-endpoint`.
fn enum_name(s: &str) -> String {
    s.to_ascii_lowercase().replace('_', "-")
}

fn tcp_flags(f: &flow::TcpFlags) -> String {
    [
        (f.syn, "SYN"),
        (f.ack, "ACK"),
        (f.fin, "FIN"),
        (f.rst, "RST"),
        (f.psh, "PSH"),
        (f.urg, "URG"),
        (f.ece, "ECE"),
        (f.cwr, "CWR"),
        (f.ns, "NS"),
    ]
    .iter()
    .filter(|(on, _)| *on)
    .map(|(_, n)| *n)
    .collect::<Vec<_>>()
    .join(" ")
}

/// `s`, cut to at most `max` bytes (at a character boundary) and marked with an ellipsis.
fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// The first `MAX_ITEMS` entries of `v`, each clipped to `MAX_TEXT`.
fn clip_all(v: &[String]) -> Vec<String> {
    v.iter().take(MAX_ITEMS).map(|s| clip(s, MAX_TEXT)).collect()
}

fn endpoint_detail(
    ep: Option<&Endpoint>,
    ip: &str,
    port: u32,
    svc: Option<&flow::Service>,
    names: &[String],
) -> EndpointDetail {
    EndpointDetail {
        id: ep.map(endpoint_id).unwrap_or_else(|| "unknown".into()),
        namespace: ep.map(|e| e.namespace.clone()).unwrap_or_default(),
        pod: ep.map(|e| e.pod_name.clone()).unwrap_or_default(),
        workload: ep.map(workload).unwrap_or_default(),
        identity: ep.map_or(0, |e| e.identity),
        ip: ip.to_string(),
        port,
        labels: ep.map(|e| e.labels.clone()).unwrap_or_default(),
        service: svc.filter(|s| !s.name.is_empty()).map(|s| format!("{}/{}", s.namespace, s.name)).unwrap_or_default(),
        names: clip_all(names),
    }
}

pub fn convert(f: &flow::Flow) -> FlowDetail {
    let (protocol, src_port, dst_port, flags) = match f.l4.as_ref().and_then(|l| l.protocol.as_ref()) {
        Some(layer4::Protocol::Tcp(t)) => {
            ("TCP", t.source_port, t.destination_port, t.flags.as_ref().map(tcp_flags).unwrap_or_default())
        }
        Some(layer4::Protocol::Udp(u)) => ("UDP", u.source_port, u.destination_port, String::new()),
        Some(layer4::Protocol::Sctp(s)) => ("SCTP", s.source_port, s.destination_port, String::new()),
        Some(layer4::Protocol::IcmPv4(_)) => ("ICMPv4", 0, 0, String::new()),
        Some(layer4::Protocol::IcmPv6(_)) => ("ICMPv6", 0, 0, String::new()),
        Some(layer4::Protocol::Vrrp(_)) => ("VRRP", 0, 0, String::new()),
        Some(layer4::Protocol::Igmp(_)) => ("IGMP", 0, 0, String::new()),
        None => ("", 0, 0, String::new()),
    };

    let mut l7_detail = None;
    let mut l7 = String::new();
    if let Some(layer) = f.l7.as_ref() {
        let mut d = L7Detail {
            flow_type: flow::L7FlowType::try_from(layer.r#type)
                .map(|t| match t {
                    flow::L7FlowType::Request => "request",
                    flow::L7FlowType::Response => "response",
                    flow::L7FlowType::Sample => "sample",
                    flow::L7FlowType::UnknownL7Type => "",
                })
                .unwrap_or_default()
                .into(),
            latency_ms: layer.latency_ns as f64 / 1e6,
            ..Default::default()
        };
        #[allow(deprecated)] // Kafka records are deprecated upstream but still sent.
        match layer.record.as_ref() {
            Some(layer7::Record::Http(h)) => {
                d.kind = "HTTP".into();
                d.http_method = clip(&h.method, MAX_TEXT);
                d.http_url = clip(&h.url, MAX_TEXT);
                d.http_code = h.code;
                d.http_protocol = clip(&h.protocol, MAX_TEXT);
                l7 = format!("HTTP {} {}", d.http_method, d.http_url);
                if h.code != 0 {
                    l7.push_str(&format!(" {}", h.code));
                }
            }
            Some(layer7::Record::Dns(q)) => {
                d.kind = "DNS".into();
                d.dns_query = clip(q.query.trim_end_matches('.'), MAX_TEXT);
                d.dns_ips = clip_all(&q.ips);
                d.dns_rcode = q.rcode;
                d.dns_qtypes = clip_all(&q.qtypes);
                d.dns_cnames = clip_all(&q.cnames);
                d.dns_ttl = q.ttl;
                l7 = format!("DNS {}", d.dns_query);
                if !d.dns_ips.is_empty() {
                    l7.push_str(&format!(" → {}", d.dns_ips.join(", ")));
                }
            }
            Some(layer7::Record::Kafka(_)) => {
                d.kind = "Kafka".into();
                l7 = "Kafka".into();
            }
            None => {}
        }
        l7 = clip(&l7, MAX_SUMMARY);
        l7_detail = Some(d);
    }

    let v = verdict(f.verdict);
    let drop_reason = if matches!(v, Verdict::Dropped | Verdict::Error) {
        flow::DropReason::try_from(f.drop_reason_desc).map(|d| d.as_str_name().to_string()).unwrap_or_default()
    } else {
        String::new()
    };
    let direction = match flow::TrafficDirection::try_from(f.traffic_direction) {
        Ok(flow::TrafficDirection::Ingress) => "ingress",
        Ok(flow::TrafficDirection::Egress) => "egress",
        _ => "",
    };
    let trace_point = flow::TraceObservationPoint::try_from(f.trace_observation_point)
        .ok()
        .filter(|p| *p != flow::TraceObservationPoint::UnknownPoint)
        .map(|p| enum_name(p.as_str_name()))
        .unwrap_or_default();

    let mut allowed_by = policy_names(&f.egress_allowed_by);
    allowed_by.extend(policy_names(&f.ingress_allowed_by));
    let mut denied_by = policy_names(&f.egress_denied_by);
    denied_by.extend(policy_names(&f.ingress_denied_by));
    let mut policies = allowed_by.clone();
    policies.extend(denied_by.iter().map(|p| format!("deny {p}")));

    let ip = f.ip.clone().unwrap_or_default();
    let source = endpoint_detail(f.source.as_ref(), &ip.source, src_port, f.source_service.as_ref(), &f.source_names);
    let destination = endpoint_detail(
        f.destination.as_ref(),
        &ip.destination,
        dst_port,
        f.destination_service.as_ref(),
        &f.destination_names,
    );

    let event = FlowEvent {
        id: 0,
        time_ms: f
            .time
            .as_ref()
            .map(|t| t.seconds.saturating_mul(1000).saturating_add(i64::from(t.nanos) / 1_000_000))
            .unwrap_or_else(now_ms),
        node: f.node_name.clone(),
        source: source.id.clone(),
        destination: destination.id.clone(),
        src_workload: source.workload.clone(),
        dst_workload: destination.workload.clone(),
        protocol: protocol.into(),
        dst_port,
        verdict: v,
        drop_reason,
        direction: direction.into(),
        l7,
        policies,
        is_reply: f.is_reply.unwrap_or(false),
        dst_dns: destination.names.first().cloned().unwrap_or_default(),
        src_ip: ip.source.clone(),
        dst_ip: ip.destination.clone(),
        src_port,
        src_identity: source.identity,
        dst_identity: destination.identity,
        tcp_flags: flags,
        latency_ms: l7_detail.as_ref().map_or(0.0, |d| d.latency_ms as f32),
        trace_point,
    };

    FlowDetail {
        event,
        uuid: f.uuid.clone(),
        source,
        destination,
        ip_version: flow::IpVersion::try_from(ip.ip_version)
            .ok()
            .filter(|v| *v != flow::IpVersion::IpNotUsed)
            .map(|v| v.as_str_name().to_string())
            .unwrap_or_default(),
        encrypted: ip.encrypted,
        trace_reason: flow::TraceReason::try_from(f.trace_reason)
            .ok()
            .map(|r| enum_name(r.as_str_name()))
            .filter(|r| !r.contains("unknown"))
            .unwrap_or_default(),
        interface: f.interface.as_ref().map(|i| i.name.clone()).unwrap_or_default(),
        l7: l7_detail,
        allowed_by,
        denied_by,
    }
}

/// End-to-end over real gRPC: a fake hubble-relay streams flows to `stream`.
#[cfg(test)]
mod relay_tests {
    use super::*;
    use crate::proto::observer::observer_server::{Observer, ObserverServer};
    use crate::proto::observer::*;
    use futures::Stream;
    use std::pin::Pin;
    use std::sync::Mutex;
    use tonic::{Request, Response, Status};

    type Boxed<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

    /// Streams the same two flows on every call (as a relay replaying its
    /// buffer would), plus a new one from the second call on.
    struct FakeRelay {
        requests: Arc<Mutex<Vec<GetFlowsRequest>>>,
    }

    fn ep(ns: &str, pod: &str, labels: &[&str], workload: &str) -> Endpoint {
        Endpoint {
            namespace: ns.into(),
            pod_name: pod.into(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            workloads: if workload.is_empty() {
                vec![]
            } else {
                vec![flow::Workload { name: workload.into(), kind: "Deployment".into() }]
            },
            ..Default::default()
        }
    }

    fn flow_resp(f: flow::Flow) -> GetFlowsResponse {
        GetFlowsResponse { response_types: Some(ResponseTypes::Flow(f)), ..Default::default() }
    }

    #[tonic::async_trait]
    impl Observer for FakeRelay {
        type GetFlowsStream = Boxed<GetFlowsResponse>;
        type GetAgentEventsStream = Boxed<GetAgentEventsResponse>;
        type GetDebugEventsStream = Boxed<GetDebugEventsResponse>;

        async fn get_flows(&self, req: Request<GetFlowsRequest>) -> Result<Response<Self::GetFlowsStream>, Status> {
            let calls = {
                let mut r = self.requests.lock().unwrap();
                r.push(req.into_inner());
                r.len()
            };
            let now = prost_types::Timestamp::from(std::time::SystemTime::now());
            let tcp = |port| {
                Some(flow::Layer4 {
                    protocol: Some(layer4::Protocol::Tcp(flow::Tcp { destination_port: port, ..Default::default() })),
                })
            };
            let mut items = vec![
                Ok(flow_resp(flow::Flow {
                    uuid: "a".into(),
                    time: Some(now),
                    verdict: flow::Verdict::Forwarded as i32,
                    source: Some(ep("shop", "web-1", &[], "web")),
                    destination: Some(ep("pay", "db-0", &[], "")),
                    l4: tcp(5432),
                    node_name: "worker-1".into(),
                    ..Default::default()
                })),
                Ok(GetFlowsResponse {
                    response_types: Some(ResponseTypes::LostEvents(flow::LostEvent {
                        num_events_lost: 3,
                        ..Default::default()
                    })),
                    ..Default::default()
                }),
                Ok(flow_resp(flow::Flow {
                    uuid: "b".into(),
                    time: Some(now),
                    verdict: flow::Verdict::Dropped as i32,
                    drop_reason_desc: flow::DropReason::PolicyDenied as i32,
                    source: Some(ep("", "", &["reserved:world"], "")),
                    destination: Some(ep("shop", "web-1", &[], "web")),
                    l4: tcp(22),
                    ingress_denied_by: vec![flow::Policy {
                        name: "no-ssh".into(),
                        namespace: "shop".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                })),
            ];
            if calls > 1 {
                items.push(Ok(flow_resp(flow::Flow {
                    uuid: "c".into(),
                    time: Some(now),
                    verdict: flow::Verdict::Forwarded as i32,
                    source: Some(ep("shop", "web-1", &[], "web")),
                    destination: Some(ep("shop", "cart-0", &[], "")),
                    l4: tcp(8080),
                    ..Default::default()
                })));
            }
            Ok(Response::new(Box::pin(futures::stream::iter(items))))
        }

        async fn get_agent_events(
            &self,
            _: Request<GetAgentEventsRequest>,
        ) -> Result<Response<Self::GetAgentEventsStream>, Status> {
            Err(Status::unimplemented("fake"))
        }

        async fn get_debug_events(
            &self,
            _: Request<GetDebugEventsRequest>,
        ) -> Result<Response<Self::GetDebugEventsStream>, Status> {
            Err(Status::unimplemented("fake"))
        }

        async fn get_nodes(&self, _: Request<GetNodesRequest>) -> Result<Response<GetNodesResponse>, Status> {
            Err(Status::unimplemented("fake"))
        }

        async fn get_namespaces(
            &self,
            _: Request<GetNamespacesRequest>,
        ) -> Result<Response<GetNamespacesResponse>, Status> {
            Err(Status::unimplemented("fake"))
        }

        async fn server_status(
            &self,
            _: Request<ServerStatusRequest>,
        ) -> Result<Response<ServerStatusResponse>, Status> {
            Err(Status::unimplemented("fake"))
        }
    }

    #[tokio::test]
    async fn streams_flows_from_relay() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(ObserverServer::new(FakeRelay { requests: requests.clone() }))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
        );

        let state = crate::state::AppState::new(false);
        let mut connected = false;
        stream(&format!("http://{addr}"), &state, &mut connected).await.unwrap();

        {
            let r = requests.lock().unwrap();
            assert!(r[0].follow, "client must follow");
            assert_eq!(r[0].number, BACKFILL);
            assert!(r[0].since.is_none());
        }
        assert!(connected);
        assert!(state.status.read().unwrap().hubble_connected);
        let flows = state.recent_flows();
        assert_eq!(flows.len(), 2);
        assert_eq!(flows[0].destination, "pay/db-0");
        assert_eq!(flows[1].source, "reserved:world");
        assert_eq!(flows[1].policies, vec!["deny shop/no-ssh".to_string()]);

        let g = state.build_graph();
        assert!(g.edges.iter().any(|e| e.source == "shop/web-1" && e.target == "pay/db-0"));
        let web = g.nodes.iter().find(|n| n.id == "shop/web-1").unwrap();
        assert_eq!(web.workload, "Deployment/web");
        assert!(g.nodes.iter().any(|n| n.id == "reserved:world"));

        // Reconnect: resume from the newest flow, and drop the repeats.
        stream(&format!("http://{addr}"), &state, &mut connected).await.unwrap();
        {
            let r = requests.lock().unwrap();
            assert!(r[1].follow);
            assert_eq!(r[1].number, 0, "`number` and `since` don't mix");
            assert!(r[1].since.is_some());
        }
        let flows = state.recent_flows();
        assert_eq!(flows.len(), 3, "only the new flow is added");
        assert_eq!(flows[2].destination, "shop/cart-0");
    }

    #[test]
    fn resume_window() {
        let now = 1_700_000_000_000;
        assert_eq!(request(None, now).number, BACKFILL);
        let r = request(Some(now - 5_000), now);
        let since = r.since.unwrap();
        assert_eq!(since.seconds * 1000 + i64::from(since.nanos) / 1_000_000, now - 5_000 - RESUME_OVERLAP_MS);
        // Too old: start over with a backfill.
        assert_eq!(request(Some(now - MAX_RESUME_AGE_MS - 1), now).number, BACKFILL);
        // A node clock ahead of ours doesn't push `since` into the future.
        let ahead = request(Some(now + 60_000), now).since.unwrap();
        assert_eq!(ahead.seconds * 1000 + i64::from(ahead.nanos) / 1_000_000, now - RESUME_OVERLAP_MS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(ns: &str, pod: &str, labels: &[&str], identity: u32) -> Endpoint {
        Endpoint {
            namespace: ns.into(),
            pod_name: pod.into(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            identity,
            ..Default::default()
        }
    }

    #[test]
    fn endpoint_ids() {
        assert_eq!(endpoint_id(&ep("shop", "web-1", &[], 1234)), "shop/web-1");
        assert_eq!(endpoint_id(&ep("", "", &["reserved:world-ipv4"], 9)), "reserved:world");
        assert_eq!(
            endpoint_id(&ep("", "", &["reserved:remote-node", "reserved:kube-apiserver"], 7)),
            "reserved:kube-apiserver"
        );
        assert_eq!(endpoint_id(&ep("", "", &["cidr:10.0.0.0/8"], 16777217)), "reserved:world");
        assert_eq!(endpoint_id(&ep("", "", &[], 1)), "reserved:host");
        assert_eq!(endpoint_id(&ep("", "", &[], 99999)), "identity:99999");
    }

    #[test]
    fn converts_dropped_http_flow() {
        let f = flow::Flow {
            verdict: flow::Verdict::Dropped as i32,
            drop_reason_desc: flow::DropReason::PolicyDenied as i32,
            source: Some(ep("shop", "web-1", &[], 1)),
            destination: Some(ep("pay", "db-0", &[], 2)),
            l4: Some(flow::Layer4 {
                protocol: Some(layer4::Protocol::Tcp(flow::Tcp {
                    source_port: 40000,
                    destination_port: 5432,
                    flags: None,
                })),
            }),
            l7: Some(flow::Layer7 {
                record: Some(layer7::Record::Http(flow::Http {
                    code: 403,
                    method: "GET".into(),
                    url: "/x".into(),
                    ..Default::default()
                })),
                ..Default::default()
            }),
            is_reply: Some(false),
            ..Default::default()
        };
        let e = convert(&f).event;
        assert_eq!(e.source, "shop/web-1");
        assert_eq!(e.destination, "pay/db-0");
        assert_eq!(e.protocol, "TCP");
        assert_eq!(e.dst_port, 5432);
        assert_eq!(e.verdict, Verdict::Dropped);
        assert_eq!(e.drop_reason, "POLICY_DENIED");
        assert_eq!(e.l7, "HTTP GET /x 403");
        assert_eq!(e.tcp_flags, "");
    }

    #[test]
    fn clips_long_text_from_flows() {
        // Any client can make a URL this long; a DNS answer can be long too.
        let http = flow::Flow {
            l7: Some(flow::Layer7 {
                record: Some(layer7::Record::Http(flow::Http {
                    code: 200,
                    method: "GET".into(),
                    url: format!("/{}", "a".repeat(100_000)),
                    ..Default::default()
                })),
                ..Default::default()
            }),
            destination_names: (0..100).map(|i| format!("name-{i}.example")).collect(),
            ..Default::default()
        };
        let d = convert(&http);
        assert!(d.event.l7.starts_with("HTTP GET /aaa") && d.event.l7.ends_with('…'));
        assert!(d.event.l7.len() <= MAX_SUMMARY + '…'.len_utf8());
        assert_eq!(d.l7.unwrap().http_url.len(), MAX_TEXT + '…'.len_utf8());
        assert_eq!(d.destination.names.len(), MAX_ITEMS);
        assert_eq!(d.event.dst_dns, "name-0.example");

        let dns = flow::Flow {
            l7: Some(flow::Layer7 {
                record: Some(layer7::Record::Dns(flow::Dns {
                    query: "big.example.".into(),
                    ips: (0..1000).map(|i| format!("10.0.{}.{}", i / 256, i % 256)).collect(),
                    ..Default::default()
                })),
                ..Default::default()
            }),
            ..Default::default()
        };
        let d = convert(&dns);
        assert!(d.event.l7.starts_with("DNS big.example → 10.0.0.0, 10.0.0.1, "));
        assert!(d.event.l7.len() <= MAX_SUMMARY + '…'.len_utf8());
        assert_eq!(d.l7.unwrap().dns_ips.len(), MAX_ITEMS);
    }

    #[test]
    fn clip_cuts_between_characters() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("abcdef", 3), "abc…");
        // 'é' takes bytes 1 and 2: a cut after byte 2 would split it.
        assert_eq!(clip("héllo", 2), "h…");
    }
}
