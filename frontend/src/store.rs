//! Flows kept in the browser.
//!
//! The server streams every flow in the cluster. In a plain "newest N" buffer
//! the rest of the cluster would push a quiet pod's flows out within seconds,
//! so the store keeps up to three pools, each of the newest `limit` flows of
//! its kind:
//!
//! - every flow;
//! - with a filter, the flows that pass it;
//! - with a node selected on the map (or a policy highlighted), the flows
//!   that also involve it, for the traffic panel.
//!
//! Only flows of the same pool push its oldest ones out, so a filtered table
//! keeps its flows however busy the rest of the cluster is. Each flow is
//! checked once, as it arrives. When the filter or the selection changes, the
//! pool picks again from the flows it holds and the newest flows.

use crate::app::EpIndex;
use crate::flows::{flow_matches, Scope};
use common::filter::Filter;
use common::FlowEvent;
use std::collections::VecDeque;
use std::sync::Arc;

/// One flow, shared by every pool and table that holds it.
pub type Flow = Arc<FlowEvent>;

/// The "Keep" setting's choices: flows per pool.
pub const KEEP_CHOICES: [usize; 5] = [1_000, 5_000, 10_000, 25_000, 50_000];
pub const DEFAULT_KEEP: usize = 5_000;

#[derive(Default)]
pub struct FlowStore {
    /// Every pool is newest first (flows arrive in id order).
    all: VecDeque<Flow>,
    /// Flows that pass `filter`. While there is no filter it's left as it
    /// is, so its flows are still there when a filter is set again.
    matching: VecDeque<Flow>,
    /// Flows that pass `filter` and lie in `scope`; left alone the same way.
    scoped: VecDeque<Flow>,
    filter: Filter,
    scope: Option<Scope>,
    limit: usize,
    /// Highest flow id taken. The server numbers its flows in order, so
    /// anything at or below it is a repeat.
    newest: u64,
    /// The server run the ids come from (`Status::instance`); 0 until known.
    instance: u64,
    /// The server restarted while the tables were paused: the pools hold the
    /// old run's ids, so they start over with the next flows.
    stale: bool,
}

impl FlowStore {
    pub fn new(limit: usize) -> Self {
        Self { limit: limit.max(1), ..Self::default() }
    }

    /// Every flow (the newest `limit`).
    pub fn all(&self) -> &VecDeque<Flow> {
        &self.all
    }

    /// The flows that pass the filter (every flow when there is none).
    pub fn filtered(&self) -> &VecDeque<Flow> {
        if self.filter.is_empty() {
            &self.all
        } else {
            &self.matching
        }
    }

