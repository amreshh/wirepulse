//! Workload icons: simple stroked paths on a 24×24 grid, drawn on the canvas
//! with `Path2D` and in HTML as inline SVG, so both always match.

/// Icon key for a node's `workload_kind`.
pub fn key(kind: &str) -> &'static str {
    match kind {
        "Deployment" | "ReplicaSet" | "ReplicationController" => "Deployment",
        "StatefulSet" => "StatefulSet",
        "DaemonSet" => "DaemonSet",
        "Job" => "Job",
        "CronJob" => "CronJob",
        "Namespace" => "Namespace",
        "world" => "world",
        // Kubernetes nodes have their role as their kind.
        "host" | "remote-node" | "node" | "control-plane" => "host",
        "kube-apiserver" => "kube-apiserver",
        "ingress" => "ingress",
        "health" => "health",
        "unmanaged" | "init" | "unknown" => "unknown",
        k if k.starts_with("identity") => "unknown",
        _ => "Pod",
    }
}

pub fn path(key: &str) -> &'static str {
    match key {
        // Two stacked boxes: replicated.
        "Deployment" => "M9 3h10a2 2 0 0 1 2 2v10 M5 7h10a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V9a2 2 0 0 1 2-2z",
        // Cylinder: state.
        "StatefulSet" => "M4 6c0-1.66 3.58-3 8-3s8 1.34 8 3-3.58 3-8 3-8-1.34-8-3z M4 6v12c0 1.66 3.58 3 8 3s8-1.34 8-3V6 M4 12c0 1.66 3.58 3 8 3s8-1.34 8-3",
        // Grid of four: one per node.
        "DaemonSet" => "M4 4h6v6H4z M14 4h6v6h-6z M4 14h6v6H4z M14 14h6v6h-6z",
        // Circle with a check: runs to completion.
        "Job" => "M12 3a9 9 0 1 0 0 18a9 9 0 1 0 0-18z M8 12.5l2.8 2.8L16.5 9.5",
        // Clock: scheduled.
        "CronJob" => "M12 3a9 9 0 1 0 0 18a9 9 0 1 0 0-18z M12 7v5l3.5 2",
        // Corner brackets: a scope.
        "Namespace" => "M8 3H5a2 2 0 0 0-2 2v3 M16 3h3a2 2 0 0 1 2 2v3 M21 16v3a2 2 0 0 1-2 2h-3 M8 21H5a2 2 0 0 1-2-2v-3",
        "world" => "M12 3a9 9 0 1 0 0 18a9 9 0 1 0 0-18z M3 12h18 M12 3c2.5 2.6 3.8 5.6 3.8 9s-1.3 6.4-3.8 9c-2.5-2.6-3.8-5.6-3.8-9S9.5 5.6 12 3z",
        // Server rack.
        "host" => "M3 4h18v6H3z M3 14h18v6H3z M7 7h.01 M7 17h.01",
        // Ship's wheel.
        "kube-apiserver" => "M12 5a7 7 0 1 0 0 14a7 7 0 1 0 0-14z M12 2v6 M12 16v6 M2 12h6 M16 12h6 M4.9 4.9l4.2 4.2 M14.9 14.9l4.2 4.2 M4.9 19.1l4.2-4.2 M14.9 9.1l4.2-4.2",
        "ingress" => "M10 17l5-5-5-5 M15 12H3 M14 3h5a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2h-5",
        "health" => "M3 12h4l2-5 4 10 2-5h6",
        "unknown" => "M9.1 9a3 3 0 1 1 4.2 2.7c-.8.4-1.3 1.1-1.3 1.9V14 M12 18h.01",
        // Cube: a single pod.
        _ => "M12 2.5l8.5 4.75v9.5L12 21.5l-8.5-4.75v-9.5z M3.5 7.25L12 12l8.5-4.75 M12 12v9.5",
    }
}

/// Inline SVG markup for HTML (legend, tables, details).
pub fn svg(kind: &str) -> String {
    format!(
        r#"<svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="{}"/></svg>"#,
        path(key(kind))
    )
}

/// Legend entries: (icon key, label).
pub const LEGEND: &[(&str, &str)] = &[
    ("Deployment", "Deployment"),
    ("StatefulSet", "StatefulSet"),
    ("DaemonSet", "DaemonSet"),
    ("Job", "Job"),
    ("CronJob", "CronJob"),
    ("Pod", "Pod"),
    ("Namespace", "Namespace"),
    ("world", "world"),
    ("host", "host / node"),
    ("kube-apiserver", "API server"),
];
