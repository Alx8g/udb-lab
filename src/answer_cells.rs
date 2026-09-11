//! Answer cells: reuse an exact winner across a region of query weights
//! and a bounded envelope of data changes.
//!
//! score(w) = w*price + (1-w)*delivery, lower is better.
//! Lower envelope of lines over w in [0,1] partitions weight space into
//! cells with a constant winner. A cell remains valid while every product's
//! L∞ drift is less than half the minimum gap on that cell.

use crate::stats::{record, time_ns, Record};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde_json::json;

#[derive(Clone, Copy, Debug)]
pub struct Product {
    pub price: f64,
    pub delivery: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct Cell {
    pub w_lo: f64,
    pub w_hi: f64,
    pub winner: usize,
    pub min_gap: f64,
}

fn score(p: Product, w: f64) -> f64 {
    w * p.price + (1.0 - w) * p.delivery
}

fn brute_winner(ps: &[Product], w: f64) -> (usize, f64) {
    let mut best_i = 0usize;
    let mut best = f64::INFINITY;
    let mut second = f64::INFINITY;
    for (i, &p) in ps.iter().enumerate() {
        let s = score(p, w);
        if s < best {
            second = best;
            best = s;
            best_i = i;
        } else if s < second {
            second = s;
        }
    }
    (best_i, second - best)
}

/// Lower envelope of lines `delivery + w*(price-delivery)` on [0,1].
/// The pointwise min of linear functions is concave, so successive slopes
/// decrease. Sort high slope first (winner at small w).
pub fn envelope(ps: &[Product]) -> Vec<Cell> {
    if ps.is_empty() {
        return Vec::new();
    }
    let intercept = |i: usize| ps[i].delivery;
    let slope = |i: usize| ps[i].price - ps[i].delivery;
    let mut idx: Vec<usize> = (0..ps.len()).collect();
    idx.sort_by(|&i, &j| {
        slope(j)
            .partial_cmp(&slope(i))
            .unwrap()
            .then_with(|| intercept(i).partial_cmp(&intercept(j)).unwrap())
            .then_with(|| i.cmp(&j))
    });
    let mut uniq: Vec<usize> = Vec::new();
    for i in idx {
        if let Some(&last) = uniq.last() {
            if (slope(i) - slope(last)).abs() < 1e-15 {
                continue;
            }
        }
        uniq.push(i);
    }
    let meet = |i: usize, j: usize| -> f64 {
        let ds = slope(j) - slope(i);
        if ds.abs() < 1e-18 {
            return f64::INFINITY;
        }
        (intercept(i) - intercept(j)) / ds
    };
    let mut hull: Vec<usize> = Vec::new();
    for i in uniq {
        while hull.len() >= 2 {
            let a = hull[hull.len() - 2];
            let b = hull[hull.len() - 1];
            if meet(a, b) >= meet(b, i) {
                hull.pop();
            } else {
                break;
            }
        }
        hull.push(i);
    }
    while hull.len() >= 2 && meet(hull[0], hull[1]) <= 0.0 {
        hull.remove(0);
    }
    while hull.len() >= 2 && meet(hull[hull.len() - 2], hull[hull.len() - 1]) >= 1.0 {
        hull.pop();
    }
    // Clip to [0,1] and drop segments that miss the interval.
    let mut cells = Vec::new();
    for k in 0..hull.len() {
        let i = hull[k];
        let lo = if k == 0 {
            f64::NEG_INFINITY
        } else {
            meet(hull[k - 1], i)
        };
        let hi = if k + 1 == hull.len() {
            f64::INFINITY
        } else {
            meet(i, hull[k + 1])
        };
        let w_lo = lo.max(0.0);
        let w_hi = hi.min(1.0);
        if w_hi <= w_lo + 1e-15 {
            continue;
        }
        // Endpoints have gap 0 by construction (adjacent winners meet).
        // Interior gap is the certificate: sample mid and two interior points.
        let mid = 0.5 * (w_lo + w_hi);
        let a = w_lo + 0.25 * (w_hi - w_lo);
        let b = w_lo + 0.75 * (w_hi - w_lo);
        let g = brute_winner(ps, mid)
            .1
            .min(brute_winner(ps, a).1)
            .min(brute_winner(ps, b).1);
        cells.push(Cell {
            w_lo,
            w_hi,
            winner: i,
            min_gap: g,
        });
    }
    if !cells.is_empty() {
        cells[0].w_lo = 0.0;
        let last = cells.len() - 1;
        cells[last].w_hi = 1.0;
    }
    cells
}

pub fn lookup_cell(cells: &[Cell], w: f64) -> Option<Cell> {
    let mut lo = 0usize;
    let mut hi = cells.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if w < cells[mid].w_lo {
            hi = mid;
        } else if w > cells[mid].w_hi {
            lo = mid + 1;
        } else {
            return Some(cells[mid]);
        }
    }
    None
}

