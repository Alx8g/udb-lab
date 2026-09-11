//! Combined bet: executable region + answer cells on the same catalog.
//!
//! Prices are millunits: current = base + adj + exception.
//! Rankings use integer weight k/W, lower score, then smaller index.
//! Uniform adj shifts every score by k*adj and cannot change the winner.
//!
//! Brute mixed workload must not build or rebuild the envelope. That was
//! charging the baseline for the competitor's index.

use crate::answer_cells::{cell_winner, envelope, score_k, Cell, Product, W_DEN};
use crate::stats::{record, time_ns, Record};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde_json::json;

#[derive(Clone, Copy)]
struct Item {
    price_base: i64,
    delivery: i64,
    exception: i64,
}

struct Catalog {
    items: Vec<Item>,
    adj: i64,
    bases: Vec<Product>,
    cells: Vec<Cell>,
}

struct BruteCatalog {
    items: Vec<Item>,
    adj: i64,
}

fn price_of(it: Item, adj: i64) -> i64 {
    it.price_base + adj + it.exception
}

fn brute_on(items: &[Item], adj: i64, k: i64) -> usize {
    let mut best_i = 0usize;
    let mut best = i128::MAX;
    for (i, &it) in items.iter().enumerate() {
        let p = Product {
            price: price_of(it, adj),
            delivery: it.delivery,
        };
        let s = score_k(p, k);
        if s < best || (s == best && i < best_i) {
            best = s;
            best_i = i;
        }
    }
    best_i
}

impl Catalog {
    fn new(items: Vec<Item>) -> Self {
        let bases: Vec<Product> = items
            .iter()
            .map(|it| Product {
                price: it.price_base + it.exception,
                delivery: it.delivery,
            })
            .collect();
        Self {
            cells: envelope(&bases),
            bases,
            items,
            adj: 0,
        }
    }

    fn joint_winner(&self, k: i64) -> usize {
        cell_winner(&self.cells, &self.bases, k)
    }

    fn brute_winner(&self, k: i64) -> usize {
        brute_on(&self.items, self.adj, k)
    }

    fn bulk_adj(&mut self, d: i64) {
        self.adj += d;
    }

    fn set_exception(&mut self, i: usize, e: i64) {
        self.items[i].exception = e;
        self.bases[i].price = self.items[i].price_base + e;
        self.cells = envelope(&self.bases);
    }
}

impl BruteCatalog {
    fn new(items: Vec<Item>) -> Self {
        Self { items, adj: 0 }
    }
    fn bulk_adj(&mut self, d: i64) {
        self.adj += d;
    }
    fn set_exception(&mut self, i: usize, e: i64) {
        self.items[i].exception = e;
    }
    fn winner(&self, k: i64) -> usize {
        brute_on(&self.items, self.adj, k)
    }
}

fn gen_pareto(n: usize, seed: u64) -> Vec<Item> {
    let mut rng = SmallRng::seed_from_u64(seed);
    let front = (n / 40).clamp(8, 48);
    let den = (front as i64 - 1).max(1);
    let mut items = Vec::with_capacity(n);
    for i in 0..front {
        let t = i as i64;
        items.push(Item {
            price_base: 1000 + 18000 * t / den,
            delivery: 19000 - 18000 * t / den,
            exception: 0,
        });
    }
    while items.len() < n {
        let t = rng.gen_range(0..1001);
        items.push(Item {
            price_base: 4000 + 18 * t + rng.gen_range(0..5001),
            delivery: 4000 + 18 * (1000 - t) + rng.gen_range(0..5001),
            exception: 0,
        });
    }
    items
}

pub fn correctness() -> Result<(), String> {
    let items = gen_pareto(400, 1);
    let mut cat = Catalog::new(items.clone());
    let mut brute = BruteCatalog::new(items);
    for k in (0..=W_DEN).step_by(500) {
        let j = cat.joint_winner(k);
        let b = cat.brute_winner(k);
        if j != b {
            let sj = score_k(
                Product {
                    price: price_of(cat.items[j], cat.adj),
                    delivery: cat.items[j].delivery,
                },
                k,
            );
            let sb = score_k(
                Product {
                    price: price_of(cat.items[b], cat.adj),
                    delivery: cat.items[b].delivery,
                },
                k,
            );
            if sj != sb || j != b {
                return Err(format!("pre-adj mismatch k={k} joint={j} brute={b}"));
            }
        }
    }
    cat.bulk_adj(12500);
    brute.bulk_adj(12500);
    for k in (0..=W_DEN).step_by(500) {
        let j = cat.joint_winner(k);
        let b = brute.winner(k);
        if j != b {
            return Err(format!(
                "adj must not change winner k={k} joint={j} brute={b}"
            ));
        }
    }
    cat.set_exception(0, 50000);
    brute.set_exception(0, 50000);
    for k in (0..=W_DEN).step_by(500) {
        if cat.joint_winner(k) != brute.winner(k) {
            return Err(format!("after exception mismatch k={k}"));
        }
    }
    eprintln!("joint correctness: integer millunits, id ties, brute has no envelope");
    Ok(())
}

