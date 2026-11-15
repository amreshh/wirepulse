//! Hubble-style filter expressions.
//!
//! A filter is a space-separated list of terms:
//!
//! ```text
//! label=app=web  from:ns=shop  to:port=5432  !verdict=forwarded  dns=*.stripe.com  checkout
//! ```
//!
//! - `kind=value` where kind is one of `label`, `pod`, `ns`/`namespace`,
//!   `workload`, `ip`, `dns`/`fqdn`, `identity`, `port`, `protocol`/`proto`,
//!   `verdict`, `node`, `policy`.
//! - `from:` / `to:` (also `from-`/`to-`, as in the Hubble CLI) restrict an
//!   endpoint term to the source or the destination; without it either side
//!   may match.
//! - A leading `!` negates the term.
//! - A bare word matches names, IPs, DNS names and L7 text.
//! - Terms of the same kind and side are OR-ed (`pod=a pod=b`), different
//!   kinds are AND-ed, negated terms always apply.
//!
//! `label=` takes Cilium labels (`k8s:app=web`, `reserved:world`) or plain
//! Kubernetes ones (`app=web`); `label=app` only checks the key exists.

use serde::{Deserialize, Serialize};
use std::net::IpAddr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Either,
    From,
    To,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Kind {
    Label { key: String, value: Option<String> },
    Pod(String),
    Namespace(String),
    Workload(String),
    Ip(String),
    Dns(String),
    Identity(String),
    Port(u32),
    Protocol(String),
    Verdict(String),
    Node(String),
    Policy(String),
    Text(String),
}

impl Kind {
    /// Terms about one endpoint (as opposed to the whole flow).
    pub fn is_endpoint(&self) -> bool {
        matches!(
            self,
            Kind::Label { .. }
                | Kind::Pod(_)
                | Kind::Namespace(_)
                | Kind::Workload(_)
                | Kind::Ip(_)
                | Kind::Dns(_)
                | Kind::Identity(_)
        )
    }

