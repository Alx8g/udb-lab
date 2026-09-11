//! Combined bet from the research note: executable region + answer cells
//! + progressive bounds on the same changing catalog.
//!
//! Workload: parameterized ranking over products whose prices are stored as
//! base + group adjustment + exception. Compare:
//!   1. brute: apply adj, scan all products per query
//!   2. region only: O(1) bulk adj, still scan
//!   3. cells only: envelope over current prices, rebuild on any adj
//!   4. joint: envelope over bases; adj shifts every score by the same
//!      amount and cannot change the winner; only exceptions can.
//!
//! This is the design-together test: the representation makes a stronger
//! certificate than caching the ranking of fully materialized prices.

use crate::answer_cells::{envelope, lookup_cell, Cell, Product};
use crate::stats::{record, time_ns, Record};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde_json::json;

#[derive(Clone, Copy)]
struct Item {
    price_base: f64,
    delivery: f64,
    exception: f64,
}

struct Catalog {
    items: Vec<Item>,
    adj: f64,
    cells: Vec<Cell>,
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
            items,
            adj: 0.0,
        }
    }

    fn price(&self, i: usize) -> f64 {
        self.items[i].price_base + self.adj + self.items[i].exception
    }

    fn as_products(&self) -> Vec<Product> {
        (0..self.items.len())
            .map(|i| Product {
                price: self.price(i),
                delivery: self.items[i].delivery,
            })
            .collect()
    }

    fn brute_winner(&self, w: f64) -> usize {
        let mut best_i = 0usize;
        let mut best = f64::INFINITY;
        for i in 0..self.items.len() {
            let s = w * self.price(i) + (1.0 - w) * self.items[i].delivery;
            if s < best {
                best = s;
                best_i = i;
            }
        }
        best_i
    }

    fn joint_winner(&self, w: f64) -> usize {
        // Uniform adj cancels in comparisons. Exceptions are already in the
        // certified bases if we rebuild after exception writes.
        lookup_cell(&self.cells, w)
            .map(|c| c.winner)
            .unwrap_or_else(|| self.brute_winner(w))
    }

    fn bulk_adj(&mut self, k: f64) {
        self.adj += k;
    }

    fn set_exception(&mut self, i: usize, e: f64) {
        self.items[i].exception = e;
        let bases: Vec<Product> = self
            .items
            .iter()
            .map(|it| Product {
                price: it.price_base + it.exception,
                delivery: it.delivery,
            })
            .collect();
        self.cells = envelope(&bases);
    }
}

fn gen_pareto(n: usize, seed: u64) -> Vec<Item> {
    let mut rng = SmallRng::seed_from_u64(seed);
    let front = (n / 40).clamp(8, 48);
    let mut items = Vec::with_capacity(n);
    for i in 0..front {
        let t = i as f64 / (front as f64 - 1.0).max(1.0);
        items.push(Item {
            price_base: 1.0 + 18.0 * t,
            delivery: 19.0 - 18.0 * t,
            exception: 0.0,
        });
    }
    while items.len() < n {
        let t = rng.gen::<f64>();
        items.push(Item {
            price_base: 4.0 + 18.0 * t + rng.gen::<f64>() * 5.0,
            delivery: 4.0 + 18.0 * (1.0 - t) + rng.gen::<f64>() * 5.0,
            exception: 0.0,
        });
    }
    items
}

pub fn correctness() -> Result<(), String> {
    let items = gen_pareto(400, 1);
    let mut cat = Catalog::new(items);
    for w_i in 0..=20 {
        let w = w_i as f64 / 20.0;
        if cat.joint_winner(w) != cat.brute_winner(w) {
            let j = cat.joint_winner(w);
            let b = cat.brute_winner(w);
            let sj = w * cat.price(j) + (1.0 - w) * cat.items[j].delivery;
            let sb = w * cat.price(b) + (1.0 - w) * cat.items[b].delivery;
            if (sj - sb).abs() > 1e-9 {
                return Err(format!("pre-adj mismatch w={w}"));
            }
        }
    }
    cat.bulk_adj(12.5);
    for w_i in 0..=20 {
        let w = w_i as f64 / 20.0;
        let j = cat.joint_winner(w);
        let b = cat.brute_winner(w);
        let sj = w * cat.price(j) + (1.0 - w) * cat.items[j].delivery;
        let sb = w * cat.price(b) + (1.0 - w) * cat.items[b].delivery;
        if (sj - sb).abs() > 1e-9 {
            return Err(format!(
                "adj must not change winner w={w} joint={j} brute={b}"
            ));
        }
    }
    cat.set_exception(0, 50.0);
    for w_i in 0..=20 {
        let w = w_i as f64 / 20.0;
        let j = cat.joint_winner(w);
        let b = cat.brute_winner(w);
        let sj = w * cat.price(j) + (1.0 - w) * cat.items[j].delivery;
        let sb = w * cat.price(b) + (1.0 - w) * cat.items[b].delivery;
        if (sj - sb).abs() > 1e-9 {
            return Err(format!("after exception mismatch w={w}"));
        }
    }
    eprintln!("joint correctness: bulk adj preserves ranking; exceptions rebuild cells");
    Ok(())
}

