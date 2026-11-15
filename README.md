# wirepulse

A Hubble UI replacement written in Rust: an axum backend plus a Leptos
frontend compiled to WebAssembly. It is read-only and keeps everything in
memory; nothing is stored.

- **Global map**: every namespace and pod in the cluster on one canvas, at
  three zoom levels (namespaces, workloads, pods). Connections show live
  Hubble flow rates; drops are red; traffic to and from outside the cluster
  (world, host, kube-apiserver) is grouped separately. Each node carries an
  icon for what it is (Deployment, StatefulSet, DaemonSet, Job, CronJob,
  standalone Pod, world, host, API server). Click a node for its connections,
  ports, rates and the policies that select it.
- **View options** (View menu, remembered per browser):
  - presets *Bubbles*, *Boxes*, *Cards* (Hubble-style) and *Flow*
  - nodes as circles or cards; namespaces as circles or boxes
  - packed, or left → right following the direction of traffic
  - curved, straight or right-angle edges; labels auto/always/off
  - **3D**: *Map* (namespaces on the ground, traffic arcing over them) or
    *Tower* (one floor per namespace); drag to orbit, Shift/right-drag to
    pan, scroll or pinch to zoom, optional auto-rotate
- **Traffic panel** under the map (resizable, collapsible), following the
  current selection or highlighted policy:
  - *Flows*: choose columns (source/destination with IP, port and
    identity, TCP flags, L7, latency, verdict and drop reason, direction,
    observation point, policy, node); click a row for the full flow details
    (labels, services, DNS answers, HTTP status, UUID, ...)
  - *Connections*: per-connection flow and drop rates, totals, last seen
- **Flows tab**: the same table full-screen, with the policy that allowed or
  denied each flow (when your Cilium version reports it).
- **Filters** (one filter bar shared by the map, traffic panel, Flows and
  Endpoints): `label=app=web`, `pod=web`, `ns=shop`, `workload=web`,
  `ip=10.0.0.0/8`, `dns=*.stripe.com`, `identity=12345`, `port=5432`,
  `protocol=udp`, `verdict=dropped`, `node=…`, `policy=…`, or a plain word.
  Prefix with `from:`/`to:` for one side and `!` to exclude. Several terms of
  one kind match any of them; different kinds must all match. Suggestions
  come from live data, and clicking a label anywhere adds it. The map hides
  what doesn't match (switchable in the View menu).
- **Endpoints tab**: every Cilium endpoint (`CiliumEndpoint`, or
  `CiliumEndpointSlice` when slices are enabled) with endpoint ID, security
  identity, identity labels, IPv4/IPv6, node, state, ingress/egress policy
  enforcement and live traffic; sortable, with a details panel showing all
  labels and the object's YAML.
- **Policies**: every `CiliumNetworkPolicy`, `CiliumClusterwideNetworkPolicy`
  and `NetworkPolicy`, with rule counts, the pods each one selects right now,
  and the YAML. "Show on map" highlights the selected pods.
- **Config**: the `cilium-config` ConfigMap, searchable and grouped by topic,
  with a summary of the key settings and any `CiliumNodeConfig` overrides.

Works on phones too (pinch to zoom, bottom-sheet details).

## Try it without a cluster

```sh
rustup target add wasm32-unknown-unknown
cargo install trunk --locked

(cd frontend && trunk build --release)
cargo run --release -p backend -- --demo
# open http://localhost:8080
```

`--demo` runs a synthetic cluster (6 namespaces, ~24 pods, policies, config,
rolling restarts) through the same code paths as real data.

## Run against your cluster

Locally, with your kubeconfig and a port-forward to hubble-relay:

```sh
kubectl -n kube-system port-forward svc/hubble-relay 4245:80
cargo run --release -p backend -- --hubble-relay http://127.0.0.1:4245
```

In the cluster:

```sh
docker build -t <registry>/wirepulse:0.1.0 .
docker push <registry>/wirepulse:0.1.0
# set the image in deploy/wirepulse.yaml, then
kubectl apply -f deploy/wirepulse.yaml
kubectl -n wirepulse port-forward svc/wirepulse 8080:80
```

### Settings

