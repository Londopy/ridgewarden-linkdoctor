//! Firmware upgrade planning: "upgrade the far end first". A node restarts during an
//! upgrade and may come back broken, so a node is only upgraded once no site still
//! waiting for its upgrade depends on it to be reached from here. Going farthest first,
//! by hops on the live graph, does that: if every path from here to some site ran
//! through a node, that site would be farther away than the node.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use anyhow::Result;
use ridgewarden_client::graph::Graph;
use rusqlite::{Connection, params};

/// Each node's firmware from its latest successful poll since `since`.
pub fn firmware(conn: &Connection, since: i64) -> Result<BTreeMap<String, String>> {
    let mut st = conn.prepare(
        "SELECT node, firmware, max(ts) FROM polls WHERE ok = 1 AND firmware IS NOT NULL
         AND ts > ?1 GROUP BY node",
    )?;
    let rows = st.query_map(params![since], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The numeric parts of a release version, "3.25.8.0" -> [3, 25, 8, 0]. Nightly builds
/// and anything else without a dot in a number are not releases.
pub fn release(v: &str) -> Option<Vec<u64>> {
    let parts: Option<Vec<u64>> = v.trim().split('.').map(|p| p.parse().ok()).collect();
    parts.filter(|p| p.len() >= 2)
}

/// The newest release among `versions`.
pub fn newest<'a>(versions: impl Iterator<Item = &'a String>) -> Option<&'a String> {
    versions
        .filter_map(|v| Some((release(v)?, v)))
        .max()
        .map(|(_, v)| v)
}

/// Whether a node on `have` is behind `target`. Unknown or nightly builds aren't
/// behind anything; the operator decides those by hand.
pub fn behind(have: &str, target: &str) -> bool {
    matches!((release(have), release(target)), (Some(h), Some(t)) if h < t)
}

/// One upgrade in the plan.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub site: String,
    pub hops: usize,
    /// sites out of reach from here while it restarts (all already upgraded, or not in
    /// the plan)
    pub dark: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub steps: Vec<Step>,
    /// sites in `todo` that can't be reached from here now
    pub unreachable: BTreeSet<String>,
}

fn hops(g: &Graph, from: &str) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::from([(from.to_string(), 0)]);
    let mut q = VecDeque::from([from.to_string()]);
    while let Some(u) = q.pop_front() {
        let d = out[&u];
        for v in g.adj.get(&u).into_iter().flatten() {
            if !out.contains_key(v) {
                out.insert(v.clone(), d + 1);
                q.push_back(v.clone());
            }
        }
    }
    out
}

/// The order to upgrade `todo` from `here`, farthest first, `here` itself last.
pub fn plan(g: &Graph, here: &str, todo: &BTreeSet<String>) -> Plan {
    let dist = hops(g, here);
    let mut steps: Vec<Step> = todo
        .iter()
        .filter_map(|s| {
            Some(Step {
                site: s.clone(),
                hops: *dist.get(s)?,
                dark: BTreeSet::new(),
            })
        })
        .collect();
    steps.sort_by(|a, b| b.hops.cmp(&a.hops).then_with(|| a.site.cmp(&b.site)));
    let (no_nodes, no_edges) = (BTreeSet::new(), BTreeSet::new());
    let already = g.what_if(&no_nodes, &no_edges, Some(here));
    for s in steps.iter_mut().filter(|s| s.site != here) {
        s.dark = g
            .what_if(&[s.site.clone()].into(), &no_edges, Some(here))
            .difference(&already)
            .cloned()
            .collect();
    }
    Plan {
        unreachable: todo
            .iter()
            .filter(|s| !dist.contains_key(*s))
            .cloned()
            .collect(),
        steps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(xs: &[&str]) -> BTreeSet<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    // here - hub - mid - far, a second path hub - alt - far, a leaf off mid, and an
    // island linked to nothing
    fn mesh() -> Graph {
        let mut g = Graph::default();
        for (a, b) in [
            ("here", "hub"),
            ("hub", "mid"),
            ("mid", "far"),
            ("hub", "alt"),
            ("alt", "far"),
            ("mid", "leaf"),
        ] {
            g.add_edge(a, b);
        }
        g.add_node("island");
        g
    }

    #[test]
    fn far_end_first_and_nothing_waiting_is_ever_cut_off() {
        let g = mesh();
        let todo = set(&["here", "hub", "mid", "far", "leaf", "alt", "island"]);
        let p = plan(&g, "here", &todo);
        let order: Vec<&str> = p.steps.iter().map(|s| s.site.as_str()).collect();
        assert_eq!(order, ["far", "leaf", "alt", "mid", "hub", "here"]);
        assert_eq!(p.unreachable, set(&["island"]));
        let mut waiting = todo.clone();
        for s in &p.steps {
            waiting.remove(&s.site);
            assert!(
                s.dark.is_disjoint(&waiting),
                "{} cuts off {:?}",
                s.site,
                s.dark
            );
        }
        let hub = p.steps.iter().find(|s| s.site == "hub").unwrap();
        assert!(hub.dark.contains("far") && hub.dark.contains("leaf"));
    }

    #[test]
    fn versions_compare_as_numbers_and_nightlies_are_left_alone() {
        let v: Vec<String> = ["3.25.8.0", "3.25.10.0", "20261101-8e2a4f", "4.26.7.0"]
            .map(String::from)
            .to_vec();
        assert_eq!(newest(v.iter()).map(String::as_str), Some("4.26.7.0"));
        assert!(behind("3.25.10.0", "4.26.7.0"));
        assert!(behind("3.25.8.0", "3.25.10.0"));
        assert!(!behind("4.26.7.0", "4.26.7.0"));
        assert!(!behind("20261101-8e2a4f", "4.26.7.0"));
    }
}