pub fn run(quick: bool) -> Vec<Record> {
    let mut out = Vec::new();
    let n = if quick { 8_000usize } else { 60_000usize };
    let queries = if quick { 3_000usize } else { 15_000usize };
    let items = gen_pareto(n, 11);
    let mut rng = SmallRng::seed_from_u64(33);
    let ks: Vec<i64> = (0..queries).map(|_| rng.gen_range(0..=W_DEN)).collect();

    let cat = Catalog::new(items.clone());
    let brute0 = BruteCatalog::new(items.clone());

    let (val, times, reps) = time_ns(2, 5, || {
        let mut acc = 0usize;
        for &k in &ks {
            acc ^= brute0.winner(k);
        }
        acc
    });
    out.push(record(
        "joint",
        "brute_materialized_scan",
        n as u64,
        json!({"queries": queries, "w_den": W_DEN}),
        times,
        reps,
        (n * queries) as u64,
        (n * queries * 16) as u64,
        json!({"xor": val}),
        "scan every product; no envelope allocated",
    ));

    let (val, times, reps) = time_ns(3, 10, || {
        let mut acc = 0usize;
        for &k in &ks {
            acc ^= cat.joint_winner(k);
        }
        acc
    });
    out.push(record(
        "joint",
        "joint_cells_over_bases",
        n as u64,
        json!({"queries": queries, "cells": cat.cells.len(), "w_den": W_DEN}),
        times,
        reps,
        queries as u64,
        queries as u64 * 32,
        json!({"xor": val, "cells": cat.cells.len()}),
        "envelope on bases; adj is invisible to ranking",
    ));

    let (val, times, reps) = time_ns(1, 4, || {
        let mut c = Catalog::new(items.clone());
        let mut acc = 0usize;
        for (q, &k) in ks.iter().enumerate() {
            if q % 20 == 0 {
                c.bulk_adj(10);
            }
            if q % 400 == 0 {
                let i = q % n;
                c.set_exception(i, 50);
            }
            acc ^= c.joint_winner(k);
        }
        acc
    });
    out.push(record(
        "joint",
        "joint_mixed_bulk_and_exceptions",
        n as u64,
        json!({"queries": queries, "bulk_every": 20, "exc_every": 400}),
        times,
        reps,
        queries as u64,
        0,
        json!({"xor": val}),
        "bulk ops free for ranking; exception rebuilds envelope",
    ));

    let (val, times, reps) = time_ns(1, 3, || {
        let mut c = BruteCatalog::new(items.clone());
        let mut acc = 0usize;
        for (q, &k) in ks.iter().enumerate() {
            if q % 20 == 0 {
                c.bulk_adj(10);
            }
            if q % 400 == 0 {
                let i = q % n;
                c.set_exception(i, 50);
            }
            acc ^= c.winner(k);
        }
        acc
    });
    out.push(record(
        "joint",
        "brute_mixed_bulk_and_exceptions",
        n as u64,
        json!({"queries": queries}),
        times,
        reps,
        (n * queries) as u64,
        (n * queries * 16) as u64,
        json!({"xor": val}),
        "same mixed mutations; no envelope construction or rebuild",
    ));

    let (val, times, reps) = time_ns(1, 3, || {
        let mut products: Vec<Product> = items
            .iter()
            .map(|it| Product {
                price: it.price_base,
                delivery: it.delivery,
            })
            .collect();
        let mut acc = 0usize;
        let mut cells = envelope(&products);
        for (q, &k) in ks.iter().enumerate() {
            if q % 50 == 0 {
                let i = q % n;
                products[i].price += 500;
                cells = envelope(&products);
            }
            acc ^= cell_winner(&cells, &products, k);
        }
        acc
    });
    out.push(record(
        "joint",
        "rebuild_on_irregular_updates",
        n as u64,
        json!({"queries": queries}),
        times,
        reps,
        queries as u64,
        0,
        json!({"xor": val}),
        "irregular per-row changes deny the bulk-adj identity",
    ));
    out
}