pub fn run(quick: bool) -> Vec<Record> {
    let mut out = Vec::new();
    let n = if quick { 8_000usize } else { 60_000usize };
    let queries = if quick { 3_000usize } else { 15_000usize };
    let items = gen_pareto(n, 11);
    let mut rng = SmallRng::seed_from_u64(33);
    let ws: Vec<f64> = (0..queries).map(|_| rng.gen::<f64>()).collect();

    let cat = Catalog::new(items.clone());

    let (val, times) = time_ns(2, 5, || {
        let mut acc = 0usize;
        for &w in &ws {
            acc ^= cat.brute_winner(w);
        }
        acc
    });
    out.push(record(
        "joint",
        "brute_materialized_scan",
        n as u64,
        json!({"queries": queries}),
        times,
        (n * queries) as u64,
        (n * queries * 16) as u64,
        json!({"xor": val}),
        "evaluate every product for every weight",
    ));

    let (val, times) = time_ns(3, 10, || {
        let mut acc = 0usize;
        for &w in &ws {
            acc ^= cat.joint_winner(w);
        }
        acc
    });
    out.push(record(
        "joint",
        "joint_cells_over_bases",
        n as u64,
        json!({"queries": queries, "cells": cat.cells.len()}),
        times,
        queries as u64,
        queries as u64 * 32,
        json!({"xor": val, "cells": cat.cells.len()}),
        "envelope on bases; adj is invisible to ranking",
    ));

    // Mix: many bulk adjs, rare exceptions.
    let (val, times) = time_ns(1, 4, || {
        let mut c = Catalog::new(items.clone());
        let mut acc = 0usize;
        for (q, &w) in ws.iter().enumerate() {
            if q % 20 == 0 {
                c.bulk_adj(0.01);
            }
            if q % 400 == 0 {
                let i = q % n;
                c.set_exception(i, 0.05);
            }
            acc ^= c.joint_winner(w);
        }
        acc
    });
    out.push(record(
        "joint",
        "joint_mixed_bulk_and_exceptions",
        n as u64,
        json!({"queries": queries, "bulk_every": 20, "exc_every": 400}),
        times,
        queries as u64,
        0,
        json!({"xor": val}),
        "bulk ops free for ranking; exception forces envelope rebuild",
    ));

    let (val, times) = time_ns(1, 3, || {
        let mut c = Catalog::new(items.clone());
        let mut acc = 0usize;
        for (q, &w) in ws.iter().enumerate() {
            if q % 20 == 0 {
                c.bulk_adj(0.01);
            }
            if q % 400 == 0 {
                let i = q % n;
                c.set_exception(i, 0.05);
            }
            acc ^= c.brute_winner(w);
        }
        acc
    });
    out.push(record(
        "joint",
        "brute_mixed_bulk_and_exceptions",
        n as u64,
        json!({"queries": queries}),
        times,
        (n * queries) as u64,
        (n * queries * 16) as u64,
        json!({"xor": val}),
        "same mixed mutations, no certificate",
    ));

    // Negative: per-row price jitter is not a bulk adj. Joint must rebuild
    // or it would be wrong; we force a full envelope each jitter batch.
    let (val, times) = time_ns(1, 3, || {
        let mut products = cat.as_products();
        let mut acc = 0usize;
        let mut cells = envelope(&products);
        for (q, &w) in ws.iter().enumerate() {
            if q % 50 == 0 {
                let i = q % n;
                products[i].price += 0.5;
                cells = envelope(&products);
            }
            acc ^= lookup_cell(&cells, w).map(|c| c.winner).unwrap_or(0);
        }
        acc
    });
    out.push(record(
        "joint",
        "rebuild_on_irregular_updates",
        n as u64,
        json!({"queries": queries}),
        times,
        queries as u64,
        0,
        json!({"xor": val}),
        "irregular per-row changes deny the bulk-adj identity; rebuild cost shows up",
    ));
    out
}