    fn group(&self) -> &'static str {
        match self {
            Kind::Label { .. } => "label",
            Kind::Pod(_) => "pod",
            Kind::Namespace(_) => "ns",
            Kind::Workload(_) => "workload",
            Kind::Ip(_) => "ip",
            Kind::Dns(_) => "dns",
            Kind::Identity(_) => "identity",
            Kind::Port(_) => "port",
            Kind::Protocol(_) => "protocol",
            Kind::Verdict(_) => "verdict",
            Kind::Node(_) => "node",
            Kind::Policy(_) => "policy",
            Kind::Text(_) => "text",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Term {
    pub negate: bool,
    pub side: Side,
    pub kind: Kind,
    /// The text the term was parsed from, for chips in the UI.
    pub raw: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParseError {
    pub token: String,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filter {
    pub terms: Vec<Term>,
    pub errors: Vec<ParseError>,
}

/// Kinds accepted before `=`, for suggestions.
pub const KINDS: &[&str] = &[
    "label=", "from:label=", "to:label=", "pod=", "from:pod=", "to:pod=", "ns=", "from:ns=", "to:ns=",
    "workload=", "ip=", "dns=", "identity=", "port=", "protocol=", "verdict=", "node=", "policy=",
];

impl Filter {
    pub fn parse(input: &str) -> Filter {
        let mut f = Filter::default();
        for token in input.split_whitespace() {
            match parse_term(token) {
                Ok(t) => f.terms.push(t),
                Err(message) => f.errors.push(ParseError {
                    token: token.to_string(),
                    message,
                }),
            }
        }
        f
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Whether any term is about the flow rather than an endpoint.
    pub fn has_flow_terms(&self) -> bool {
        self.terms.iter().any(|t| !t.kind.is_endpoint())
    }

    /// Evaluates every term with `eval`, OR-ing positive terms within a
    /// (kind, side) group and AND-ing everything else. Without `by_side`,
    /// groups are by kind alone.
    fn combine(&self, by_side: bool, mut eval: impl FnMut(&Term) -> Option<bool>) -> bool {
        let mut groups: Vec<((&str, Side), bool)> = Vec::new();
        for t in &self.terms {
            // `None`: the term does not apply here (e.g. a port on an endpoint).
            let Some(hit) = eval(t) else { continue };
            if t.negate {
                if hit {
                    return false;
                }
                continue;
            }
            let key = (t.kind.group(), if by_side { t.side } else { Side::Either });
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, any)) => *any |= hit,
                None => groups.push((key, hit)),
            }
        }
        groups.iter().all(|(_, hit)| *hit)
    }

    pub fn matches_flow(&self, f: &FlowRef) -> bool {
        self.combine(true, |t| Some(term_matches_flow(t, f)))
    }

    /// For a single endpoint (endpoints table, idle nodes on the map): only
    /// endpoint terms and free text apply, and `from:`/`to:` are ignored, so
    /// `from:ns=a to:ns=b` matches endpoints in either namespace.
    pub fn matches_endpoint(&self, e: &EndpointRef) -> bool {
        self.combine(false, |t| match &t.kind {
            k if k.is_endpoint() => Some(endpoint_matches(k, e)),
            Kind::Text(s) => Some(text_matches_endpoint(s, e)),
            _ => None,
        })
    }
}

fn parse_term(token: &str) -> Result<Term, String> {
    let raw = token.to_string();
    let (negate, rest) = match token.strip_prefix('!') {
        Some(r) => (true, r),
        None => (false, token),
    };
    let (side, rest) = if let Some(r) = rest.strip_prefix("from:").or_else(|| rest.strip_prefix("from-")) {
        (Side::From, r)
    } else if let Some(r) = rest.strip_prefix("to:").or_else(|| rest.strip_prefix("to-")) {
        (Side::To, r)
    } else {
        (Side::Either, rest)
    };
    let Some((key, value)) = rest.split_once('=') else {
        if side != Side::Either {
            return Err("from:/to: needs a kind, e.g. from:pod=web".into());
        }
        if rest.is_empty() {
            return Err("empty term".into());
        }
        return Ok(Term {
            negate,
            side,
            kind: Kind::Text(rest.to_lowercase()),
            raw,
        });
    };
    if value.is_empty() {
        return Err(format!("{key}= needs a value"));
    }
    let v = value.to_string();
    let kind = match key.to_ascii_lowercase().as_str() {
        "label" | "labels" => match v.split_once('=') {
            Some((k, val)) => Kind::Label {
                key: k.to_string(),
                value: Some(val.to_string()),
            },
            None => Kind::Label { key: v, value: None },
        },
        "pod" => Kind::Pod(v),
        "ns" | "namespace" => Kind::Namespace(v),
        "workload" | "wl" => Kind::Workload(v),
        "ip" => {
            check_ip(&v)?;
            Kind::Ip(v)
        }
        "dns" | "fqdn" => Kind::Dns(v.to_lowercase()),
        "identity" | "id" => Kind::Identity(v),
        "port" => Kind::Port(v.parse().map_err(|_| format!("port must be a number, not {v:?}"))?),
        "protocol" | "proto" => Kind::Protocol(v.to_ascii_uppercase()),
        "verdict" => Kind::Verdict(v.to_ascii_lowercase()),
        "node" => Kind::Node(v),
        "policy" => Kind::Policy(v.to_lowercase()),
        other => return Err(format!("unknown filter {other:?}")),
    };
    let flow_level = !kind.is_endpoint();
    if side != Side::Either && flow_level {
        return Err(format!("{key} applies to the whole flow; drop from:/to:"));
    }
    Ok(Term { negate, side, kind, raw })
}

fn check_ip(v: &str) -> Result<(), String> {
    match v.split_once('/') {
        Some((ip, bits)) => {
            let ip: IpAddr = ip.parse().map_err(|_| format!("bad IP {ip:?}"))?;
            let bits: u8 = bits.parse().map_err(|_| format!("bad prefix length {bits:?}"))?;
            let max = if ip.is_ipv4() { 32 } else { 128 };
            if bits > max {
                return Err(format!("prefix length over {max}"));
            }
            Ok(())
        }
        None => v.parse::<IpAddr>().map(|_| ()).map_err(|_| format!("bad IP {v:?}")),
    }
}

fn ip_matches(pattern: &str, ip: &str) -> bool {
    let Ok(addr) = ip.parse::<IpAddr>() else {
        return false;
    };
    match pattern.split_once('/') {
        None => pattern.parse::<IpAddr>().is_ok_and(|p| p == addr),
        Some((net, bits)) => {
            let (Ok(net), Ok(bits)) = (net.parse::<IpAddr>(), bits.parse::<u32>()) else {
                return false;
            };
            match (net, addr) {
                (IpAddr::V4(n), IpAddr::V4(a)) => {
                    let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits.min(32)) };
                    u32::from(n) & mask == u32::from(a) & mask
                }
                (IpAddr::V6(n), IpAddr::V6(a)) => {
                    let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits.min(128)) };
                    u128::from(n) & mask == u128::from(a) & mask
                }
                _ => false,
            }
        }
    }
}