#[derive(Clone)]
pub struct AnswerStore {
    pub certified: Vec<Product>,
    pub current: Vec<Product>,
    pub cells: Vec<Cell>,
    pub max_drift: f64,
    pub rebuilds: u64,
    pub cell_hits: u64,
    pub cell_misses: u64,
}

impl AnswerStore {
    pub fn new(ps: Vec<Product>) -> Self {
        let cells = envelope(&ps);
        Self {
            certified: ps.clone(),
            current: ps,
            cells,
            max_drift: 0.0,
            rebuilds: 0,
            cell_hits: 0,
            cell_misses: 0,
        }
    }

    fn drift_of(cert: Product, cur: Product) -> f64 {
        (cur.price - cert.price)
            .abs()
            .max((cur.delivery - cert.delivery).abs())
    }

    pub fn update(&mut self, i: usize, p: Product) {
        self.current[i] = p;
        let d = Self::drift_of(self.certified[i], p);
        if d > self.max_drift {
            self.max_drift = d;
        }
    }

    fn valid_for(&self, cell: Cell) -> bool {
        // Each score can move by at most drift; relative gap shrinks by <= 2*drift.
        2.0 * self.max_drift + 1e-12 < cell.min_gap
    }

    pub fn query(&mut self, w: f64) -> usize {
        if let Some(cell) = lookup_cell(&self.cells, w) {
            if self.valid_for(cell) {
                self.cell_hits += 1;
                return cell.winner;
            }
        }
        self.cell_misses += 1;
        self.rebuild();
        lookup_cell(&self.cells, w)
            .map(|c| c.winner)
            .unwrap_or_else(|| brute_winner(&self.current, w).0)
    }

    fn rebuild(&mut self) {
        self.certified = self.current.clone();
        self.cells = envelope(&self.current);
        self.max_drift = 0.0;
        self.rebuilds += 1;
    }

    fn reset_from(&mut self, ps: &[Product], cells: &[Cell]) {
        self.certified.copy_from_slice(ps);
        self.current.copy_from_slice(ps);
        self.cells.clear();
        self.cells.extend_from_slice(cells);
        self.max_drift = 0.0;
        self.rebuilds = 0;
        self.cell_hits = 0;
        self.cell_misses = 0;
    }
}

fn gen_products(n: usize, seed: u64, mode: &str) -> Vec<Product> {
    let mut rng = SmallRng::seed_from_u64(seed);
    match mode {
        "pareto" => {
            // A few on the front, many dominated. Envelope stays small.
            let front = (n / 40).clamp(8, 64);
            let mut ps = Vec::with_capacity(n);
            for i in 0..front {
                let t = i as f64 / (front as f64 - 1.0).max(1.0);
                ps.push(Product {
                    price: 1.0 + 18.0 * t,
                    delivery: 19.0 - 18.0 * t,
                });
            }
            while ps.len() < n {
                let t = rng.gen::<f64>();
                ps.push(Product {
                    price: 3.0 + 18.0 * t + rng.gen::<f64>() * 6.0,
                    delivery: 3.0 + 18.0 * (1.0 - t) + rng.gen::<f64>() * 6.0,
                });
            }
            ps
        }
        "near_tie" => (0..n)
            .map(|_| Product {
                price: 10.0 + rng.gen::<f64>() * 0.05,
                delivery: 10.0 + rng.gen::<f64>() * 0.05,
            })
            .collect(),
        _ => (0..n)
            .map(|_| Product {
                price: rng.gen::<f64>() * 20.0,
                delivery: rng.gen::<f64>() * 20.0,
            })
            .collect(),
    }
}

