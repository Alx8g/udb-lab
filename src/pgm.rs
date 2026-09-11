//! Learned / model index vs binary search vs hash.
//!
//! Simplified PGM: piecewise linear model of the CDF of a sorted unique
//! key array. Lookup predicts a position, then binary-searches a bounded
//! window. Prediction never decides existence.

use crate::stats::{record, time_ns, Record};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde_json::json;
use std::collections::HashMap;

#[derive(Clone, Copy)]
struct Segment {
    key0: u64,
    pos0: usize,
    slope: f64,
    err: usize,
}

struct Pgm {
    keys: Vec<u64>,
    segs: Vec<Segment>,
}

impl Pgm {
    /// Slope is fixed from the first two keys of the segment. A start-to-end
    /// fit always has zero error at the newest endpoint, so it never splits.
    fn build(keys: Vec<u64>, eps: usize) -> Self {
        let mut segs = Vec::new();
        if keys.is_empty() {
            return Self { keys, segs };
        }
        let mut start = 0usize;
        while start < keys.len() {
            if start + 1 >= keys.len() {
                segs.push(Segment {
                    key0: keys[start],
                    pos0: start,
                    slope: 0.0,
                    err: 1,
                });
                break;
            }
            let dx0 = (keys[start + 1] - keys[start]) as f64;
            let slope = if dx0 == 0.0 { 0.0 } else { 1.0 / dx0 };
            let mut last = start + 1;
            let mut err = 1usize;
            for end in (start + 2)..keys.len() {
                let pred = start as f64 + slope * (keys[end] - keys[start]) as f64;
                let e = (pred.round() as i64 - end as i64).unsigned_abs() as usize;
                if e > eps {
                    break;
                }
                last = end;
                if e > err {
                    err = e;
                }
            }
            segs.push(Segment {
                key0: keys[start],
                pos0: start,
                slope,
                err: err.max(1),
            });
            start = last + 1;
        }
        Self { keys, segs }
    }

    fn find_seg(&self, key: u64) -> Segment {
        let mut lo = 0usize;
        let mut hi = self.segs.len();
        while lo + 1 < hi {
            let mid = (lo + hi) / 2;
            if self.segs[mid].key0 <= key {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        self.segs[lo]
    }

    fn get(&self, key: u64) -> Option<usize> {
        if self.keys.is_empty() {
            return None;
        }
        let seg = self.find_seg(key);
        let pred = seg.pos0 as f64 + seg.slope * (key as i64 - seg.key0 as i64) as f64;
        let pred = pred.round().clamp(0.0, (self.keys.len() - 1) as f64) as usize;
        let lo = pred.saturating_sub(seg.err + 2);
        let hi = (pred + seg.err + 2).min(self.keys.len() - 1);
        self.keys[lo..=hi]
            .binary_search(&key)
            .ok()
            .map(|i| lo + i)
    }
}

fn binary_get(keys: &[u64], key: u64) -> Option<usize> {
    keys.binary_search(&key).ok()
}

fn gen_linear(n: usize) -> Vec<u64> {
    (0..n as u64).map(|i| i * 10).collect()
}

fn gen_gaps(n: usize, seed: u64) -> Vec<u64> {
    let mut rng = SmallRng::seed_from_u64(seed);
    let mut x = 0u64;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        x += 1 + rng.gen_range(0..50);
        v.push(x);
    }
    v
}

fn gen_clusters(n: usize, seed: u64) -> Vec<u64> {
    let mut rng = SmallRng::seed_from_u64(seed);
    let mut v = Vec::with_capacity(n);
    let mut x = 0u64;
    while v.len() < n {
        let run = rng.gen_range(50..400).min(n - v.len());
        for _ in 0..run {
            x += 1;
            v.push(x);
        }
        x += rng.gen_range(10_000..80_000);
    }
    v
}

pub fn correctness() -> Result<(), String> {
    for (name, keys) in [
        ("linear", gen_linear(20_000)),
        ("gaps", gen_gaps(20_000, 1)),
        ("clusters", gen_clusters(20_000, 2)),
    ] {
        let pgm = Pgm::build(keys.clone(), 32);
        for (i, &k) in keys.iter().enumerate().step_by(17) {
            match pgm.get(k) {
                Some(j) if j == i && keys[j] == k => {}
                other => return Err(format!("{name} miss key={k} i={i} got={other:?}")),
            }
        }
        if pgm.get(u64::MAX).is_some() {
            return Err(format!("{name} false positive"));
        }
        eprintln!(
            "pgm correctness {name}: {} segments for {} keys",
            pgm.segs.len(),
            keys.len()
        );
    }
    Ok(())
}

pub fn run(quick: bool) -> Vec<Record> {
    let mut out = Vec::new();
    let n = if quick { 200_000usize } else { 5_000_000usize };
    let qn = if quick { 20_000usize } else { 200_000usize };
    let mut rng = SmallRng::seed_from_u64(11);

    for (name, keys) in [
        ("linear", gen_linear(n)),
        ("gaps", gen_gaps(n, 4)),
        ("clusters", gen_clusters(n, 5)),
    ] {
        let pgm = Pgm::build(keys.clone(), 64);
        let mut queries = Vec::with_capacity(qn);
        for _ in 0..qn {
            if rng.gen::<f64>() < 0.9 {
                queries.push(keys[rng.gen_range(0..n)]);
            } else {
                queries.push(rng.gen::<u64>());
            }
        }
        let map: HashMap<u64, usize> = keys.iter().enumerate().map(|(i, &k)| (k, i)).collect();

        let (val, times) = time_ns(2, 6, || {
            let mut h = 0u64;
            for &q in &queries {
                if let Some(i) = binary_get(&keys, q) {
                    h ^= i as u64;
                }
            }
            h
        });
        out.push(record(
            "pgm",
            &format!("binary_{name}"),
            n as u64,
            json!({"queries": qn}),
            times,
            qn as u64,
            qn as u64 * 64,
            json!({"xor": val}),
            "classic sorted-array binary search",
        ));

        let segs = pgm.segs.len();
        let (val, times) = time_ns(2, 6, || {
            let mut h = 0u64;
            for &q in &queries {
                if let Some(i) = pgm.get(q) {
                    h ^= i as u64;
                }
            }
            h
        });
        out.push(record(
            "pgm",
            &format!("pgm_{name}"),
            n as u64,
            json!({"queries": qn, "segments": segs}),
            times,
            qn as u64,
            qn as u64 * 48,
            json!({"xor": val, "segments": segs, "bytes_model": segs * 32}),
            "linear CDF model + bounded correction window",
        ));

        let (val, times) = time_ns(2, 6, || {
            let mut h = 0u64;
            for &q in &queries {
                if let Some(&i) = map.get(&q) {
                    h ^= i as u64;
                }
            }
            h
        });
        out.push(record(
            "pgm",
            &format!("hash_{name}"),
            n as u64,
            json!({"queries": qn, "map_len": map.len()}),
            times,
            qn as u64,
            qn as u64 * 64,
            json!({"xor": val}),
            "HashMap specialist: extra memory, O(1) expected",
        ));
    }
    out
}