/// `*` matches any run of characters; otherwise an exact, case-insensitive match.
pub fn glob(pattern: &str, s: &str) -> bool {
    let p = pattern.to_lowercase();
    let s = s.to_lowercase();
    if !p.contains('*') {
        return p == s;
    }
    let parts: Vec<&str> = p.split('*').collect();
    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match s[pos..].find(part) {
            Some(at) if i > 0 || at == 0 => pos += at + part.len(),
            _ => return false,
        }
    }
    parts.last().is_none_or(|l| l.is_empty() || s.ends_with(l))
}

/// Splits a label into (source, key, value):
/// `k8s:app=web` -> (Some("k8s"), "app", Some("web")), `app` -> (None, "app", None).
pub fn split_label(label: &str) -> (Option<&str>, &str, Option<&str>) {
    let (lhs, value) = match label.split_once('=') {
        Some((l, v)) => (l, Some(v)),
        None => (label, None),
    };
    match lhs.split_once(':') {
        Some((src, key))
            if !src.is_empty() && src.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') =>
        {
            (Some(src), key, value)
        }
        _ => (None, lhs, value),
    }
}

fn label_matches(want_key: &str, want_value: Option<&str>, labels: &[String]) -> bool {
    let (want_src, want_key, _) = split_label(want_key);
    labels.iter().any(|l| {
        let (src, key, value) = split_label(l);
        let src_ok = match want_src {
            None | Some("any") => true,
            Some(w) => src == Some(w),
        };
        src_ok
            && key == want_key
            && match want_value {
                None => true,
                Some(w) => value.is_some_and(|v| glob(w, v)),
            }
    })
}

/// What a filter needs to know about one endpoint.
#[derive(Clone, Copy, Debug, Default)]
pub struct EndpointRef<'a> {
    /// `namespace/pod` or `reserved:<entity>`.
    pub id: &'a str,
    /// `Deployment/web`.
    pub workload: &'a str,
    pub ip: &'a str,
    pub identity: u32,
    pub labels: &'a [String],
    pub dns: &'a str,
}

impl EndpointRef<'_> {
    fn namespace(&self) -> &str {
        if self.id.starts_with("reserved:") {
            return "";
        }
        self.id.split_once('/').map_or("", |(ns, _)| ns)
    }

    fn pod(&self) -> &str {
        self.id.rsplit_once('/').map_or(self.id, |(_, p)| p)
    }
}

/// What a filter needs to know about a flow (or a map connection).
#[derive(Clone, Debug, Default)]
pub struct FlowRef<'a> {
    pub src: EndpointRef<'a>,
    pub dst: EndpointRef<'a>,
    /// (protocol, destination port) pairs; one for a flow, several for a
    /// connection on the map.
    pub ports: Vec<(&'a str, u32)>,
    pub forwarded: bool,
    pub dropped: bool,
    /// Exact verdict name for flows (`audit`, `redirected`, ...).
    pub verdict: &'a str,
    pub node: &'a str,
    pub policies: &'a [String],
    pub l7: &'a str,
}

