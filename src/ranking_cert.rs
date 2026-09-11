//! Ranking certificates: ordinary vs simplex-tight sufficient bounds.
//! Matches REPORT.md section 5 and certificate_coverage.py.
//! Construction scans competitors. Failure to certify is not a wrong answer.

use crate::stats::{record, Record};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde_json::json;

fn score(q: &[i64], z: &[i64]) -> i128 {
    q.iter()
        .zip(z.iter())
        .map(|(a, b)| *a as i128 * *b as i128)
        .sum()
}

fn winner(zs: &[Vec<i64>], q: &[i64]) -> usize {
    let mut best_i = 0usize;
    let mut best = i128::MAX;
    for (i, z) in zs.iter().enumerate() {
        let s = score(q, z);
        if s < best || (s == best && i < best_i) {
            best = s;
            best_i = i;
        }
    }
    best_i
}

/// Integer test from certificate_coverage.py, unnormalized weights summing to q1.
/// ordinary: r * ||d||_inf < m - 2 eps ||q||_1
/// simplex:  r * (max d - min d) < 2 (m - 2 eps ||q||_1)
fn ordinary_ok(m: i128, d: &[i64], r: i64, eps: i64, q1: i64) -> bool {
    let l = d.iter().map(|x| x.abs()).max().unwrap_or(0) as i128;
    let num = m - 2 * eps as i128 * q1 as i128;
    l > 0 && num > 0 && (r as i128) * l < num
}

fn simplex_ok(m: i128, d: &[i64], r: i64, eps: i64, q1: i64) -> bool {
    let mx = *d.iter().max().unwrap_or(&0);
    let mn = *d.iter().min().unwrap_or(&0);
    let num = (m - 2 * eps as i128 * q1 as i128) * 2;
    let den = (mx - mn) as i128;
    den > 0 && num > 0 && (r as i128) * den < num
}

pub fn correctness() -> Result<(), String> {
    let zs = [vec![10i64, 90], vec![40, 40], vec![90, 10]];
    let q0 = [50i64, 50];
    let k = winner(&zs, &q0);
    if k != 1 {
        return Err(format!("expected middle product to win, got {k}"));
    }
    let r = 10i64; // w in [45,55] of 100 is L1 10
    let eps = 2i64;
    let mut false_pos = 0u32;
    let mut certified = 0u32;
    let mut all_ok = true;
    for j in 0..3 {
        if j == k {
            continue;
        }
        let d: Vec<i64> = (0..2).map(|t| zs[j][t] - zs[k][t]).collect();
        let m = score(&q0, &d);
        if !simplex_ok(m, &d, r, eps, 100) {
            all_ok = false;
        }
    }
    for e0 in -2i32..=2 {
        for e1 in -2i32..=2 {
            for e2 in -2i32..=2 {
                for e3 in -2i32..=2 {
                    for e4 in -2i32..=2 {
                        for e5 in -2i32..=2 {
                            let cur = [
                                vec![10 + e0 as i64, 90 + e1 as i64],
                                vec![40 + e2 as i64, 40 + e3 as i64],
                                vec![90 + e4 as i64, 10 + e5 as i64],
                            ];
                            for wi in 45..=55 {
                                let q = [wi, 100 - wi];
                                let act = winner(&cur, &q);
                                if all_ok {
                                    certified += 1;
                                    if act != k {
                                        false_pos += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if false_pos != 0 {
        return Err(format!("false certificate on 3-product: {false_pos}"));
    }
    eprintln!("ranking_cert 3-product: certified={certified} false_pos=0 simplex_ok={all_ok}");
    Ok(())
}

pub fn run(quick: bool) -> Vec<Record> {
    let mut out = Vec::new();
    let n = if quick { 512usize } else { 4_096usize };
    let trials = if quick { 20usize } else { 50usize };
    let r = 50_000i64; // normalized 0.05 of W=1_000_000
    let eps = 100i64;
    let w = 1_000_000i64;
    let mut rng = SmallRng::seed_from_u64(11);

    for dim in [2usize, 8, 32] {
        let t0 = std::time::Instant::now();
        let mut ord_c = 0u32;
        let mut sim_c = 0u32;
        let mut actual = 0u32;
        let mut false_pos = 0u32;
        let q0: Vec<i64> = vec![w / dim as i64; dim];
        let q1: i64 = q0.iter().sum();
        for _ in 0..trials {
            let zs: Vec<Vec<i64>> = (0..n)
                .map(|_| (0..dim).map(|_| rng.gen_range(0..=1_000_000)).collect())
                .collect();
            let k = winner(&zs, &q0);
            let mut ord = true;
            let mut sim = true;
            for (j, z) in zs.iter().enumerate() {
                if j == k {
                    continue;
                }
                let d: Vec<i64> = (0..dim).map(|t| z[t] - zs[k][t]).collect();
                let m = score(&q0, &d);
                if !ordinary_ok(m, &d, r, eps, q1) {
                    ord = false;
                }
                if !simplex_ok(m, &d, r, eps, q1) {
                    sim = false;
                }
            }
            let mut q = q0.clone();
            let a = rng.gen_range(0..dim);
            let mut b = rng.gen_range(0..dim);
            while b == a {
                b = rng.gen_range(0..dim);
            }
            q[a] += r / 2;
            q[b] -= r / 2;
            let zs2: Vec<Vec<i64>> = zs
                .iter()
                .map(|z| z.iter().map(|&x| x + rng.gen_range(-eps..=eps)).collect())
                .collect();
            let act = winner(&zs2, &q);
            if ord {
                ord_c += 1;
            }
            if sim {
                sim_c += 1;
            }
            if act == k {
                actual += 1;
            }
            if (ord || sim) && act != k {
                false_pos += 1;
            }
        }
        let ns = t0.elapsed().as_secs_f64() * 1e9;
        out.push(record(
            "ranking_cert",
            &format!("coverage_dim{dim}"),
            n as u64,
            json!({
                "dim": dim,
                "trials": trials,
                "r": r,
                "eps": eps,
                "ordinary_certified": ord_c,
                "simplex_certified": sim_c,
                "actual_unchanged": actual,
                "false_pos": false_pos
            }),
            vec![ns],
            1,
            trials as u64,
            0,
            json!({
                "ordinary": ord_c,
                "simplex": sim_c,
                "actual": actual,
                "false_pos": false_pos
            }),
            "sufficient bounds vs actual stability; high dim should reject many true stables",
        ));
    }
    out
}
