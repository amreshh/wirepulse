//! HTTP API, WebSocket push and static file serving.

use crate::hosts::{strip_port, AllowedHosts};
use crate::state::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use common::{ConfigView, EndpointsView, FlowDetail, FlowEvent, Graph, Info, PolicyInfo};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;
use tower_http::compression::CompressionLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;
use tracing::warn;

pub fn router(state: Arc<AppState>, static_dir: &Path, hosts: AllowedHosts) -> Router {
    let hosts = Arc::new(hosts);
    let spa = ServeDir::new(static_dir).fallback(ServeFile::new(static_dir.join("index.html")));
    Router::new()
        .route("/api/info", get(info))
        .route("/api/graph", get(graph))
        .route("/api/flows", get(flows))
        .route("/api/flows/{id}", get(flow_detail))
        .route("/api/policies", get(policies))
        .route("/api/config", get(config))
        .route("/api/endpoints", get(endpoints))
        .route("/api/endpoints/{namespace}/{name}", get(endpoint_yaml))
        .route("/api/ws", get(ws))
        .route("/healthz", get(|| async { "ok" }))
        .fallback_service(spa)
        .layer(CompressionLayer::new())
        .layer(Extension(hosts.clone()))
        .layer(middleware::from_fn_with_state(hosts, check_host))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Refuses requests addressed to a host name we don't answer to (see
/// `hosts`). Health checks are exempt so probes never depend on it.
async fn check_host(State(hosts): State<Arc<AllowedHosts>>, req: Request, next: Next) -> Response {
    let host = match req.headers().get(header::HOST) {
        Some(h) => h.to_str().ok().map(str::to_string),
        // HTTP/2 carries the host in the URI instead.
        None => req.uri().authority().map(|a| a.as_str().to_string()),
    };
    let allowed = match &host {
        Some(h) => hosts.allows(h),
        // Present but not text: refuse. Absent (HTTP/1.0): not a browser.
        None => !req.headers().contains_key(header::HOST),
    };
    if allowed || req.uri().path() == "/healthz" {
        return next.run(req).await;
    }
    warn!(host = ?host, path = %req.uri().path(), "rejected request for a host name not in --allowed-hosts");
    (
        StatusCode::FORBIDDEN,
        format!(
            "wirepulse does not answer to host {:?}. Add it with --allowed-hosts \
             (WIREPULSE_ALLOWED_HOSTS) if you reach wirepulse by that name.\n",
            host.as_deref().map(strip_port).unwrap_or_default()
        ),
    )
        .into_response()
}

async fn info(State(s): State<Arc<AppState>>) -> Json<Info> {
    let mut info = s.info.clone();
    info.cluster_name = s
        .config
        .read()
        .unwrap()
        .entries
        .iter()
        .find(|e| e.key == "cluster-name")
        .map(|e| e.value.clone())
        .unwrap_or_default();
    Json(info)
}

async fn graph(State(s): State<Arc<AppState>>) -> Json<Graph> {
    Json(s.build_graph())
}

async fn flows(State(s): State<Arc<AppState>>) -> Json<Vec<FlowEvent>> {
    Json(s.recent_flows())
}

async fn flow_detail(
    State(s): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<u64>,
) -> Result<Json<FlowDetail>, axum::http::StatusCode> {
    s.flow_detail(id).map(Json).ok_or(axum::http::StatusCode::NOT_FOUND)
}

async fn policies(State(s): State<Arc<AppState>>) -> Json<Vec<PolicyInfo>> {
    Json(s.policies.read().unwrap().clone())
}

async fn endpoints(State(s): State<Arc<AppState>>) -> Json<EndpointsView> {
    Json(s.endpoints.read().unwrap().clone())
}

/// The endpoint's object as YAML.
async fn endpoint_yaml(
    State(s): State<Arc<AppState>>,
    axum::extract::Path((namespace, name)): axum::extract::Path<(String, String)>,
) -> Result<String, axum::http::StatusCode> {
    s.endpoint_raw
        .read()
        .unwrap()
        .get(&format!("{namespace}/{name}"))
        .map(crate::policy::to_yaml)
        .ok_or(axum::http::StatusCode::NOT_FOUND)
}

async fn config(State(s): State<Arc<AppState>>) -> Json<ConfigView> {
    Json(s.config.read().unwrap().clone())
}

/// The UI never sends anything that matters, so keep what a client can make
/// the server buffer small.
const MAX_CLIENT_MESSAGE: usize = 64 * 1024;

async fn ws(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    Extension(hosts): Extension<Arc<AllowedHosts>>,
    State(s): State<Arc<AppState>>,
) -> Response {
    if !same_origin(&headers, &hosts) {
        warn!(
            origin = ?headers.get(header::ORIGIN),
            host = ?headers.get(header::HOST),
            "rejected WebSocket from another origin"
        );
        return StatusCode::FORBIDDEN.into_response();
    }
    ws.max_message_size(MAX_CLIENT_MESSAGE)
        .max_frame_size(MAX_CLIENT_MESSAGE)
        .on_upgrade(move |socket| client(socket, s))
        .into_response()
}

/// Browsers let any web page open a WebSocket to any address, and the
/// same-origin policy does not protect the stream. Without this check, a site
/// the user happens to visit could read the cluster's flows through, say, a
/// `kubectl port-forward` on localhost. So the `Origin` must match the host
/// the UI was served from (`Host`, or `X-Forwarded-Host` behind a proxy,
/// which pages cannot set), or be a name listed with `--allowed-hosts` (for
/// proxies that rewrite `Host` and add nothing). Clients that send no
/// `Origin` are not browsers and are allowed.
fn same_origin(headers: &HeaderMap, hosts: &AllowedHosts) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    // `scheme://host[:port]`; anything else (e.g. `null`) is rejected.
    let Some((scheme, origin_host)) = origin.to_str().ok().and_then(|o| o.split_once("://")) else {
        return false;
    };
    let default_port = if scheme.eq_ignore_ascii_case("https") { ":443" } else { ":80" };
    let normalize = |h: &str| {
        let h = h.trim().to_ascii_lowercase();
        h.strip_suffix(default_port).map(str::to_string).unwrap_or(h)
    };
    let origin_host = normalize(origin_host);
    [header::HOST, HeaderName::from_static("x-forwarded-host")]
        .iter()
        .filter_map(|name| headers.get(name)?.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|host| normalize(host) == origin_host)
        || hosts.lists(strip_port(&origin_host))
}

