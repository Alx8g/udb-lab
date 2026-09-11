//! Queryable redundancy: store C = A + B as both a recovery block and an
//! aggregate projection.
//!
//! C is i128 so any one source reconstructs from the other two without
//! overflow. The extra width is the redundancy the system already pays for.

use crate::stats::{record, time_ns, Record};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde_json::json;

#[derive(Clone)]
struct Blocks {
    a: Vec<i64>,
    b: Vec<i64>,
    c: Vec<i128>,
}

impl Blocks {
    fn new(a: Vec<i64>, b: Vec<i64>) -> Self {
        assert_eq!(a.len(), b.len());
        let c = a
            .iter()
            .zip(b.iter())
            .map(|(&x, &y)| x as i128 + y as i128)
            .collect();
        Self { a, b, c }
    }

    fn recover_a(&self) -> Vec<i64> {
        self.c
            .iter()
            .zip(self.b.iter())
            .map(|(&c, &b)| (c - b as i128) as i64)
            .collect()
    }

    fn sum_a_plus_b_from_c(&self) -> i128 {
        self.c.iter().copied().sum()
    }

    fn sum_a_plus_b_from_sources(&self) -> i128 {
        self.a.iter().map(|&x| x as i128).sum::<i128>()
            + self.b.iter().map(|&x| x as i128).sum::<i128>()
    }
}

pub fn correctness() -> Result<(), String> {
    let mut rng = SmallRng::seed_from_u64(2);
    let n = 10_000;
    let a: Vec<i64> = (0..n).map(|_| rng.gen::<i64>()).collect();
    let b: Vec<i64> = (0..n).map(|_| rng.gen::<i64>()).collect();
    let blocks = Blocks::new(a.clone(), b);
    let rec = blocks.recover_a();
    if rec != a {
        return Err("recovery of A from C-B failed".into());
    }
    if blocks.sum_a_plus_b_from_c() != blocks.sum_a_plus_b_from_sources() {
        return Err("coded sum mismatch".into());
    }
    eprintln!("redundancy correctness: recover A and coded A+B sum ok");
    Ok(())
}

pub fn run(quick: bool) -> Vec<Record> {
    let mut out = Vec::new();
    let n = if quick { 500_000usize } else { 8_000_000usize };
    let mut rng = SmallRng::seed_from_u64(6);
    let a: Vec<i64> = (0..n).map(|_| rng.gen_range(-1_000_000..1_000_000)).collect();
    let b: Vec<i64> = (0..n).map(|_| rng.gen_range(-1_000_000..1_000_000)).collect();
    let c64: Vec<i64> = a.iter().zip(b.iter()).map(|(&x, &y)| x + y).collect();
    let blocks = Blocks::new(a.clone(), b.clone());

    let (val, times) = time_ns(2, 6, || {
        a.iter().sum::<i64>() as i128 + b.iter().sum::<i64>() as i128
    });
    out.push(record(
        "redundancy",
        "sum_from_a_and_b",
        n as u64,
        json!({}),
        times,
        (2 * n) as u64,
        (2 * n * 8) as u64,
        json!({"sum": val}),
        "read both source blocks",
    ));

    let (val, times) = time_ns(2, 6, || c64.iter().sum::<i64>());
    out.push(record(
        "redundancy",
        "sum_from_coded_c",
        n as u64,
        json!({}),
        times,
        n as u64,
        (n * 8) as u64,
        json!({"sum": val}),
        "parity block is the elementwise sum, same width as a source",
    ));

    let t = 0i64;
    let (val, times) = time_ns(2, 6, || {
        let mut s = 0i64;
        for i in 0..n {
            let v = a[i] + b[i];
            if v > t {
                s += v;
            }
        }
        s
    });
    out.push(record(
        "redundancy",
        "filter_sum_from_sources",
        n as u64,
        json!({"t": t}),
        times,
        (2 * n) as u64,
        (2 * n * 8) as u64,
        json!({"sum": val}),
        "predicate on A+B still needs both sources without C",
    ));
    let (val, times) = time_ns(2, 6, || {
        c64.iter().filter(|&&v| v > t).copied().sum::<i64>()
    });
    out.push(record(
        "redundancy",
        "filter_sum_from_coded_c",
        n as u64,
        json!({"t": t}),
        times,
        n as u64,
        (n * 8) as u64,
        json!({"sum": val}),
        "same predicate against the recovery block",
    ));

    let (val, times) = time_ns(2, 6, || a.iter().filter(|&&x| x > 0).count());
    out.push(record(
        "redundancy",
        "filter_a_only_negative_control",
        n as u64,
        json!({}),
        times,
        n as u64,
        (n * 8) as u64,
        json!({"count": val}),
        "C = A+B does not answer predicates on A; coded queryability is operator-specific",
    ));

    let (val, times) = time_ns(1, 4, || {
        blocks.recover_a().iter().fold(0i64, |s, &x| s.wrapping_add(x))
    });
    out.push(record(
        "redundancy",
        "recover_a_from_b_and_c",
        n as u64,
        json!({}),
        times,
        n as u64,
        (n * 24) as u64,
        json!({"checksum": val}),
        "erasure repair cost of the same extra block",
    ));
    out
}
