//! Kubernetes / Cilium label selector matching.
//!
//! Cilium selectors may prefix keys with a label source (`k8s:app`,
//! `any:app`). Pods only carry `k8s` labels, so those two prefixes are
//! stripped and any other source (`reserved:`, `cidr:`, ...) never matches a pod.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelSelector {
    #[serde(default)]
    pub match_labels: BTreeMap<String, String>,
    #[serde(default)]
    pub match_expressions: Vec<Requirement>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Requirement {
    pub key: String,
    pub operator: String,
    #[serde(default)]
    pub values: Vec<String>,
}

/// Returns the plain label key, or `None` if the key targets a label source
/// that pods do not have.
fn normalize(key: &str) -> Option<&str> {
    match key.split_once(':') {
        Some(("k8s" | "any", rest)) => Some(rest),
        Some((src, _)) if !src.contains('.') && !src.contains('/') => None,
        _ => Some(key),
    }
}

impl LabelSelector {
    pub fn is_empty(&self) -> bool {
        self.match_labels.is_empty() && self.match_expressions.is_empty()
    }

    pub fn matches(&self, labels: &BTreeMap<String, String>) -> bool {
        for (k, v) in &self.match_labels {
            match normalize(k) {
                Some(k) if labels.get(k) == Some(v) => {}
                _ => return false,
            }
        }
        for r in &self.match_expressions {
            let Some(k) = normalize(&r.key) else {
                return false;
            };
            let val = labels.get(k);
            let ok = match r.operator.as_str() {
                "In" => val.is_some_and(|v| r.values.contains(v)),
                "NotIn" => !val.is_some_and(|v| r.values.contains(v)),
                "Exists" => val.is_some(),
                "DoesNotExist" => val.is_none(),
                _ => false,
            };
            if !ok {
                return false;
            }
        }
        true
    }

    /// `app=frontend, tier in (web, api)`; `(all)` when empty.
    pub fn describe(&self) -> String {
        if self.is_empty() {
            return "(all)".into();
        }
        let mut parts: Vec<String> = self.match_labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
        for r in &self.match_expressions {
            parts.push(match r.operator.as_str() {
                "Exists" => r.key.clone(),
                "DoesNotExist" => format!("!{}", r.key),
                op => format!("{} {} ({})", r.key, op.to_lowercase(), r.values.join(", ")),
            });
        }
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn empty_selects_all() {
        assert!(LabelSelector::default().matches(&labels(&[("a", "b")])));
    }

    #[test]
    fn match_labels_with_cilium_prefixes() {
        let s = LabelSelector {
            match_labels: [("k8s:app".to_string(), "web".to_string())].into(),
            match_expressions: vec![],
        };
        assert!(s.matches(&labels(&[("app", "web")])));
        assert!(!s.matches(&labels(&[("app", "db")])));
        let reserved = LabelSelector {
            match_labels: [("reserved:host".to_string(), String::new())].into(),
            match_expressions: vec![],
        };
        assert!(!reserved.matches(&labels(&[("host", "")])));
    }

    #[test]
    fn keys_with_domains_are_not_mistaken_for_sources() {
        let s = LabelSelector {
            match_labels: [("app.kubernetes.io/name".to_string(), "x".to_string())].into(),
            match_expressions: vec![],
        };
        assert!(s.matches(&labels(&[("app.kubernetes.io/name", "x")])));
    }

    #[test]
    fn expressions() {
        let s = LabelSelector {
            match_labels: BTreeMap::new(),
            match_expressions: vec![
                Requirement { key: "tier".into(), operator: "In".into(), values: vec!["web".into(), "api".into()] },
                Requirement { key: "legacy".into(), operator: "DoesNotExist".into(), values: vec![] },
            ],
        };
        assert!(s.matches(&labels(&[("tier", "api")])));
        assert!(!s.matches(&labels(&[("tier", "db")])));
        assert!(!s.matches(&labels(&[("tier", "web"), ("legacy", "1")])));
        assert_eq!(s.describe(), "tier in (web, api), !legacy");
    }
}
