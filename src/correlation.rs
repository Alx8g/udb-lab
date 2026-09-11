//! Lossless correlation representation: store a model + residuals, then
//! prune with the same model.
//!
//! Natural law: delivery ≈ a + b * price, plus residual.
//! Storage: (a, b, residual[]) instead of (price, delivery).
//! Filter delivery < T becomes a bound on residual given price, or a bound
//! on price given residual extrema.
//!
//! Negative control: shuffled delivery (no correlation) — model still
//! lossless, but residuals are large and pruning dies.

use crate::stats::{record, time_ns, Record};
use rand::rngs::SmallRng;
use rand::{seq::SliceRandom, Rng, SeedableRng};
use serde_json::json;

#[derive(Clone)]
struct Col {
    price: Vec<f64>,
    delivery: Vec<f64>,
}

const BLOCK: usize = 1024;

#[derive(Clone)]
struct Residual {
    a: f64,
    b: f64,
    price: Vec<f64>,
    resid: Vec<f64>,
    resid_min: f64,
    resid_max: f64,
    block_dmin: Vec<f64>,
    block_dmax: Vec<f64>,
}

fn fit(price: &[f64], delivery: &[f64]) -> (f64, f64) {
    let n = price.len() as f64;
    let mx = price.iter().sum::<f64>() / n;
    let my = delivery.iter().sum::<f64>() / n;
    let mut num = 0.0;
    let mut den = 0.0;
    for i in 0..price.len() {
        let dx = price[i] - mx;
        num += dx * (delivery[i] - my);
        den += dx * dx;
    }
    let b = if den.abs() < 1e-18 { 0.0 } else { num / den };
    let a = my - b * mx;
    (a, b)
}

fn encode(c: &Col) -> Residual {
    let (a, b) = fit(&c.price, &c.delivery);
    let resid: Vec<f64> = c
        .price
        .iter()
        .zip(c.delivery.iter())
        .map(|(&p, &d)| d - (a + b * p))
        .collect();
    let resid_min = resid.iter().copied().fold(f64::INFINITY, f64::min);
    let resid_max = resid.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let mut block_dmin = Vec::new();
    let mut block_dmax = Vec::new();
    for chunk in c.delivery.chunks(BLOCK) {
        let mut mn = f64::INFINITY;
        let mut mx = f64::NEG_INFINITY;
        for &d in chunk {
            mn = mn.min(d);
            mx = mx.max(d);
        }
        block_dmin.push(mn);
        block_dmax.push(mx);
    }
    Residual {
        a,
        b,
        price: c.price.clone(),
        resid,
        resid_min,
        resid_max,
        block_dmin,
        block_dmax,
    }
}

fn filter_delivery_blocks(c: &Col, r: &Residual, t: f64) -> u64 {
    let mut n = 0u64;
    for (b, chunk) in c.delivery.chunks(BLOCK).enumerate() {
        if r.block_dmin[b] >= t {
            continue;
        }
        if r.block_dmax[b] < t {
            n += chunk.len() as u64;
            continue;
        }
        n += chunk.iter().filter(|&&d| d < t).count() as u64;
    }
    n
}

fn filter_delivery_full(c: &Col, t: f64) -> u64 {
    c.delivery.iter().filter(|&&d| d < t).count() as u64
}

fn filter_delivery_residual(r: &Residual, t: f64) -> u64 {
    let mut c = 0u64;
    for i in 0..r.price.len() {
        let pred_max = r.a + r.b * r.price[i] + r.resid_max;
        let pred_min = r.a + r.b * r.price[i] + r.resid_min;
        if pred_min >= t {
            continue;
        }
        if pred_max < t {
            c += 1;
            continue;
        }
        let d = r.a + r.b * r.price[i] + r.resid[i];
        if d < t {
            c += 1;
        }
    }
    c
}

/// Tighter: use the actual residual, but skip the reconstructed delivery
/// load when the per-row residual bound is enough. Here the global residual
/// range is the bound. A per-row residual is still needed unless the bound
/// is tight. Measure bytes: we always read price + maybe residual.
fn filter_delivery_decode(r: &Residual, t: f64) -> u64 {
    let mut c = 0u64;
    for i in 0..r.price.len() {
        let d = r.a + r.b * r.price[i] + r.resid[i];
        if d < t {
            c += 1;
        }
    }
    c
}