    /// The filtered flows in the selected part of the map.
    pub fn scoped(&self) -> &VecDeque<Flow> {
        if self.scope.is_some() {
            &self.scoped
        } else {
            self.filtered()
        }
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn instance(&self) -> u64 {
        self.instance
    }

    /// Where a reconnect can carry on from: the server run, and the newest
    /// flow id this page has from it.
    pub fn resume_point(&self) -> Option<(u64, u64)> {
        (self.instance != 0 && self.newest > 0 && !self.stale).then_some((self.instance, self.newest))
    }

    /// Takes flows from the server, oldest first, skipping repeats.
    pub fn push(&mut self, batch: Vec<FlowEvent>, ix: &EpIndex) {
        if self.stale {
            self.restart();
        }
        for f in batch {
            if f.id <= self.newest {
                continue;
            }
            self.newest = f.id;
            let f = Arc::new(f);
            let filtered = self.filter.is_empty() || flow_matches(&self.filter, ix, &f);
            if filtered && !self.filter.is_empty() {
                push_front(&mut self.matching, f.clone(), self.limit);
            }
            if filtered && self.scope.as_ref().is_some_and(|s| s.contains(&f)) {
                push_front(&mut self.scoped, f.clone(), self.limit);
            }
            push_front(&mut self.all, f, self.limit);
        }
    }

    /// Flows older than the newest one here (from `/api/flows`, oldest
    /// first), after the limit went up: they fill the pools they belong to.
    pub fn backfill(&mut self, flows: Vec<FlowEvent>, ix: &EpIndex) {
        if self.stale {
            return;
        }
        // Newer ones come through the stream; taking them here too would
        // make them repeats there.
        let older: VecDeque<Flow> = flows.into_iter().rev().filter(|f| f.id <= self.newest).map(Arc::new).collect();
        self.all = pick(&self.all, &older, self.limit, |_, _, _| true);
        let filter = &self.filter;
        let passes = |f: &Flow| filter.is_empty() || flow_matches(filter, ix, f);
        if !filter.is_empty() {
            self.matching = pick(&self.matching, &older, self.limit, |f, kept, _| kept || passes(f));
        }
        if let Some(scope) = &self.scope {
            self.scoped = pick(&self.scoped, &older, self.limit, |f, kept, _| kept || (scope.contains(f) && passes(f)));
        }
    }

    /// Applies the filter. A new one makes the pools pick again from the
    /// flows they hold and the newest ones; the same one with new labels
    /// (`ix`) only adds the flows that match now.
    pub fn set_filter(&mut self, filter: Filter, ix: &EpIndex) {
        let same = filter == self.filter;
        self.filter = filter;
        let f = &self.filter;
        if !f.is_empty() {
            self.matching =
                pick(&self.matching, &self.all, self.limit, |e, kept, _| (same && kept) || flow_matches(f, ix, e));
        }
        self.repick_scope(!same, ix);
    }

    /// Follows the traffic panel's selection. Another node or connection
    /// makes the scoped pool pick again; the same one with other pods (the
    /// map changed) only adds flows.
    pub fn set_scope(&mut self, scope: Option<Scope>, ix: &EpIndex) {
        let same = matches!((&self.scope, &scope), (Some(a), Some(b)) if a.key == b.key);
        self.scope = scope;
        self.repick_scope(!same, ix);
    }

    /// Picks the scoped pool from the flows it holds and the filtered pool.
    /// `recheck`: whether the flows it holds have to match again.
    fn repick_scope(&mut self, recheck: bool, ix: &EpIndex) {
        let Some(scope) = &self.scope else { return };
        let filter = &self.filter;
        let base = if filter.is_empty() { &self.all } else { &self.matching };
        self.scoped = pick(&self.scoped, base, self.limit, |f, kept, filtered| {
            if kept && !recheck {
                return true;
            }
            scope.contains(f) && (filtered || filter.is_empty() || flow_matches(filter, ix, f))
        });
    }

    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit.max(1);
        for pool in [&mut self.all, &mut self.matching, &mut self.scoped] {
            pool.truncate(self.limit);
        }
    }

    /// Empties the pools. Flows already seen still count as repeats.
    pub fn clear(&mut self) {
        self.all.clear();
        self.matching.clear();
        self.scoped.clear();
    }

    fn restart(&mut self) {
        self.clear();
        self.newest = 0;
        self.stale = false;
    }

    /// Every status names the server run. Ids start over in a new run, so
    /// the pools empty (when the tables resume, while they're paused).
    pub fn set_instance(&mut self, instance: u64, paused: bool) {
        if instance == self.instance {
            return;
        }
        let known = self.instance != 0;
        self.instance = instance;
        if known {
            self.server_restarted(paused);
        }
    }

    /// The pools hold another run's ids.
    pub fn server_restarted(&mut self, paused: bool) {
        if paused {
            self.stale = true;
        } else {
            self.restart();
        }
    }

    /// Whether the page holds flows without knowing their server run: from
    /// a server that doesn't report one, so a reconnect has to start over.
    pub fn run_unknown(&self) -> bool {
        self.instance == 0 && self.newest > 0
    }
}

fn push_front(pool: &mut VecDeque<Flow>, f: Flow, limit: usize) {
    while pool.len() >= limit {
        pool.pop_back();
    }
    pool.push_front(f);
}

