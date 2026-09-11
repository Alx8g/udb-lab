//! Answer cells: reuse an exact winner across a region of query weights
//! and a bounded envelope of data changes.
//!
//! score(k) = k*price + (W-k)*delivery, lower is better, then smaller index.
//! Prices and deliveries are integer millunits. Query weight is k/W.
//! Lower envelope of lines over w in [0,1] partitions weight space into
//! cells with a constant winner. A cell remains valid while every product's
//! L∞ drift is less than half the minimum gap on that cell.

use crate::stats::{record, time_ns, Record};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde_json::json;
use std::cmp::Ordering;

pub const W_DEN: i64 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Product {
    pub price: i64,
    pub delivery: i64,
}

#[derive(Clone, Copy, Debug)]
pub struct Frac {
    pub n: i128,
    pub d: i128,
}

impl Frac {
    pub fn new(n: i128, d: i128) -> Self {
        if d < 0 {
            Self { n: -n, d: -d }
        } else {
            Self { n, d }
        }
    }

    pub fn cmp(self, other: Self) -> Ordering {
        (self.n * other.d).cmp(&(other.n * self.d))
    }

    pub fn le_query(self, k: i64, wden: i64) -> bool {
        self.n * wden as i128 <= k as i128 * self.d
    }

    pub fn ge_query(self, k: i64, wden: i64) -> bool {
        self.n * wden as i128 >= k as i128 * self.d
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Cell {
    pub w_lo: Frac,
    pub w_hi: Frac,
    pub winner: usize,
    pub min_gap: i128,
}

pub fn score_k(p: Product, k: i64) -> i128 {
    k as i128 * p.price as i128 + (W_DEN - k) as i128 * p.delivery as i128
}

pub fn brute_winner(ps: &[Product], k: i64) -> (usize, i128) {
    let mut best_i = 0usize;
    let mut best = i128::MAX;
    let mut second = i128::MAX;
    for (i, &p) in ps.iter().enumerate() {
        let s = score_k(p, k);
        if s < best || (s == best && i < best_i) {
            if s != best {
                second = best;
            }
            best = s;
            best_i = i;
        } else if s < second {
            second = s;
        }
    }
    (best_i, second.saturating_sub(best))
}

fn intercept(p: Product) -> i64 {
    p.delivery
}

fn slope(p: Product) -> i64 {
    p.price - p.delivery
}

fn meet(ps: &[Product], i: usize, j: usize) -> Frac {
    let num = intercept(ps[i]) as i128 - intercept(ps[j]) as i128;
    let den = slope(ps[j]) as i128 - slope(ps[i]) as i128;
    Frac::new(num, den)
}

/// Lower envelope of lines `delivery + w*(price-delivery)` on [0,1].
/// Ties: equal scores keep the smaller index.
pub fn envelope(ps: &[Product]) -> Vec<Cell> {
    if ps.is_empty() {
        return Vec::new();
    }
    let mut idx: Vec<usize> = (0..ps.len()).collect();
    idx.sort_by(|&i, &j| {
        slope(ps[j])
            .cmp(&slope(ps[i]))
            .then_with(|| intercept(ps[i]).cmp(&intercept(ps[j])))
            .then_with(|| i.cmp(&j))
    });
    let mut uniq: Vec<usize> = Vec::new();
    for i in idx {
        if let Some(&last) = uniq.last() {
            if slope(ps[i]) == slope(ps[last]) {
                continue;
            }
        }
        uniq.push(i);
    }
    let mut hull: Vec<usize> = Vec::new();
    for i in uniq {
        while hull.len() >= 2 {
            let a = hull[hull.len() - 2];
            let b = hull[hull.len() - 1];
            if meet(ps, a, b).cmp(meet(ps, b, i)) != Ordering::Less {
                hull.pop();
            } else {
                break;
            }
        }
        hull.push(i);
    }
    let zero = Frac::new(0, 1);
    let one = Frac::new(1, 1);
    while hull.len() >= 2 && meet(ps, hull[0], hull[1]).cmp(zero) != Ordering::Greater {
        hull.remove(0);
    }
    while hull.len() >= 2
        && meet(ps, hull[hull.len() - 2], hull[hull.len() - 1]).cmp(one) != Ordering::Less
    {
        hull.pop();
    }
    let mut cells = Vec::new();
    for k in 0..hull.len() {
        let i = hull[k];
        let lo = if k == 0 {
            zero
        } else {
            meet(ps, hull[k - 1], i)
        };
        let hi = if k + 1 == hull.len() {
            one
        } else {
            meet(ps, i, hull[k + 1])
        };
        if hi.cmp(lo) != Ordering::Greater {
            continue;
        }
        let (k_lo, k_hi) = integer_span(lo, hi);
        // Endpoints have gap 0 by construction. Certificate uses interior points.
        let g = if k_hi > k_lo + 2 {
            let span = k_hi - k_lo;
            let mid = k_lo + span / 2;
            let a = k_lo + span / 4;
            let b = k_lo + (3 * span) / 4;
            brute_winner(ps, mid)
                .1
                .min(brute_winner(ps, a).1)
                .min(brute_winner(ps, b).1)
        } else {
            0
        };
        cells.push(Cell {
            w_lo: lo,
            w_hi: hi,
            winner: i,
            min_gap: g,
        });
    }
    if !cells.is_empty() {
        cells[0].w_lo = zero;
        let last = cells.len() - 1;
        cells[last].w_hi = one;
    }
    cells
}

fn integer_span(lo: Frac, hi: Frac) -> (i64, i64) {
    let k_lo = div_ceil(lo.n * W_DEN as i128, lo.d).clamp(0, W_DEN as i128) as i64;
    let k_hi = (hi.n * W_DEN as i128 / hi.d).clamp(0, W_DEN as i128) as i64;
    (k_lo, k_hi)
}

fn div_ceil(n: i128, d: i128) -> i128 {
    if n >= 0 {
        (n + d - 1) / d
    } else {
        n / d
    }
}

fn strictly_before_hi(cell: Cell, k: i64, last: bool) -> bool {
    if last {
        cell.w_hi.ge_query(k, W_DEN)
    } else {
        cell.w_hi.n * W_DEN as i128 > k as i128 * cell.w_hi.d
    }
}

pub fn lookup_cell(cells: &[Cell], k: i64) -> Option<(usize, Cell)> {
    if cells.is_empty() {
        return None;
    }
    let mut lo = 0usize;
    let mut hi = cells.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        let last = mid + 1 == cells.len();
        if !cells[mid].w_lo.le_query(k, W_DEN) {
            hi = mid;
        } else if !strictly_before_hi(cells[mid], k, last) {
            lo = mid + 1;
        } else {
            return Some((mid, cells[mid]));
        }
    }
    None
}

fn better(ps: &[Product], k: i64, i: usize, j: usize) -> usize {
    let si = score_k(ps[i], k);
    let sj = score_k(ps[j], k);
    if (sj, j) < (si, i) {
        j
    } else {
        i
    }
}

pub fn cell_winner(cells: &[Cell], ps: &[Product], k: i64) -> usize {
    let Some((idx, c)) = lookup_cell(cells, k) else {
        return brute_winner(ps, k).0;
    };
    let mut win = c.winner;
    if idx > 0 {
        let prev = cells[idx - 1];
        if prev.w_hi.n * W_DEN as i128 == k as i128 * prev.w_hi.d {
            win = better(ps, k, win, prev.winner);
        }
    }
    if idx + 1 < cells.len() {
        let nxt = cells[idx + 1];
        if nxt.w_lo.n * W_DEN as i128 == k as i128 * nxt.w_lo.d {
            win = better(ps, k, win, nxt.winner);
        }
    }
    // At w=0 / w=1 many dominated products can share the intercept.
    if k == 0 {
        let d = ps[win].delivery;
        for (i, p) in ps.iter().enumerate() {
            if p.delivery < d || (p.delivery == d && i < win) {
                win = i;
            }
        }
    } else if k == W_DEN {
        let pr = ps[win].price;
        for (i, p) in ps.iter().enumerate() {
            if p.price < pr || (p.price == pr && i < win) {
                win = i;
            }
        }
    }
    win
}

#[derive(Clone)]
pub struct AnswerStore {
    pub certified: Vec<Product>,
    pub current: Vec<Product>,
    pub cells: Vec<Cell>,
    pub max_drift: i64,
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
            max_drift: 0,
            rebuilds: 0,
            cell_hits: 0,
            cell_misses: 0,
        }
    }

    fn drift_of(cert: Product, cur: Product) -> i64 {
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
        // Each score moves by at most W_DEN * drift; relative gap shrinks by <= 2 W_DEN drift.
        (2 * self.max_drift as i128) * W_DEN as i128 + 1 < cell.min_gap
    }

    pub fn query(&mut self, k: i64) -> usize {
        if let Some((_, cell)) = lookup_cell(&self.cells, k) {
            if self.valid_for(cell) {
                self.cell_hits += 1;
                return cell_winner(&self.cells, &self.current, k);
            }
        }
        self.cell_misses += 1;
        self.rebuild();
        cell_winner(&self.cells, &self.current, k)
    }

    fn rebuild(&mut self) {
        self.certified = self.current.clone();
        self.cells = envelope(&self.current);
        self.max_drift = 0;
        self.rebuilds += 1;
    }

    fn reset_from(&mut self, ps: &[Product], cells: &[Cell]) {
        self.certified.copy_from_slice(ps);
        self.current.copy_from_slice(ps);
        self.cells.clear();
        self.cells.extend_from_slice(cells);
        self.max_drift = 0;
        self.rebuilds = 0;
        self.cell_hits = 0;
        self.cell_misses = 0;
    }
}

