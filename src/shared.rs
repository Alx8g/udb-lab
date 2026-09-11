//! Shared maintained state vs per-query private copies.
//!
//! N parameterized range-count queries over the same key array.
//! Private: each query keeps its own matching list and rescans on update.
//! Shared: one sorted key array + prefix counts; each query is two bounds.

use crate::stats::{record, time_ns, Record};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde_json::json;

struct PrivateQuery {
    lo: u64,
    hi: u64,
    matches: Vec<u64>,
}

impl PrivateQuery {
    fn from_keys(keys: &[u64], lo: u64, hi: u64) -> Self {
        Self {
            lo,
            hi,
            matches: keys
                .iter()
                .copied()
                .filter(|&k| k >= lo && k < hi)
                .collect(),
        }
    }
    fn count(&self) -> usize {
        self.matches.len()
    }
    fn on_insert(&mut self, k: u64) {
        if k >= self.lo && k < self.hi {
            self.matches.push(k);
        }
    }
}

struct SharedIndex {
    keys: Vec<u64>,
}

impl SharedIndex {
    fn new(mut keys: Vec<u64>) -> Self {
        keys.sort_unstable();
        Self { keys }
    }
    fn count(&self, lo: u64, hi: u64) -> usize {
        let a = self.keys.binary_search(&lo).unwrap_or_else(|i| i);
        let b = self.keys.binary_search(&hi).unwrap_or_else(|i| i);
        b.saturating_sub(a)
    }
    fn insert(&mut self, k: u64) {
        let i = self.keys.binary_search(&k).unwrap_or_else(|i| i);
        self.keys.insert(i, k);
    }
}

pub fn correctness() -> Result<(), String> {
    let keys: Vec<u64> = (0..1_000).map(|i| i * 3).collect();
    let mut privs: Vec<PrivateQuery> = (0..20)
        .map(|q| {
            let lo = q * 50;
            PrivateQuery::from_keys(&keys, lo, lo + 200)
        })
        .collect();
    let mut shared = SharedIndex::new(keys.clone());
    for (q, p) in privs.iter().enumerate() {
        let lo = q as u64 * 50;
        if p.count() != shared.count(lo, lo + 200) {
            return Err("shared/private count mismatch".into());
        }
    }
    for k in [4u64, 250, 9000] {
        for p in &mut privs {
            p.on_insert(k);
        }
        shared.insert(k);
    }
    for (q, p) in privs.iter().enumerate() {
        let lo = q as u64 * 50;
        if p.count() != shared.count(lo, lo + 200) {
            return Err("after insert mismatch".into());
        }
    }
    eprintln!("shared-state correctness: private copies match shared index");
    Ok(())
}

pub fn run(quick: bool) -> Vec<Record> {
    let mut out = Vec::new();
    let n = if quick { 50_000usize } else { 500_000usize };
    let nq_list: Vec<usize> = if quick {
        vec![8, 64]
    } else {
        vec![8, 64, 256, 1024]
    };
    let mut rng = SmallRng::seed_from_u64(19);
    let keys: Vec<u64> = (0..n).map(|_| rng.gen_range(0..n as u64 * 4)).collect();

    for nq in nq_list {
        let bounds: Vec<(u64, u64)> = (0..nq)
            .map(|_| {
                let lo = rng.gen_range(0..n as u64 * 3);
                (lo, lo + 5_000)
            })
            .collect();

        let (val, times, reps) = time_ns(2, 6, || {
            bounds
                .iter()
                .map(|&(lo, hi)| keys.iter().filter(|&&k| k >= lo && k < hi).count())
                .sum::<usize>()
        });
        out.push(record(
            "shared_state",
            "rescan_each_query",
            n as u64,
            json!({"queries": nq}),
            times,
            reps,
            (n * nq) as u64,
            (n * nq * 8) as u64,
            json!({"sum": val}),
            "no shared state: scan the key array once per query",
        ));

        let privs: Vec<PrivateQuery> = bounds
            .iter()
            .map(|&(lo, hi)| PrivateQuery::from_keys(&keys, lo, hi))
            .collect();
        let bytes: u64 = privs.iter().map(|p| p.matches.len() as u64 * 8).sum();
        let (val, times, reps) = time_ns(3, 10, || privs.iter().map(|p| p.count()).sum::<usize>());
        out.push(record(
            "shared_state",
            "private_counts",
            n as u64,
            json!({"queries": nq, "retained_bytes": bytes}),
            times,
            reps,
            nq as u64,
            bytes,
            json!({"sum": val, "retained_bytes": bytes}),
            "each query stores its own match list (O(1) count, duplicated memory)",
        ));

        let shared = SharedIndex::new(keys.clone());
        let (val, times, reps) = time_ns(3, 10, || {
            bounds
                .iter()
                .map(|&(lo, hi)| shared.count(lo, hi))
                .sum::<usize>()
        });
        out.push(record(
            "shared_state",
            "shared_index_counts",
            n as u64,
            json!({"queries": nq, "index_bytes": n * 8}),
            times,
            reps,
            nq as u64,
            (nq * 64) as u64,
            json!({"sum": val, "index_bytes": n * 8}),
            "one sorted array serves every range",
        ));

        let insert_k = n as u64 * 2;
        let mut ps: Vec<PrivateQuery> = bounds
            .iter()
            .map(|&(lo, hi)| PrivateQuery::from_keys(&keys, lo, hi))
            .collect();
        let (val, times, reps) = time_ns(2, 8, || {
            for p in &mut ps {
                p.on_insert(insert_k);
            }
            let s = ps.iter().map(|p| p.count()).sum::<usize>();
            for p in &mut ps {
                if p.lo <= insert_k && insert_k < p.hi {
                    p.matches.pop();
                }
            }
            s
        });
        out.push(record(
            "shared_state",
            "private_insert",
            n as u64,
            json!({"queries": nq}),
            times,
            reps,
            nq as u64,
            nq as u64 * 8,
            json!({"sum": val}),
            "update fan-out = number of queries",
        ));

        let mut sidx = SharedIndex::new(keys.clone());
        let insert_at = sidx.keys.binary_search(&insert_k).unwrap_or_else(|i| i);
        let (val, times, reps) = time_ns(2, 8, || {
            sidx.insert(insert_k);
            let s = bounds
                .iter()
                .map(|&(lo, hi)| sidx.count(lo, hi))
                .sum::<usize>();
            sidx.keys.remove(insert_at);
            s
        });
        out.push(record(
            "shared_state",
            "shared_insert",
            n as u64,
            json!({"queries": nq}),
            times,
            reps,
            1,
            24,
            json!({"sum": val}),
            "one insert into the shared array; queries stay cheap. Vec insert is O(n).",
        ));
    }
    out
}