fn endpoint_matches(kind: &Kind, e: &EndpointRef) -> bool {
    match kind {
        Kind::Label { key, value } => label_matches(key, value.as_deref(), e.labels),
        Kind::Pod(p) => {
            if p.contains('/') || p.starts_with("reserved:") {
                e.id.starts_with(p.as_str()) || glob(p, e.id)
            } else {
                e.pod().starts_with(p.as_str()) || glob(p, e.pod())
            }
        }
        Kind::Namespace(n) => glob(n, e.namespace()),
        Kind::Workload(w) => {
            let name = e.workload.rsplit_once('/').map_or(e.workload, |(_, n)| n);
            glob(w, e.workload) || glob(w, name)
        }
        Kind::Ip(p) => ip_matches(p, e.ip),
        Kind::Dns(d) => !e.dns.is_empty() && (glob(d, e.dns) || glob(d, e.dns.trim_end_matches('.'))),
        Kind::Identity(i) => e.identity.to_string() == *i,
        _ => false,
    }
}

fn text_matches_endpoint(s: &str, e: &EndpointRef) -> bool {
    e.id.to_lowercase().contains(s) || e.ip.contains(s) || e.dns.to_lowercase().contains(s) || e.workload.to_lowercase().contains(s)
}

fn term_matches_flow(t: &Term, f: &FlowRef) -> bool {
    match &t.kind {
        k if k.is_endpoint() => match t.side {
            Side::From => endpoint_matches(k, &f.src),
            Side::To => endpoint_matches(k, &f.dst),
            Side::Either => endpoint_matches(k, &f.src) || endpoint_matches(k, &f.dst),
        },
        Kind::Port(p) => f.ports.iter().any(|(_, port)| port == p),
        Kind::Protocol(p) => f.ports.iter().any(|(proto, _)| proto.eq_ignore_ascii_case(p)),
        Kind::Verdict(v) => match v.as_str() {
            "forwarded" => f.forwarded,
            "dropped" | "drop" | "denied" => f.dropped,
            other => f.verdict.eq_ignore_ascii_case(other),
        },
        Kind::Node(n) => glob(n, f.node) || f.node.contains(n.as_str()),
        Kind::Policy(p) => f.policies.iter().any(|x| x.to_lowercase().contains(p.as_str())),
        Kind::Text(s) => {
            text_matches_endpoint(s, &f.src) || text_matches_endpoint(s, &f.dst) || f.l7.to_lowercase().contains(s)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    fn flow<'a>(src: EndpointRef<'a>, dst: EndpointRef<'a>, port: u32, dropped: bool) -> FlowRef<'a> {
        FlowRef {
            src,
            dst,
            ports: vec![("TCP", port)],
            forwarded: !dropped,
            dropped,
            verdict: if dropped { "dropped" } else { "forwarded" },
            node: "worker-1",
            policies: &[],
            l7: "HTTP GET /api 200",
        }
    }

    #[test]
    fn parses_terms_and_errors() {
        let f = Filter::parse("label=app=web !from:ns=shop to-pod=db port=5432 junk=1 port=x hello");
        assert_eq!(f.terms.len(), 5);
        assert_eq!(f.errors.len(), 2);
        assert_eq!(
            f.terms[0].kind,
            Kind::Label {
                key: "app".into(),
                value: Some("web".into())
            }
        );
        assert!(f.terms[1].negate);
        assert_eq!(f.terms[1].side, Side::From);
        assert_eq!(f.terms[2].side, Side::To);
        assert_eq!(f.terms[4].kind, Kind::Text("hello".into()));
        assert!(Filter::parse("from:port=80").errors.len() == 1);
        assert!(Filter::parse("ip=10.0.0.0/33").errors.len() == 1);
    }

    #[test]
    fn labels_with_and_without_source() {
        let l = labels(&["k8s:app=web", "k8s:io.kubernetes.pod.namespace=shop", "reserved:world"]);
        assert!(label_matches("app", Some("web"), &l));
        assert!(label_matches("k8s:app", Some("web"), &l));
        assert!(label_matches("app", Some("w*"), &l));
        assert!(!label_matches("app", Some("db"), &l));
        assert!(label_matches("app", None, &l));
        assert!(label_matches("reserved:world", None, &l));
        assert!(!label_matches("reserved:app", None, &l));
        assert!(label_matches("io.kubernetes.pod.namespace", Some("shop"), &l));
        // Plain Kubernetes labels (no source).
        assert!(!label_matches("k8s:tier", Some("api"), &labels(&["tier=api"])));
        assert!(label_matches("tier", Some("api"), &labels(&["tier=api"])));
    }

    #[test]
    fn direction_negation_and_grouping() {
        let web_l = labels(&["k8s:app=web"]);
        let db_l = labels(&["k8s:app=db"]);
        let web = EndpointRef { id: "shop/web-1", workload: "Deployment/web", ip: "10.0.1.5", identity: 100, labels: &web_l, dns: "" };
        let db = EndpointRef { id: "pay/db-0", workload: "StatefulSet/db", ip: "10.0.2.9", identity: 200, labels: &db_l, dns: "" };
        let f = flow(web, db, 5432, false);

        let m = |s: &str| Filter::parse(s).matches_flow(&f);
        assert!(m(""));
        assert!(m("from:label=app=web"));
        assert!(!m("to:label=app=web"));
        assert!(m("label=app=db"));
        assert!(m("ns=shop"));
        assert!(m("from:ns=shop to:ns=pay"));
        assert!(!m("from:ns=pay"));
        assert!(m("pod=web pod=nothing"), "same kind is OR");
        assert!(!m("pod=web port=80"), "different kinds are AND");
        assert!(m("port=5432 protocol=tcp"));
        assert!(m("!verdict=dropped"));
        assert!(!m("verdict=dropped"));
        assert!(m("ip=10.0.0.0/16"));
        assert!(m("to:ip=10.0.2.9"));
        assert!(!m("from:ip=10.0.2.9"));
        assert!(m("identity=200"));
        assert!(m("workload=db"));
        assert!(m("workload=Deployment/web"));
        assert!(m("api"), "free text matches L7");
        assert!(!m("!pod=web"));
        assert!(m("pod=shop/web"));
    }

    #[test]
    fn dns_globs() {
        let none: Vec<String> = vec![];
        let world = EndpointRef { id: "reserved:world", dns: "api.stripe.com", labels: &none, ..Default::default() };
        let pod = EndpointRef { id: "pay/api-1", labels: &none, ..Default::default() };
        let f = flow(pod, world, 443, false);
        assert!(Filter::parse("dns=*.stripe.com").matches_flow(&f));
        assert!(Filter::parse("to:dns=api.stripe.com").matches_flow(&f));
        assert!(!Filter::parse("dns=*.example.com").matches_flow(&f));
        assert!(Filter::parse("pod=reserved:world").matches_flow(&f));
    }

    #[test]
    fn endpoint_matching_ignores_flow_terms() {
        let l = labels(&["k8s:app=web"]);
        let e = EndpointRef { id: "shop/web-1", labels: &l, ..Default::default() };
        assert!(Filter::parse("label=app=web port=80").matches_endpoint(&e));
        assert!(Filter::parse("from:ns=shop").matches_endpoint(&e));
        assert!(!Filter::parse("ns=pay").matches_endpoint(&e));
        // One endpoint is never on both sides: `from:`/`to:` collapse.
        assert!(Filter::parse("from:ns=shop to:ns=pay").matches_endpoint(&e));
        assert!(Filter::parse("from:ns=pay to:ns=shop").matches_endpoint(&e));
        assert!(!Filter::parse("from:ns=pay to:ns=web").matches_endpoint(&e));
        assert!(!Filter::parse("from:ns=shop !to:ns=shop").matches_endpoint(&e), "negation still applies");
        assert!(Filter::parse("web").matches_endpoint(&e));
    }

    #[test]
    fn globbing() {
        assert!(glob("*.stripe.com", "api.stripe.com"));
        assert!(glob("api.*", "api.stripe.com"));
        assert!(glob("a*c", "abc"));
        assert!(!glob("a*c", "abd"));
        assert!(glob("Web", "web"));
        assert!(!glob("web", "web-1"));
    }
}