fn gen_products(n: usize, seed: u64, mode: &str) -> Vec<Product> {
    let mut rng = SmallRng::seed_from_u64(seed);
    match mode {
        "pareto" => {
            let front = (n / 40).clamp(8, 64);
            let den = (front as i64 - 1).max(1);
            let mut ps = Vec::with_capacity(n);
            for i in 0..front {
                let t = i as i64;
                ps.push(Product {
                    price: 1000 + 18000 * t / den,
                    delivery: 19000 - 18000 * t / den,
                });
            }
            while ps.len() < n {
                let t = rng.gen_range(0..1001);
                ps.push(Product {
                    price: 3000 + 18 * t + rng.gen_range(0..6001),
                    delivery: 3000 + 18 * (1000 - t) + rng.gen_range(0..6001),
                });
            }
            ps
        }
        "near_tie" => (0..n)
            .map(|_| Product {
                price: 10000 + rng.gen_range(0..50),
                delivery: 10000 + rng.gen_range(0..50),
            })
            .collect(),
        _ => (0..n)
            .map(|_| Product {
                price: rng.gen_range(0..20001),
                delivery: rng.gen_range(0..20001),
            })
            .collect(),
    }
}

pub fn correctness() -> Result<(), String> {
    for mode in ["pareto", "random", "near_tie"] {
        let ps = gen_products(200, 99, mode);
        let cells = envelope(&ps);
        if mode == "pareto" && cells.iter().any(|c| c.min_gap <= 0) {
            return Err("pareto envelope min_gap must be interior-positive".into());
        }
        for k in 0..=W_DEN {
            if k % 50 != 0 && mode != "near_tie" {
                continue;
            }
            if mode == "near_tie" && k % 200 != 0 {
                continue;
            }
            let brute = brute_winner(&ps, k).0;
            let got = cell_winner(&cells, &ps, k);
            if got != brute {
                let s_cell = score_k(ps[got], k);
                let s_brute = score_k(ps[brute], k);
                return Err(format!(
                    "winner mismatch mode={mode} k={k} cell={got} brute={brute} sc={s_cell} sb={s_brute}"
                ));
            }
        }
        let mut store = AnswerStore::new(ps.clone());
        for i in 0..ps.len() {
            let mut p = ps[i];
            p.price += 1;
            store.update(i, p);
        }
        let k = 3700;
        let got = store.query(k);
        let expect = brute_winner(&store.current, k).0;
        let s_got = score_k(store.current[got], k);
        let s_exp = score_k(store.current[expect], k);
        if got != expect {
            return Err(format!(
                "store mismatch mode={mode} got={got} expect={expect} sg={s_got} se={s_exp}"
            ));
        }
    }
    eprintln!("answer_cells correctness: integer millunit envelope + id tie-break");
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
        let mut ks = vec![0i64; queries];
        for k in ks.iter_mut() {
            *k = if mode == "pareto" {
                4500 + rng.gen_range(0..1001)
            } else {
                rng.gen_range(0..=W_DEN)
            };
        }

        let (val, times, reps) = time_ns(2, 6, || {
            let mut acc = 0usize;
            for &k in &ks {
                acc ^= brute_winner(&ps, k).0;
            }
            acc
        });
        out.push(record(
            "answer_cells",
            &format!("brute_scan_{mode}"),
            n as u64,
            json!({"queries": queries, "cells": cells.len(), "w_den": W_DEN}),
            times,
            reps,
            (n * queries) as u64,
            (n * queries * 16) as u64,
            json!({"xor": val}),
            "full scan per parameterized ranking; integer millunits",
        ));

        let store = AnswerStore::new(ps.clone());
        let cells_clone = store.cells.clone();
        let (val, times, reps) = time_ns(3, 10, || {
            let mut acc = 0usize;
            for &k in &ks {
                acc ^= cell_winner(&cells_clone, &ps, k);
            }
            acc
        });
        out.push(record(
            "answer_cells",
            &format!("cell_lookup_{mode}"),
            n as u64,
            json!({"queries": queries, "cells": cells_clone.len(), "w_den": W_DEN}),
            times,
            reps,
            queries as u64,
            (queries * 32) as u64,
            json!({"xor": val, "cells": cells_clone.len()}),
            "O(log cells) exact winner; smaller-index ties",
        ));

        for (label, mag, frac) in [("small_drift", 2i64, 0.05), ("large_drift", 3000i64, 0.05)] {
            let cells0 = envelope(&ps);
            let mut store = AnswerStore::new(ps.clone());
            let mut local = ps.clone();
            let (warmup, iters) = if mode == "near_tie" { (1, 1) } else { (1, 4) };
            let (val, times, reps) = time_ns(warmup, iters, || {
                store.reset_from(&ps, &cells0);
                local.copy_from_slice(&ps);
                let mut acc = 0usize;
                for (q, &k) in ks.iter().enumerate() {
                    if q % ((1.0 / frac) as usize).max(1) == 0 {
                        let i = q % n;
                        local[i].price += mag;
                        store.update(i, local[i]);
                    }
                    acc ^= store.query(k);
                }
                (
                    acc,
                    store.cell_hits,
                    store.cell_misses,
                    store.rebuilds,
                    store.cells.len(),
                )
            });
            out.push(record(
                "answer_cells",
                &format!("mixed_{label}_{mode}"),
                n as u64,
                json!({
                    "queries": queries,
                    "update_frac": frac,
                    "mag": mag,
                    "cells": val.4,
                    "w_den": W_DEN
                }),
                times,
                reps,
                queries as u64,
                0,
                json!({
                    "xor": val.0,
                    "hits": val.1,
                    "misses": val.2,
                    "rebuilds": val.3,
                    "hit_rate": val.1 as f64 / (val.1 + val.2).max(1) as f64
                }),
                "certificate reused until drift consumes the gap",
            ));
        }
    }
    out
}
