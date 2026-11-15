//! Which host names the server answers to (protection against DNS rebinding).
//!
//! A page on an attacker's domain can make that domain resolve to 127.0.0.1,
//! or to any other address the visitor's browser can reach, and then read
//! this API as if it were same-origin. Its requests still carry the
//! attacker's name in `Host`, so answering only to known names closes that.
//!
//! Always allowed: IP addresses (a page can only have an IP origin if it is
//! already that origin), `localhost` and `*.localhost`. That covers
//! `kubectl port-forward`, a local `cargo run` and kubelet probes. Anything
//! else, such as an Ingress host name, must be listed with `--allowed-hosts`.

use std::net::IpAddr;

#[derive(Clone, Debug, Default)]
pub struct AllowedHosts {
    /// `*` was listed: answer to any name (rebinding protection off).
    any: bool,
    /// Lowercase names without a trailing dot: `example.com` or `*.example.com`.
    names: Vec<String>,
}

impl AllowedHosts {
    pub fn new<S: AsRef<str>>(entries: &[S]) -> Self {
        let mut hosts = Self::default();
        for e in entries {
            let e = e.as_ref().trim().trim_end_matches('.').to_ascii_lowercase();
            match e.as_str() {
                "" => {}
                "*" => hosts.any = true,
                _ => hosts.names.push(e),
            }
        }
        hosts
    }

    /// Whether to answer a request whose `Host` header is `host` (which may
    /// include a port).
    pub fn allows(&self, host: &str) -> bool {
        if self.any {
            return true;
        }
        let name = normalize(strip_port(host));
        let bare = name
            .strip_prefix('[')
            .and_then(|n| n.strip_suffix(']'))
            .unwrap_or(&name);
        bare.parse::<IpAddr>().is_ok()
            || name == "localhost"
            || name.ends_with(".localhost")
            || self.lists(&name)
    }

    /// Whether `name` (no port) matches a name given with `--allowed-hosts`.
    /// The built-in names and `*` don't count: the WebSocket Origin check
    /// uses this to trust the public name of a proxy, nothing broader.
    pub fn lists(&self, name: &str) -> bool {
        let name = normalize(name);
        self.names.iter().any(|n| match n.strip_prefix("*.") {
            Some(domain) => name
                .strip_suffix(domain)
                .is_some_and(|sub| sub.len() > 1 && sub.ends_with('.')),
            None => *n == name,
        })
    }
}

fn normalize(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// `example.com:8080` -> `example.com`, `[::1]:8080` -> `[::1]`.
pub fn strip_port(host: &str) -> &str {
    let host = host.trim();
    if host.starts_with('[') {
        return host.find(']').map_or(host, |i| &host[..=i]);
    }
    match host.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_names() {
        let h = AllowedHosts::default();
        for ok in ["localhost:8080", "LOCALHOST", "localhost.", "app.localhost:8081", "127.0.0.1:8080", "10.0.3.7", "[::1]:8080", "[fd00::5]"] {
            assert!(h.allows(ok), "{ok}");
        }
        for bad in ["evil.example", "evil.example:8080", "localhost.evil.example", "127.0.0.1.nip.io", "[evil.example]:80", ""] {
            assert!(!h.allows(bad), "{bad}");
        }
    }

    #[test]
    fn configured_names_and_wildcards() {
        let h = AllowedHosts::new(&["WirePulse.example.com", "*.lab.example", " "]);
        assert!(h.allows("wirepulse.example.com"));
        assert!(h.allows("wirepulse.example.com.:443"));
        assert!(h.allows("ui.lab.example"));
        assert!(h.allows("a.b.lab.example:8080"));
        assert!(!h.allows("lab.example"), "*.x matches subdomains only");
        assert!(!h.allows("evillab.example"));
        assert!(!h.allows("other.example.com"));
        assert!(h.lists("wirepulse.example.com"));
        assert!(!h.lists("localhost"), "built-in names are not listed");

        let any = AllowedHosts::new(&["*"]);
        assert!(any.allows("anything.example"));
        assert!(!any.lists("anything.example"), "* does not vouch for origins");
    }
}
