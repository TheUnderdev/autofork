//! `after` dependency resolution: turn a set of selected forks into an
//! acyclic dependency graph the executor can run with readiness counting.

use std::collections::HashMap;

/// A fork selected to fire at the current moment, as far as scheduling is
/// concerned.
pub trait Selected {
    fn name(&self) -> &str;
    /// The `after` dependencies' fork names.
    fn after(&self) -> Vec<&str>;
    /// The fork's declared ordering weight (`priority:`, z-index-like;
    /// default 0).
    fn priority(&self) -> i64 {
        0
    }
}

/// Resolve the `after` dependencies among the selected forks into per-node
/// dependency lists (`deps[i]` = indices that must finish before `i` runs).
///
/// A dependency that wasn't selected this moment is dropped (the dependent
/// just runs earlier); self-edges and duplicates are dropped. Cycles are
/// broken by clearing the unmet dependencies of one member per cycle (with a
/// warning), so the result is always acyclic: repeatedly peeling nodes whose
/// deps are all peeled consumes every node.
pub fn resolve_deps<S: Selected>(selected: &[S]) -> Vec<Vec<usize>> {
    let n = selected.len();
    let name_to_idx: HashMap<&str, usize> = selected
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name(), i))
        .collect();
    let mut deps: Vec<Vec<usize>> = selected
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mut d: Vec<usize> = s
                .after()
                .iter()
                .filter_map(|a| name_to_idx.get(a).copied())
                .filter(|d| *d != i)
                .collect();
            d.dedup();
            d
        })
        .collect();

    loop {
        // Peel closure: a node is runnable iff all its deps are runnable.
        let mut done = vec![false; n];
        loop {
            let mut changed = false;
            for i in 0..n {
                if !done[i] && deps[i].iter().all(|d| done[*d]) {
                    done[i] = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        if done.iter().all(|d| *d) {
            break;
        }
        // Every unrunnable component contains a cycle: free one member by
        // dropping its unmet edges (edges to already-runnable nodes are kept).
        let cut = (0..n).find(|i| !done[*i]).unwrap();
        tracing::warn!(
            fork = %selected[cut].name(),
            "fork after cycle detected; dropping this fork's unmet dependencies"
        );
        deps[cut].retain(|d| done[*d]);
    }
    deps
}

/// Root nodes (no dependencies) of a resolved graph.
pub fn roots(deps: &[Vec<usize>]) -> Vec<usize> {
    (0..deps.len()).filter(|i| deps[*i].is_empty()).collect()
}

/// Effective priorities under the "`after` wins over `priority`" rule: each
/// node's declared priority is lifted to at least the effective priority of
/// every `after` predecessor, so a dependent can never end up in an earlier
/// wave than something it must run after. `deps` must be the acyclic graph
/// from [`resolve_deps`]. Execution then proceeds in ascending effective-
/// priority waves (equal priorities in parallel), with `after` edges still
/// sequencing nodes inside a wave.
pub fn effective_priorities<S: Selected>(selected: &[S], deps: &[Vec<usize>]) -> Vec<i64> {
    let n = selected.len();
    let mut eff: Vec<i64> = selected.iter().map(|s| s.priority()).collect();
    let mut done = vec![false; n];
    let mut resolved = 0usize;
    while resolved < n {
        let mut progressed = false;
        for i in 0..n {
            if done[i] || !deps[i].iter().all(|d| done[*d]) {
                continue;
            }
            for &d in &deps[i] {
                eff[i] = eff[i].max(eff[d]);
            }
            done[i] = true;
            resolved += 1;
            progressed = true;
        }
        // `deps` is acyclic by construction; this is just a safety valve.
        if !progressed {
            break;
        }
    }
    eff
}

#[cfg(test)]
mod tests {
    use super::*;

    struct S(&'static str, Vec<&'static str>);
    impl Selected for S {
        fn name(&self) -> &str {
            self.0
        }
        fn after(&self) -> Vec<&str> {
            self.1.clone()
        }
    }

    /// Simulated execution order: peel ready nodes in waves.
    fn waves(deps: &[Vec<usize>]) -> Vec<Vec<usize>> {
        let n = deps.len();
        let mut done = vec![false; n];
        let mut out = Vec::new();
        while done.iter().any(|d| !d) {
            let wave: Vec<usize> = (0..n)
                .filter(|i| !done[*i] && deps[*i].iter().all(|d| done[*d]))
                .collect();
            assert!(!wave.is_empty(), "graph not acyclic: {deps:?}");
            for &i in &wave {
                done[i] = true;
            }
            out.push(wave);
        }
        out
    }

    #[test]
    fn chain_and_fanout() {
        // a <- b <- c, d independent, e also after a.
        let sel = vec![
            S("a", vec![]),
            S("b", vec!["a"]),
            S("c", vec!["b"]),
            S("d", vec![]),
            S("e", vec!["a"]),
        ];
        let deps = resolve_deps(&sel);
        assert_eq!(roots(&deps), vec![0, 3]);
        assert_eq!(waves(&deps), vec![vec![0, 3], vec![1, 4], vec![2]]);
    }

    #[test]
    fn diamond_multi_dependency() {
        // c waits for BOTH a and b; d waits for c and a.
        let sel = vec![
            S("a", vec![]),
            S("b", vec![]),
            S("c", vec!["a", "b"]),
            S("d", vec!["c", "a"]),
        ];
        let deps = resolve_deps(&sel);
        assert_eq!(deps[2], vec![0, 1]);
        assert_eq!(deps[3], vec![2, 0]);
        assert_eq!(waves(&deps), vec![vec![0, 1], vec![2], vec![3]]);
    }

    #[test]
    fn missing_and_self_deps_drop() {
        let sel = vec![S("a", vec!["ghost", "a", "b"]), S("b", vec![])];
        let deps = resolve_deps(&sel);
        assert_eq!(deps[0], vec![1]);
        assert!(deps[1].is_empty());
    }

    #[test]
    fn breaks_two_cycle_keeping_hanger() {
        // a <-> b, c hangs off b.
        let sel = vec![S("a", vec!["b"]), S("b", vec!["a"]), S("c", vec!["b"])];
        let deps = resolve_deps(&sel);
        let w = waves(&deps);
        // All three run; c still runs after b.
        let flat: Vec<usize> = w.iter().flatten().copied().collect();
        let pos = |x: usize| flat.iter().position(|i| *i == x).unwrap();
        assert_eq!(flat.len(), 3);
        assert!(pos(2) > pos(1), "c must run after b: {w:?}");
    }

    #[test]
    fn breaks_cycle_with_partial_deps() {
        // a and b cycle; both also depend on d (selected, acyclic). The cut
        // node keeps its edge to d.
        let sel = vec![
            S("d", vec![]),
            S("a", vec!["b", "d"]),
            S("b", vec!["a", "d"]),
        ];
        let deps = resolve_deps(&sel);
        let w = waves(&deps);
        assert_eq!(w[0], vec![0]);
        // Cut member kept its dep on d, so nothing runs before d.
        let flat: Vec<usize> = w.iter().flatten().copied().collect();
        assert_eq!(flat[0], 0);
        assert_eq!(flat.len(), 3);
    }

    #[test]
    fn three_cycle_terminates() {
        let sel = vec![
            S("x", vec!["x"]),
            S("a", vec!["c"]),
            S("b", vec!["a"]),
            S("c", vec!["b"]),
        ];
        let deps = resolve_deps(&sel);
        assert_eq!(waves(&deps).iter().flatten().count(), 4);
    }

    struct P(&'static str, Vec<&'static str>, i64);
    impl Selected for P {
        fn name(&self) -> &str {
            self.0
        }
        fn after(&self) -> Vec<&str> {
            self.1.clone()
        }
        fn priority(&self) -> i64 {
            self.2
        }
    }

    #[test]
    fn priority_defaults_to_zero() {
        let sel = [S("a", vec![])];
        assert_eq!(sel[0].priority(), 0);
    }

    #[test]
    fn effective_priority_passes_through_without_deps() {
        let sel = vec![
            P("a", vec![], 0),
            P("last", vec![], 100),
            P("first", vec![], -10),
        ];
        let deps = resolve_deps(&sel);
        assert_eq!(effective_priorities(&sel, &deps), vec![0, 100, -10]);
    }

    #[test]
    fn after_lifts_a_dependent_to_its_predecessor() {
        // b declares -5 but runs after a (0): lifted to 0. c hangs off b and
        // declares 10: stays 10 (already above).
        let sel = vec![
            P("a", vec![], 0),
            P("b", vec!["a"], -5),
            P("c", vec!["b"], 10),
        ];
        let deps = resolve_deps(&sel);
        assert_eq!(effective_priorities(&sel, &deps), vec![0, 0, 10]);
    }

    #[test]
    fn lift_propagates_through_chains() {
        // high (10) ← dep (0) ← deeper (-3): both lifted to 10.
        let sel = vec![
            P("high", vec![], 10),
            P("dep", vec!["high"], 0),
            P("deeper", vec!["dep"], -3),
        ];
        let deps = resolve_deps(&sel);
        assert_eq!(effective_priorities(&sel, &deps), vec![10, 10, 10]);
    }
}