async fn client(mut socket: WebSocket, state: Arc<AppState>) {
    // Subscribe first so nothing is missed between the snapshot and the
    // stream. A flow can then arrive twice; the UI drops repeats by id.
    let mut rx = state.tx.subscribe();
    for msg in [state.status_msg(), state.latest_graph_msg(), state.recent_flows_msg()] {
        if socket.send(Message::Text(msg.as_ref().into())).await.is_err() {
            return;
        }
    }
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Ok(m) => {
                    if socket.send(Message::Text(m.as_ref().into())).await.is_err() {
                        return;
                    }
                }
                // A slow client skips messages; the next graph snapshot catches it up.
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        pairs.iter().map(|(k, v)| (HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap())).collect()
    }

    #[test]
    fn websocket_origin_from_a_listed_proxy_name() {
        // A proxy that rewrites Host and sets no X-Forwarded-Host.
        let h = headers(&[("origin", "https://wp.example.com"), ("host", "wirepulse.wirepulse.svc")]);
        assert!(!same_origin(&h, &AllowedHosts::default()));
        assert!(same_origin(&h, &AllowedHosts::new(&["wp.example.com", "wirepulse.wirepulse.svc"])));
        // `*` turns off the Host check but never vouches for an Origin.
        let evil = headers(&[("origin", "https://evil.example"), ("host", "localhost:8080")]);
        assert!(!same_origin(&evil, &AllowedHosts::new(&["*"])));
    }

    #[test]
    fn websocket_origin_must_match_host() {
        let ok = |h: &[(&str, &str)]| same_origin(&headers(h), &AllowedHosts::default());
        // Non-browser clients send no Origin.
        assert!(ok(&[("host", "localhost:8080")]));
        assert!(ok(&[("origin", "http://localhost:8080"), ("host", "localhost:8080")]));
        assert!(ok(&[("origin", "https://WirePulse.example.com"), ("host", "wirepulse.example.com:443")]));
        assert!(ok(&[("origin", "http://[::1]:8080"), ("host", "[::1]:8080")]));
        // Behind a proxy that rewrites Host.
        assert!(ok(&[
            ("origin", "https://wp.example.com"),
            ("host", "wirepulse.wirepulse.svc:80"),
            ("x-forwarded-host", "wp.example.com"),
        ]));

        // Another site, or the same host on another port.
        assert!(!ok(&[("origin", "https://evil.example"), ("host", "localhost:8080")]));
        assert!(!ok(&[("origin", "http://localhost:3000"), ("host", "localhost:8080")]));
        assert!(!ok(&[("origin", "null"), ("host", "localhost:8080")]));
        assert!(!ok(&[("origin", "http://localhost:8080")]));
    }
}