fn gen_correlated(n: usize, noise: f64, seed: u64) -> Col {
    let mut rng = SmallRng::seed_from_u64(seed);
    let mut price = Vec::with_capacity(n);
    let mut delivery = Vec::with_capacity(n);
    for _ in 0..n {
        let p = rng.gen::<f64>() * 100.0;
        let d = 80.0 - 0.6 * p + rng.gen::<f64>() * noise;
        price.push(p);
        delivery.push(d.max(0.0));
    }
    Col { price, delivery }
}

pub fn correctness() -> Result<(), String> {
    for noise in [0.01, 5.0, 40.0] {
        let c = gen_correlated(5_000, noise, 2);
        let r = encode(&c);
        for t in [10.0, 30.0, 50.0, 70.0] {
            let a = filter_delivery_full(&c, t);
            let b = filter_delivery_residual(&r, t);
            let d = filter_delivery_decode(&r, t);
            let e = filter_delivery_blocks(&c, &r, t);
            if a != b || a != d || a != e {
                return Err(format!(
                    "noise={noise} t={t} full={a} bound={b} dec={d} blk={e}"
                ));
            }
        }
        for i in 0..c.price.len() {
            let rec = r.a + r.b * r.price[i] + r.resid[i];
            if (rec - c.delivery[i]).abs() > 1e-9 {
                return Err("lossy reconstruction".into());
            }
        }
    }
    eprintln!("correlation correctness: lossless reconstruct + exact filters");
    Ok(())
}

pub fn run(quick: bool) -> Vec<Record> {
    let mut out = Vec::new();
    let n = if quick { 400_000usize } else { 8_000_000usize };
    for (label, noise) in [("tight", 0.05), ("moderate", 4.0), ("none", 80.0)] {
        let mut c = gen_correlated(n, noise, 8);
        if label == "none" {
            c.delivery.shuffle(&mut SmallRng::seed_from_u64(99));
        } else {
            let mut pairs: Vec<(f64, f64)> = c
                .price
                .iter()
                .copied()
                .zip(c.delivery.iter().copied())
                .collect();
            pairs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            c.price = pairs.iter().map(|p| p.0).collect();
            c.delivery = pairs.iter().map(|p| p.1).collect();
        }
        let r = encode(&c);
        let t = 40.0;
        let sel = filter_delivery_full(&c, t) as f64 / n as f64;

        let (val, times, reps) = time_ns(2, 6, || filter_delivery_full(&c, t));
        out.push(record(
            "correlation",
            &format!("full_columns_{label}"),
            n as u64,
            json!({"noise": noise, "t": t, "selectivity": sel, "resid_span": r.resid_max - r.resid_min}),
            times,
           reps,
            n as u64,
            (n * 16) as u64,
            json!({"count": val, "resid_span": r.resid_max - r.resid_min}),
            "scan stored delivery",
        ));

        let (val, times, reps) = time_ns(2, 6, || filter_delivery_residual(&r, t));
        out.push(record(
            "correlation",
            &format!("model_prune_{label}"),
            n as u64,
            json!({"noise": noise, "t": t, "selectivity": sel, "resid_span": r.resid_max - r.resid_min}),
            times,
           reps,
            n as u64,
            (n * 8) as u64,
            json!({"count": val}),
            "global residual envelope; decode only the uncertain band",
        ));

        let (val, times, reps) = time_ns(2, 6, || filter_delivery_decode(&r, t));
        out.push(record(
            "correlation",
            &format!("decode_all_{label}"),
            n as u64,
            json!({"noise": noise, "t": t}),
            times,
            reps,
            n as u64,
            (n * 16) as u64,
            json!({"count": val}),
            "lossless but reconstructs every delivery; storage win, scan not cheaper",
        ));

        let (val, times, reps) = time_ns(2, 6, || filter_delivery_blocks(&c, &r, t));
        out.push(record(
            "correlation",
            &format!("block_bounds_{label}"),
            n as u64,
            json!({"noise": noise, "t": t, "resid_span": r.resid_max - r.resid_min}),
            times,
            reps,
            n as u64,
            (n * 8) as u64,
            json!({"count": val}),
            "min/max delivery per 1024-row block; same idea as progressive, on the stored column",
        ));
    }
    out
}
