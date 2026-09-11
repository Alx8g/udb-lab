//! Executable regions: store a bulk transformation as one adjustment
//! instead of materializing N row writes.
//!
//! current_price(row) = base + (adj - adj_at_birth) + exception
//! A bulk +K on existing members increments adj in O(1).
//! New inserts capture adj_at_birth so they do not inherit past ops.

use crate::stats::{record, time_ns, Record};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde_json::json;
use std::collections::HashMap;

#[derive(Clone)]
struct Row {
    base: i64,
    birth_adj: i64,
    exception: i64,
}

struct Region {
    rows: Vec<Row>,
    adj: i64,
    base_sum: i64,
    birth_sum: i64,
    exception_sum: i64,
}

impl Region {
    fn with_bases(bases: &[i64]) -> Self {
        let rows = bases
            .iter()
            .map(|&base| Row {
                base,
                birth_adj: 0,
                exception: 0,
            })
            .collect::<Vec<_>>();
        let base_sum = bases.iter().sum();
        Self {
            rows,
            adj: 0,
            base_sum,
            birth_sum: 0,
            exception_sum: 0,
        }
    }

    fn price(&self, i: usize) -> i64 {
        let r = &self.rows[i];
        r.base + (self.adj - r.birth_adj) + r.exception
    }

    fn sum(&self) -> i64 {
        self.base_sum + self.adj * self.rows.len() as i64 - self.birth_sum + self.exception_sum
    }

    fn bulk_add_existing(&mut self, k: i64) {
        self.adj += k;
    }

    fn insert(&mut self, actual_price: i64) {
        self.rows.push(Row {
            base: actual_price,
            birth_adj: self.adj,
            exception: 0,
        });
        self.base_sum += actual_price;
        self.birth_sum += self.adj;
    }

    fn set_exception(&mut self, i: usize, exception: i64) {
        self.exception_sum += exception - self.rows[i].exception;
        self.rows[i].exception = exception;
    }
}

struct Materialized {
    prices: Vec<i64>,
}

impl Materialized {
    fn new(bases: &[i64]) -> Self {
        Self {
            prices: bases.to_vec(),
        }
    }
    fn bulk_add(&mut self, k: i64) {
        for p in &mut self.prices {
            *p += k;
        }
    }
    fn insert(&mut self, p: i64) {
        self.prices.push(p);
    }
    fn sum(&self) -> i64 {
        self.prices.iter().sum()
    }
}

pub fn correctness() -> Result<(), String> {
    let bases: Vec<i64> = (0..500).map(|i| 100 + i).collect();
    let mut r = Region::with_bases(&bases);
    let mut m = Materialized::new(&bases);
    r.bulk_add_existing(100);
    m.bulk_add(100);
    r.insert(50);
    m.insert(50);
    r.bulk_add_existing(7);
    m.bulk_add(7);
    // The last insert was born after the +100, so the second bulk (+7) applies
    // to it in the materialized model only if we also added 7 to everyone
    // including the new row. Our contract: future bulks apply to everyone
    // currently in the region, including later inserts. That matches m.bulk_add.
    r.set_exception(3, 9);
    m.prices[3] += 9;
    for i in 0..r.rows.len() {
        if r.price(i) != m.prices[i] {
            return Err(format!(
                "row {i} region={} mat={}",
                r.price(i),
                m.prices[i]
            ));
        }
    }
    if r.sum() != m.sum() {
        return Err(format!("sum region={} mat={}", r.sum(), m.sum()));
    }
    // Past-op isolation: a row inserted after +100 should not include that +100
    // in its starting actual price (we inserted 50 as the actual).
    if r.price(500) != 50 + 7 {
        return Err(format!(
            "insert inherited past bulk unexpectedly: {}",
            r.price(500)
        ));
    }
    eprintln!("executable_regions correctness: bulk, insert isolation, exceptions ok");
    Ok(())
}