| Flag | Env | Default |
|---|---|---|
| `--hubble-relay` | `HUBBLE_RELAY_ADDR` | `http://hubble-relay.kube-system.svc.cluster.local:80` |
| `--cilium-namespace` | `CILIUM_NAMESPACE` | `kube-system` |
| `--cilium-configmap` | `CILIUM_CONFIGMAP` | `cilium-config` |
| `--listen` | `WIREPULSE_LISTEN` | `127.0.0.1:8080` (container image: `0.0.0.0:8080`) |
| `--allowed-hosts` | `WIREPULSE_ALLOWED_HOSTS` | IP addresses and `localhost` |
| `--static-dir` | `WIREPULSE_STATIC_DIR` | `frontend/dist` |
| `--demo` | `WIREPULSE_DEMO` | off |

Logging uses `RUST_LOG` (default `info,tower_http=warn,kube=warn`).

### Access

wirepulse has no login: anyone who can reach it sees your cluster's
traffic. `kubectl port-forward` keeps it on your machine and needs no
settings. Behind an Ingress:

- list the name(s) it is served under, e.g.
  `WIREPULSE_ALLOWED_HOSTS=wirepulse.example.com` (`*.example.com` for
  subdomains). wirepulse refuses requests for other host names, so a web page
  cannot point its own domain at wirepulse (DNS rebinding) and read it;
- put authentication in front of it, e.g. oauth2-proxy or your Ingress
  controller's auth.

The live stream (`/api/ws`) also refuses connections opened by pages from
other sites.

### Permissions

All read-only (see `deploy/wirepulse.yaml`):

- `pods`, `namespaces`: get/list/watch (the map inventory)
- `ciliumnetworkpolicies`, `ciliumclusterwidenetworkpolicies`,
  `networkpolicies`, `ciliumnodeconfigs`, `ciliumendpoints`,
  `ciliumendpointslices`: get/list
- the single `kube-system/cilium-config` ConfigMap: get

If Kubernetes access fails, the map still works from flows alone.

## How it works

```
hubble-relay ──gRPC GetFlows(follow)──┐
                                      ▼
Kubernetes API ──watch/list──▶  backend (axum, tonic, kube-rs)
                                  │ per-edge flow rates (EWMA)
                                  │ policy → pod selection
                                  ▼
                       WebSocket: graph every 2 s, flows every 0.5 s
                       REST: /api/policies /api/config /api/endpoints
                             /api/flows/{id} /api/info
                                  ▼
                    frontend (Leptos → WASM, canvas renderer)
```

```
common/    shared types + pure logic, unit-tested once, used on both sides:
           aggregation (pod → workload → namespace), label-selector matching,
           deterministic layouts, the filter language (filter.rs)
backend/   hubble.rs (relay client), k8s.rs (reflectors), policy.rs,
           endpoints.rs, config.rs, flows.rs (rates), state.rs, api.rs, demo.rs
           proto/ = vendored Cilium v1.19.0 Hubble protos (compiled with
           protox, so no protoc needed)
frontend/  Leptos app; scene.rs is the canvas renderer and pointer handling,
           scene/three.rs the 3D camera, icons.rs the workload icons,
           settings.rs the saved view preferences
```

Design notes:

- **Stable layout.** Namespaces are circles packed in name order and pods sit
  on a sunflower spiral inside them. The picture doesn't jump around as flows
  arrive, and a new pod only moves its own namespace. Rows adapt to the
  screen's aspect ratio.
- **Rates, not logs.** Each connection keeps a smoothed flows/s and drops/s
  rate. Reply flows are skipped so every connection is drawn once. Idle
  connections fade after 5 minutes; connections to deleted pods after 30 s.
- **Details on demand.** The live stream carries a compact row per flow; the
  backend keeps the last 5,000 full records in memory for the details panel
  (`/api/flows/{id}`).
- **3D without WebGL.** The 3D view projects the same layout through an orbit
  camera onto the 2D canvas, so it shares selection, search, filters and
  icons with the flat map.
- **Busy clusters.** The backend counts every flow but sends at most 400 per
  half-second to the UI. The canvas drops to ~25 fps above 1,500 connections
  and stops animating above 3,000.

## Development

```sh
cargo test -p common -p backend          # unit tests + fake hubble-relay test
cargo run -p backend -- --demo           # API on :8080
(cd frontend && trunk serve)             # UI on :8081 with hot reload, proxies /api
```

## Not done yet

- **Byte rates per pod**: Hubble flows carry no byte counts. Planned: read
  kubelet stats or VictoriaMetrics/Prometheus
  (`container_network_*_bytes_total`) and show them on the nodes.
- **TLS to hubble-relay**: plaintext only for now (the Helm default).
- **What each agent actually enforces**: policies are shown as defined in
  Kubernetes; per-endpoint enforcement would need the Cilium agent API.
- **Multi-cluster** (ClusterMesh) views.