/// The flows of `a` and `b` (both newest first) that `keep` accepts: newest
/// first, each once, at most `limit`. `keep` is told whether the flow is in
/// `a` and whether it's in `b`.
fn pick(
    a: &VecDeque<Flow>,
    b: &VecDeque<Flow>,
    limit: usize,
    mut keep: impl FnMut(&Flow, bool, bool) -> bool,
) -> VecDeque<Flow> {
    let mut out = VecDeque::new();
    let (mut a, mut b) = (a.iter().peekable(), b.iter().peekable());
    while out.len() < limit {
        let (f, in_a, in_b) = match (a.peek(), b.peek()) {
            (Some(x), Some(y)) if x.id == y.id => {
                b.next();
                (a.next(), true, true)
            }
            (Some(x), Some(y)) if x.id > y.id => (a.next(), true, false),
            (_, Some(_)) => (b.next(), false, true),
            (Some(_), None) => (a.next(), true, false),
            (None, None) => break,
        };
        let Some(f) = f else { break };
        if keep(f, in_a, in_b) {
            out.push_back(f.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn flow(id: u64, src: &str, dst: &str) -> FlowEvent {
        FlowEvent { id, source: src.into(), destination: dst.into(), protocol: "TCP".into(), ..Default::default() }
    }

    /// `n` flows from `start` on: every tenth from test/busybox, the rest noise.
    fn traffic(start: u64, n: u64) -> Vec<FlowEvent> {
        (start..start + n)
            .map(|id| {
                if id % 10 == 0 {
                    flow(id, "test/busybox", "kube-system/coredns-1")
                } else {
                    flow(id, "shop/web-1", "shop/db-0")
                }
            })
            .collect()
    }

    fn ids(pool: &VecDeque<Flow>) -> Vec<u64> {
        pool.iter().map(|f| f.id).collect()
    }

    fn busybox() -> Scope {
        Scope { key: "pod/test/busybox".into(), label: "busybox".into(), ids: set(&["test/busybox"]), peer: None }
    }

    fn set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn filtered_flows_outlive_the_rest() {
        let ix = EpIndex::default();
        let mut s = FlowStore::new(20);
        s.set_filter(Filter::parse("pod=busybox"), &ix);
        s.push(traffic(1, 200), &ix);
        assert_eq!(s.all().len(), 20);
        assert_eq!(ids(s.all())[0], 200);
        // Ten times as many busybox flows are kept as a plain buffer would hold.
        assert_eq!(s.filtered().len(), 20);
        assert_eq!(ids(s.filtered()), (1..=20).rev().map(|i| i * 10).collect::<Vec<_>>());
        // Repeats (the stream overlapping the snapshot) are skipped.
        s.push(traffic(195, 10), &ix);
        assert_eq!(ids(s.all())[..3], [204, 203, 202]);
        assert_eq!(ids(s.filtered())[..2], [200, 190]);
    }

    #[test]
    fn a_new_filter_picks_from_what_is_kept() {
        let ix = EpIndex::default();
        let mut s = FlowStore::new(20);
        s.set_filter(Filter::parse("ns=test"), &ix);
        s.push(traffic(1, 200), &ix);
        // Narrower: the old matches are checked again, nothing is lost.
        s.set_filter(Filter::parse("pod=busybox port=0"), &ix);
        assert_eq!(s.filtered().len(), 20);
        // Unrelated: only what's in the newest flows.
        s.set_filter(Filter::parse("pod=web"), &ix);
        assert!(ids(s.filtered()).iter().all(|id| id % 10 != 0 && *id > 180));
        // No filter: every flow. The busybox flows are gone from the
        // matching pool, but that pool is kept until a filter returns.
        s.set_filter(Filter::default(), &ix);
        assert_eq!(s.filtered().len(), 20);
        s.set_filter(Filter::parse("pod=web"), &ix);
        assert_eq!(s.filtered().len(), 18);
    }

    #[test]
    fn same_filter_with_new_labels_only_adds() {
        let mut ix = EpIndex::default();
        let mut s = FlowStore::new(50);
        s.set_filter(Filter::parse("label=app=web"), &ix);
        s.push(traffic(1, 30), &ix);
        assert!(s.filtered().is_empty(), "no labels yet");
        ix.labels.insert("shop/web-1".into(), vec!["k8s:app=web".into()]);
        s.set_filter(Filter::parse("label=app=web"), &ix);
        assert_eq!(s.filtered().len(), 27);
        // The pod is gone and its labels with it: its flows stay.
        ix.labels.clear();
        s.set_filter(Filter::parse("label=app=web"), &ix);
        assert_eq!(s.filtered().len(), 27);
    }

    #[test]
    fn selection_keeps_its_own_flows() {
        let ix = EpIndex::default();
        let mut s = FlowStore::new(10);
        s.set_scope(Some(busybox()), &ix);
        s.push(traffic(1, 300), &ix);
        assert_eq!(ids(s.scoped()), (21..=30).rev().map(|i| i * 10).collect::<Vec<_>>());
        // Deselect, select again: the flows are still there.
        s.set_scope(None, &ix);
        assert_eq!(s.scoped().len(), 10);
        assert_eq!(ids(s.scoped())[0], 300);
        s.set_scope(Some(busybox()), &ix);
        assert_eq!(ids(s.scoped())[9], 210);
        // The same selection with more pods (the map changed): only adds.
        let mut more = busybox();
        more.ids.insert("shop/web-1".into());
        s.set_scope(Some(more), &ix);
        assert_eq!(ids(s.scoped())[..2], [300, 299]);
        // Another node: what's kept is checked against it.
        let web = Scope { key: "pod/shop/web-1".into(), label: "web-1".into(), ids: set(&["shop/web-1"]), peer: None };
        s.set_scope(Some(web), &ix);
        assert!(s.scoped().iter().all(|f| f.source == "shop/web-1"));
    }

    #[test]
    fn selection_follows_the_filter() {
        let ix = EpIndex::default();
        let mut s = FlowStore::new(10);
        s.set_scope(Some(busybox()), &ix);
        s.push(traffic(1, 100), &ix);
        assert_eq!(s.scoped().len(), 10);
        s.set_filter(Filter::parse("ns=shop"), &ix);
        assert!(s.scoped().is_empty(), "busybox flows don't pass ns=shop");
    }

    #[test]
    fn backfill_fills_in_older_flows() {
        let ix = EpIndex::default();
        let mut s = FlowStore::new(10);
        s.set_filter(Filter::parse("pod=busybox"), &ix);
        s.push(traffic(91, 10), &ix);
        assert_eq!(ids(s.filtered()), [100]);
        s.set_limit(50);
        // What /api/flows returns: older flows, repeats, and one the stream
        // hasn't delivered yet.
        s.backfill(traffic(51, 51), &ix);
        assert_eq!(ids(s.all())[0], 100, "newer flows come from the stream");
        assert_eq!(s.all().len(), 50);
        assert_eq!(ids(s.filtered()), [100, 90, 80, 70, 60]);
        s.push(traffic(101, 1), &ix);
        assert_eq!(ids(s.all())[..2], [101, 100]);
    }

    #[test]
    fn a_server_restart_starts_over() {
        let ix = EpIndex::default();
        let mut s = FlowStore::new(10);
        s.set_instance(7, false);
        s.push(traffic(1, 5), &ix);
        assert_eq!(s.resume_point(), Some((7, 5)));
        // The same run after a reconnect: carry on.
        s.set_instance(7, false);
        assert_eq!(s.all().len(), 5);
        // A new run while paused: kept until the tables resume.
        s.set_instance(8, true);
        assert_eq!(s.all().len(), 5);
        assert_eq!(s.resume_point(), None);
        s.push(traffic(1, 2), &ix);
        assert_eq!(ids(s.all()), [2, 1]);
        // A new run otherwise: start over at once.
        s.set_instance(9, false);
        assert!(s.all().is_empty());
        s.push(traffic(1, 1), &ix);
        assert_eq!(ids(s.all()), [1]);
    }

    #[test]
    fn lower_limit_trims() {
        let ix = EpIndex::default();
        let mut s = FlowStore::new(100);
        s.push(traffic(1, 100), &ix);
        s.set_limit(10);
        assert_eq!(ids(s.all())[9], 91);
        assert_eq!(s.all().len(), 10);
    }
}