pub fn run(quick: bool) -> Vec<Record> {
    let mut out = Vec::new();
    let ns: Vec<usize> = if quick {
        vec![10_000, 200_000]
    } else {
        vec![10_000, 200_000, 2_000_000, 20_000_000]
    };
    let mut rng = SmallRng::seed_from_u64(3);

    for n in ns {
        let bases: Vec<i64> = (0..n).map(|_| rng.gen_range(50..5_000)).collect();

        let mut mat = Materialized::new(&bases);
        let ((), times) = time_ns(1, 5, || {
            mat.bulk_add(100);
            mat.bulk_add(-100);
        });
        out.push(record(
            "executable_regions",
            "materialized_bulk_add",
            n as u64,
            json!({"k": 100}),
            times,
            n as u64,
            (n * 8) as u64,
            json!({"sum": mat.sum()}),
            "N scalar writes per bulk operation",
        ));

        let mut region = Region::with_bases(&bases);
        let ((), times) = time_ns(4, 30, || {
            region.bulk_add_existing(100);
            region.bulk_add_existing(-100);
        });
        out.push(record(
            "executable_regions",
            "compact_bulk_add",
            n as u64,
            json!({"k": 100}),
            times,
            1,
            8,
            json!({"sum": region.sum()}),
            "one adjustment increment; N writes never happen",
        ));

        // Point lookups
        let ids: Vec<usize> = (0..4_000).map(|_| rng.gen_range(0..n)).collect();
        let region = Region::with_bases(&bases);
        let (val, times) = time_ns(3, 12, || {
            let mut s = 0i64;
            for &i in &ids {
                s += region.price(i);
            }
            s
        });
        out.push(record(
            "executable_regions",
            "compact_point_lookup_4k",
            n as u64,
            json!({"lookups": 4000}),
            times,
            4000,
            4000 * 24,
            json!({"sum": val}),
            "evaluate base+adj-birth+exc per lookup",
        ));

        let mat = Materialized::new(&bases);
        let (val, times) = time_ns(3, 12, || {
            let mut s = 0i64;
            for &i in &ids {
                s += mat.prices[i];
            }
            s
        });
        out.push(record(
            "executable_regions",
            "materialized_point_lookup_4k",
            n as u64,
            json!({"lookups": 4000}),
            times,
            4000,
            4000 * 8,
            json!({"sum": val}),
            "direct load of current scalar",
        ));

        // Filter price > T: compact still scans bases; materialized can scan prices.
        // Engine pairing: SIMD-style scan of bases + broadcast adj.
        let t = 2500i64;
        let adj = 100i64;
        let (val, times) = time_ns(2, 8, || {
            let mut c = 0u64;
            for &b in &bases {
                if b + adj > t {
                    c += 1;
                }
            }
            c
        });
        out.push(record(
            "executable_regions",
            "compact_filter_scan",
            n as u64,
            json!({"threshold": t, "adj": adj}),
            times,
            n as u64,
            (n * 8) as u64,
            json!({"count": val}),
            "bulk adj is free; filter still touches every base unless an index exists",
        ));
    }

    // Exception density vs compactness.
    let n = if quick { 50_000usize } else { 500_000usize };
    let bases: Vec<i64> = (0..n).map(|_| 1000).collect();
    for density in [0.0, 0.01, 0.1, 1.0] {
        let mut region = Region::with_bases(&bases);
        let n_exc = (n as f64 * density) as usize;
        for i in 0..n_exc {
            region.set_exception(i, 3);
        }
        let map: HashMap<usize, i64> = (0..n_exc).map(|i| (i, 3i64)).collect();
        let (val, times) = time_ns(3, 10, || {
            region.sum() + map.len() as i64
        });
        out.push(record(
            "executable_regions",
            "exception_density_sum",
            n as u64,
            json!({"density": density, "exceptions": n_exc, "map_bytes_est": map.len() * 24}),
            times,
            1,
            32 + (n_exc as u64 * 24),
            json!({"sum": val}),
            "compact form wins while exceptions stay sparse",
        ));
    }
    out
}
