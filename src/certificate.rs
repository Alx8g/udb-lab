//! Certificate-complexity joins vs input-size joins.
//!
//! Intersection of two sorted sets:
//!   - merge scan: O(|A| + |B|)
//!   - galloping / exponential search: O(k log(n/k)) where k is output
//!   - bound certificate: if max(A) < min(B) the empty intersection is proven
//!     from two numbers
//!
//! Triangle counting on a bipartite-ish graph: naive vs leapfrog-style.

use crate::stats::{record, time_ns, Record};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde_json::json;

fn gen_sorted(n: usize, max: u64, seed: u64) -> Vec<u64> {
    let mut rng = SmallRng::seed_from_u64(seed);
    let mut v: Vec<u64> = (0..n).map(|_| rng.gen_range(0..max)).collect();
    v.sort_unstable();
    v.dedup();
    v
}

fn merge_intersect(a: &[u64], b: &[u64]) -> usize {
    let mut i = 0;
    let mut j = 0;
    let mut c = 0;
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                c += 1;
                i += 1;
                j += 1;
            }
        }
    }
    c
}

fn gallop_seek(xs: &[u64], mut lo: usize, target: u64) -> usize {
    if lo >= xs.len() || xs[lo] >= target {
        return lo;
    }
    let mut step = 1usize;
    let mut hi = lo + 1;
    while hi < xs.len() && xs[hi] < target {
        lo = hi;
        hi = hi.saturating_add(step).min(xs.len());
        step *= 2;
    }
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if xs[mid] < target {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

fn gallop_intersect(a: &[u64], b: &[u64]) -> usize {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut j = 0usize;
    let mut c = 0usize;
    for &x in small {
        j = gallop_seek(large, j, x);
        if j >= large.len() {
            break;
        }
        if large[j] == x {
            c += 1;
            j += 1;
        }
    }
    c
}

/// Two-number certificate: disjoint by range.
fn bound_disjoint(a: &[u64], b: &[u64]) -> bool {
    if a.is_empty() || b.is_empty() {
        return true;
    }
    a[a.len() - 1] < b[0] || b[b.len() - 1] < a[0]
}

pub fn correctness() -> Result<(), String> {
    for seed in 0..20u64 {
        let a = gen_sorted(5_000, 20_000, seed);
        let b = gen_sorted(5_000, 20_000, seed + 50);
        let m = merge_intersect(&a, &b);
        let g = gallop_intersect(&a, &b);
        if m != g {
            return Err(format!(
                "intersect mismatch seed={seed} merge={m} gallop={g}"
            ));
        }
        if bound_disjoint(&a, &b) && m != 0 {
            return Err("bound claimed disjoint but intersection nonempty".into());
        }
    }
    let a: Vec<u64> = (0..10_000).collect();
    let b: Vec<u64> = (20_000..30_000).collect();
    if !bound_disjoint(&a, &b) || merge_intersect(&a, &b) != 0 {
        return Err("separated ranges should be empty".into());
    }
    eprintln!("certificate correctness: merge=gallop, bound empty-join ok");
    Ok(())
}

pub fn run(quick: bool) -> Vec<Record> {
    let mut out = Vec::new();
    let n = if quick { 200_000usize } else { 2_000_000usize };

    // Case 1: heavily overlapping.
    let a = gen_sorted(n, (n as u64) * 2, 1);
    let b = gen_sorted(n, (n as u64) * 2, 2);
    let (val, times, reps) = time_ns(2, 6, || merge_intersect(&a, &b));
    out.push(record(
        "certificate",
        "merge_overlap",
        n as u64,
        json!({"out": val}),
        times,
        reps,
        (a.len() + b.len()) as u64,
        ((a.len() + b.len()) * 8) as u64,
        json!({"out": val}),
        "must inspect both inputs; certificate ≈ input",
    ));
    let (val, times, reps) = time_ns(2, 6, || gallop_intersect(&a, &b));
    out.push(record(
        "certificate",
        "gallop_overlap",
        n as u64,
        json!({"out": val}),
        times,
        reps,
        a.len().min(b.len()) as u64,
        (a.len().min(b.len()) * 16) as u64,
        json!({"out": val}),
        "galloping loses when output is dense",
    ));

    // Case 2: tiny overlap (skewed).
    let a = gen_sorted(n, n as u64 * 8, 3);
    let b: Vec<u64> = a.iter().step_by(10_000).copied().collect();
    let (val, times, reps) = time_ns(2, 8, || merge_intersect(&a, &b));
    out.push(record(
        "certificate",
        "merge_sparse",
        n as u64,
        json!({"small": b.len(), "out": val}),
        times,
        reps,
        (a.len() + b.len()) as u64,
        ((a.len() + b.len()) * 8) as u64,
        json!({"out": val}),
        "merge still walks the large side",
    ));
    let (val, times, reps) = time_ns(3, 12, || gallop_intersect(&a, &b));
    out.push(record(
        "certificate",
        "gallop_sparse",
        n as u64,
        json!({"small": b.len(), "out": val}),
        times,
        reps,
        b.len() as u64,
        (b.len() * 64) as u64,
        json!({"out": val}),
        "work tracks small side + log jumps",
    ));

    // Case 3: disjoint by range. Bound certificate vs merge.
    let a: Vec<u64> = (0..n as u64).collect();
    let b: Vec<u64> = ((n as u64 + 10)..(2 * n as u64 + 10)).collect();
    let (val, times, reps) = time_ns(2, 6, || merge_intersect(&a, &b));
    out.push(record(
        "certificate",
        "merge_disjoint_range",
        n as u64,
        json!({"out": val}),
        times,
        reps,
        (a.len() + b.len()) as u64,
        ((a.len() + b.len()) * 8) as u64,
        json!({"out": val}),
        "empty result still walks until one side ends",
    ));
    let (val, times, reps) = time_ns(8, 40, || bound_disjoint(&a, &b) as usize);
    out.push(record(
        "certificate",
        "bound_certificate_disjoint",
        n as u64,
        json!({"out": val}),
        times,
        reps,
        2,
        16,
        json!({"disjoint": val == 1}),
        "two extrema prove empty intersection",
    ));

    // Case 4: interleaved empty (no range certificate).
    let a: Vec<u64> = (0..n as u64).map(|x| x * 2).collect();
    let b: Vec<u64> = (0..n as u64).map(|x| x * 2 + 1).collect();
    let (val, times, reps) = time_ns(2, 5, || merge_intersect(&a, &b));
    out.push(record(
        "certificate",
        "merge_interleaved_empty",
        n as u64,
        json!({"out": val}),
        times,
        reps,
        (a.len() + b.len()) as u64,
        ((a.len() + b.len()) * 8) as u64,
        json!({"out": val, "bound_lies": bound_disjoint(&a, &b)}),
        "empty result, but bounds overlap; merge is the certificate",
    ));
    out
}