pub fn correctness() -> Result<(), String> {
    for mode in ["pareto", "random", "near_tie"] {
        let ps = gen_products(200, 99, mode);
        let cells = envelope(&ps);
        for k in 0..=200 {
            let w = k as f64 / 200.0;
            let brute = brute_winner(&ps, w).0;
            let cell = lookup_cell(&cells, w).ok_or_else(|| {
                format!("no cell for w={w} mode={mode} cells={}", cells.len())
            })?;
            if cell.winner != brute {
                // Near-ties may swap due to numeric endpoints; allow equal scores.
                let s_cell = score(ps[cell.winner], w);
                let s_brute = score(ps[brute], w);
                if (s_cell - s_brute).abs() > 1e-9 {
                    return Err(format!(
                        "winner mismatch mode={mode} w={w} cell={} brute={}",
                        cell.winner, brute
                    ));
                }
            }
        }
        let mut store = AnswerStore::new(ps.clone());
        for i in 0..ps.len() {
            let mut p = ps[i];
            p.price += 0.0001;
            store.update(i, p);
        }
        let w = 0.37;
        let got = store.query(w);
        let expect = brute_winner(&store.current, w).0;
        let s_got = score(store.current[got], w);
        let s_exp = score(store.current[expect], w);
        if (s_got - s_exp).abs() > 1e-9 {
            return Err(format!("store mismatch mode={mode}"));
        }
    }
    eprintln!("answer_cells correctness: envelope + drift certificates ok");
    Ok(())
}

pub fn run(quick: bool) -> Vec<Record> {
    let mut out = Vec::new();
    let n = if quick { 5_000usize } else { 20_000usize };
    let queries = if quick { 2_000usize } else { 12_000usize };
    let mut rng = SmallRng::seed_from_u64(123);

    for mode in ["pareto", "random", "near_tie"] {
        let ps = gen_products(n, 5, mode);
        let cells = envelope(&ps);
        let mut ws = vec![0.0; queries];
        for w in ws.iter_mut() {
            *w = if mode == "pareto" {
                0.45 + rng.gen::<f64>() * 0.1
            } else {
                rng.gen::<f64>()
            };
        }

        let (val, times) = time_ns(2, 6, || {
            let mut acc = 0usize;
            for &w in &ws {
                acc ^= brute_winner(&ps, w).0;
            }
            acc
        });
        out.push(record(
            "answer_cells",
            &format!("brute_scan_{mode}"),
            n as u64,
            json!({"queries": queries, "cells": cells.len()}),
            times,
            (n * queries) as u64,
            (n * queries * 16) as u64,
            json!({"xor": val}),
            "full scan per parameterized ranking",
        ));

        let store = AnswerStore::new(ps.clone());
        let cells_clone = store.cells.clone();
        let (val, times) = time_ns(3, 10, || {
            let mut acc = 0usize;
            for &w in &ws {
                acc ^= lookup_cell(&cells_clone, w).map(|c| c.winner).unwrap_or(0);
            }
            acc
        });
        out.push(record(
            "answer_cells",
            &format!("cell_lookup_{mode}"),
            n as u64,
            json!({"queries": queries, "cells": cells_clone.len()}),
            times,
            queries as u64,
            (queries * 32) as u64,
            json!({"xor": val, "cells": cells_clone.len()}),
            "O(log cells) exact winner; no data scan",
        ));

        // Mixed updates: small drift vs large invalidating updates.
        for (label, mag, frac) in [
            ("small_drift", 0.002, 0.05),
            ("large_drift", 3.0, 0.05),
        ] {
            let cells0 = envelope(&ps);
            let mut store = AnswerStore::new(ps.clone());
            let mut local = ps.clone();
            let (val, times) = time_ns(1, 4, || {
                store.reset_from(&ps, &cells0);
                local.copy_from_slice(&ps);
                let mut acc = 0usize;
                for (q, &w) in ws.iter().enumerate() {
                    if q % ((1.0 / frac) as usize).max(1) == 0 {
                        let i = q % n;
                        local[i].price += mag;
                        store.update(i, local[i]);
                    }
                    acc ^= store.query(w);
                }
                (acc, store.cell_hits, store.cell_misses, store.rebuilds, store.cells.len())
            });
            let hits = val.1;
            let misses = val.2;
            let rebuilds = val.3;
            let cell_n = val.4;
            let xor = val.0;
            out.push(record(
                "answer_cells",
                &format!("mixed_{label}_{mode}"),
                n as u64,
                json!({
                    "queries": queries,
                    "update_frac": frac,
                    "mag": mag,
                    "cells": cell_n
                }),
                times,
                queries as u64,
                0,
                json!({
                    "xor": xor,
                    "hits": hits,
                    "misses": misses,
                    "rebuilds": rebuilds,
                    "hit_rate": hits as f64 / (hits + misses).max(1) as f64
                }),
                "certificate reused until drift consumes the gap",
            ));
        }
    }
    out
}
