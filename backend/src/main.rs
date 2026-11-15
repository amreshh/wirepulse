mod api;
mod config;
mod demo;
mod endpoints;
mod flows;
mod hosts;
mod hubble;
mod k8s;
mod policy;
mod proto;
mod state;

use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use tracing::{info, warn};

/// Live Cilium/Hubble UI: global service map, flows, policies and config.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Address to serve the UI and API on. There is no login, so the default
    /// only accepts connections from this machine; the container image
    /// listens on 0.0.0.0.
    #[arg(long, env = "WIREPULSE_LISTEN", default_value = "127.0.0.1:8080")]
    listen: SocketAddr,

    /// Host names the UI may be reached by, besides IP addresses and
    /// localhost (comma-separated; `*.example.com` for subdomains, `*` for
    /// any). Needed behind an Ingress; protects against DNS rebinding.
    #[arg(long, env = "WIREPULSE_ALLOWED_HOSTS", value_delimiter = ',')]
    allowed_hosts: Vec<String>,

    /// hubble-relay gRPC address (plaintext).
    #[arg(long, env = "HUBBLE_RELAY_ADDR", default_value = "http://hubble-relay.kube-system.svc.cluster.local:80")]
    hubble_relay: String,

    /// Namespace Cilium is installed in.
    #[arg(long, env = "CILIUM_NAMESPACE", default_value = "kube-system")]
    cilium_namespace: String,

    /// Name of the Cilium agent ConfigMap.
    #[arg(long, env = "CILIUM_CONFIGMAP", default_value = "cilium-config")]
    cilium_configmap: String,

    /// Directory with the built frontend (index.html + wasm).
    #[arg(long, env = "WIREPULSE_STATIC_DIR", default_value = "frontend/dist")]
    static_dir: PathBuf,

    /// Serve a synthetic cluster instead of connecting to Kubernetes/Hubble.
    #[arg(long, env = "WIREPULSE_DEMO")]
    demo: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=warn,kube=warn".into()),
        )
        .init();
    let args = Args::parse();
    let state = state::AppState::new(args.demo);

    if args.demo {
        info!("demo mode: serving a synthetic cluster");
        tokio::spawn(demo::run(state.clone()));
    } else {
        match kube::Client::try_default().await {
            Ok(client) => {
                tokio::spawn(k8s::run_inventory(client.clone(), state.clone()));
                tokio::spawn(policy::refresh_loop(client.clone(), state.clone()));
                tokio::spawn(endpoints::refresh_loop(client.clone(), state.clone()));
                tokio::spawn(config::refresh_loop(
                    client,
                    state.clone(),
                    args.cilium_namespace.clone(),
                    args.cilium_configmap.clone(),
                ));
            }
            Err(e) => {
                // Flows still work without the API server; pods then come from flows only.
                warn!(error = %e, "no Kubernetes client; policies, config and idle pods unavailable");
                state.update_status(|s| s.message = format!("kubernetes: {e}"));
            }
        }
        tokio::spawn(hubble::run(args.hubble_relay.clone(), state.clone()));
    }
    tokio::spawn(state.clone().publish_loop());

    if !args.static_dir.join("index.html").exists() {
        warn!(dir = %args.static_dir.display(), "frontend not found; build it with `trunk build --release` in frontend/");
    }
    let app = api::router(state, &args.static_dir, hosts::AllowedHosts::new(&args.allowed_hosts));
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    info!(addr = %args.listen, "listening");
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;
    Ok(())
}

/// Ctrl-C, or SIGTERM from Kubernetes. The container runs this binary as
/// PID 1, which the kernel shields from signals it has no handler for, so
/// without one a pod would sit out its whole grace period and be killed.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                warn!(error = %e, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    info!("shutting down");
}
