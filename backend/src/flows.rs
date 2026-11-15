//! Turns individual flows into per-edge rates.

use common::{Edge, FlowEvent, Verdict};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

/// Edges with no traffic for this long are removed from the map.
const EDGE_TTL: Duration = Duration::from_secs(300);
const MAX_PORTS: usize = 6;
/// Weight of the newest sample in the exponential moving average.
const ALPHA: f32 = 0.4;

struct Acc {
    fwd: u64,
    drop: u64,
    prev_fwd: u64,
    prev_drop: u64,
    rate_fwd: f32,
    rate_drop: f32,
    ports: BTreeSet<String>,
    last_seen: Instant,
}

pub struct FlowAggregator {
    edges: HashMap<(String, String), Acc>,
    /// Workload names Hubble reported for endpoints, used for pods the
    /// Kubernetes inventory does not know (yet, or any more).
    pub workloads: HashMap<String, String>,
    last_tick: Instant,
}

impl Default for FlowAggregator {
    fn default() -> Self {
        Self { edges: HashMap::new(), workloads: HashMap::new(), last_tick: Instant::now() }
    }
}

impl FlowAggregator {
    pub fn record(&mut self, ev: &FlowEvent) {
        // Replies would draw every connection twice, in both directions.
        if ev.is_reply {
            return;
        }
        if !ev.src_workload.is_empty() {
            self.workloads.insert(ev.source.clone(), ev.src_workload.clone());
        }
        if !ev.dst_workload.is_empty() {
            self.workloads.insert(ev.destination.clone(), ev.dst_workload.clone());
        }
        let acc = self.edges.entry((ev.source.clone(), ev.destination.clone())).or_insert_with(|| Acc {
            fwd: 0,
            drop: 0,
            prev_fwd: 0,
            prev_drop: 0,
            rate_fwd: 0.0,
            rate_drop: 0.0,
            ports: BTreeSet::new(),
            last_seen: Instant::now(),
        });
        match ev.verdict {
            Verdict::Dropped | Verdict::Error => acc.drop += 1,
            _ => acc.fwd += 1,
        }
        acc.last_seen = Instant::now();
        if ev.dst_port > 0 && acc.ports.len() < MAX_PORTS {
            acc.ports.insert(format!("{}/{}", ev.protocol, ev.dst_port));
        }
    }

    /// Updates the moving averages and drops expired edges. The averages
    /// assume evenly spaced samples, so only the publish loop calls this;
    /// reading the graph (`edges`) never does.
    pub fn tick(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32().max(0.05);
        self.last_tick = now;
        self.edges.retain(|_, a| now.duration_since(a.last_seen) < EDGE_TTL);

        for a in self.edges.values_mut() {
            let inst_f = (a.fwd - a.prev_fwd) as f32 / dt;
            let inst_d = (a.drop - a.prev_drop) as f32 / dt;
            a.prev_fwd = a.fwd;
            a.prev_drop = a.drop;
            a.rate_fwd = ALPHA * inst_f + (1.0 - ALPHA) * a.rate_fwd;
            a.rate_drop = ALPHA * inst_d + (1.0 - ALPHA) * a.rate_drop;
            // Snap tiny tails to zero so idle edges read as idle.
            if a.rate_fwd < 0.005 {
                a.rate_fwd = 0.0;
            }
            if a.rate_drop < 0.005 {
                a.rate_drop = 0.0;
            }
        }

        // Workload names are only looked up for endpoints that have edges;
        // forget the rest so pod churn doesn't grow this forever.
        let live: HashSet<&str> = self.edges.keys().flat_map(|(s, t)| [s.as_str(), t.as_str()]).collect();
        self.workloads.retain(|id, _| live.contains(id.as_str()));
    }

    /// The current edges, with the rates from the last `tick`.
    pub fn edges(&self) -> Vec<Edge> {
        let now = Instant::now();
        self.edges
            .iter()
            .map(|((s, t), a)| Edge {
                source: s.clone(),
                target: t.clone(),
                flows_per_sec: a.rate_fwd,
                drops_per_sec: a.rate_drop,
                total_flows: a.fwd,
                total_drops: a.drop,
                ports: a.ports.iter().cloned().collect(),
                idle_secs: now.duration_since(a.last_seen).as_secs() as u32,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(s: &str, d: &str, v: Verdict, reply: bool) -> FlowEvent {
        FlowEvent {
            source: s.into(),
            destination: d.into(),
            protocol: "TCP".into(),
            dst_port: 80,
            verdict: v,
            is_reply: reply,
            ..Default::default()
        }
    }

    #[test]
    fn counts_and_skips_replies() {
        let mut a = FlowAggregator::default();
        for _ in 0..10 {
            a.record(&ev("a/1", "b/1", Verdict::Forwarded, false));
            a.record(&ev("b/1", "a/1", Verdict::Forwarded, true));
        }
        a.record(&ev("a/1", "b/1", Verdict::Dropped, false));
        a.tick();
        let edges = a.edges();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].total_flows, 10);
        assert_eq!(edges[0].total_drops, 1);
        assert_eq!(edges[0].ports, vec!["TCP/80".to_string()]);
        assert!(edges[0].flows_per_sec > 0.0);
        // Reading the edges leaves the rates alone.
        let r0 = edges[0].flows_per_sec;
        assert_eq!(a.edges()[0].flows_per_sec, r0);
        // No new traffic: the rate decays.
        a.tick();
        assert!(a.edges()[0].flows_per_sec < r0);
    }

    #[test]
    fn forgets_workloads_with_their_edges() {
        let mut a = FlowAggregator::default();
        let mut e = ev("a/1", "b/1", Verdict::Forwarded, false);
        e.src_workload = "Deployment/a".into();
        e.dst_workload = "Deployment/b".into();
        a.record(&e);
        a.tick();
        assert_eq!(a.workloads.len(), 2);
        let Some(long_ago) = Instant::now().checked_sub(EDGE_TTL + Duration::from_secs(1)) else {
            return; // clock too close to boot to fake an old edge
        };
        for acc in a.edges.values_mut() {
            acc.last_seen = long_ago;
        }
        a.tick();
        assert!(a.edges().is_empty());
        assert!(a.workloads.is_empty());
    }
}
