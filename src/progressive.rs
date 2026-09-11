//! Progressive revelation: store cheap bounds first, decode remainders
//! only when a decision still needs them.
//!
//! Value layout: u32 split into u16 bucket (hi) and u16 residual (lo).
//! Filter value > T:
//!   bucket_max <= T => reject without residual
//!   bucket_min >  T => accept without residual
//!   else decode.

use crate::stats::{record, time_ns, Record};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde_json::json;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

pub fn split(v: u32) -> (u16, u16) {
    ((v >> 16) as u16, v as u16)
}

pub fn join(hi: u16, lo: u16) -> u32 {
    ((hi as u32) << 16) | lo as u32
}

fn bucket_bounds(hi: u16) -> (u32, u32) {
    let min = (hi as u32) << 16;
    let max = min | 0xFFFF;
    (min, max)
}

pub fn filter_full(values: &[u32], t: u32) -> u64 {
    values.iter().filter(|&&v| v > t).count() as u64
}

pub fn filter_progressive(hi: &[u16], lo: &[u16], t: u32) -> u64 {
    let t_hi = (t >> 16) as u16;
    let mut c = 0u64;
    for i in 0..hi.len() {
        let h = hi[i];
        if h < t_hi {
            continue;
        }
        if h > t_hi {
            c += 1;
            continue;
        }
        let v = join(h, lo[i]);
        if v > t {
            c += 1;
        }
    }
    c
}

/// AVX2 first pass over buckets; scalar residual only on the boundary bucket.
#[cfg(target_arch = "x86_64")]
pub fn filter_progressive_avx2(hi: &[u16], lo: &[u16], t: u32) -> u64 {
    if !is_x86_feature_detected!("avx2") {
        return filter_progressive(hi, lo, t);
    }
    unsafe { filter_progressive_avx2_inner(hi, lo, t) }
}

#[cfg(not(target_arch = "x86_64"))]
pub fn filter_progressive_avx2(hi: &[u16], lo: &[u16], t: u32) -> u64 {
    filter_progressive(hi, lo, t)
}

#[cfg(target_arch = "x86_64")]
unsafe fn filter_progressive_avx2_inner(hi: &[u16], lo: &[u16], t: u32) -> u64 {
    let t_hi = (t >> 16) as u16;
    let n = hi.len();
    let mut c = 0u64;
    let mut i = 0usize;
    // Bias u16 into signed i16 so cmpgt is an unsigned compare.
    let bias = _mm256_set1_epi16(i16::MIN);
    let gt = _mm256_xor_si256(_mm256_set1_epi16(t_hi as i16), bias);
    let eq = _mm256_set1_epi16(t_hi as i16);
    while i + 16 <= n {
        let v = _mm256_loadu_si256(hi.as_ptr().add(i) as *const __m256i);
        let is_gt = _mm256_cmpgt_epi16(_mm256_xor_si256(v, bias), gt);
        let is_eq = _mm256_cmpeq_epi16(v, eq);
        let gt_mask = _mm256_movemask_epi8(is_gt) as u32;
        let eq_mask = _mm256_movemask_epi8(is_eq) as u32;
        c += gt_mask.count_ones() as u64 / 2;
        if eq_mask != 0 {
            for k in 0..16 {
                if (eq_mask >> (k * 2)) & 1 != 0 {
                    let v = join(hi[i + k], lo[i + k]);
                    if v > t {
                        c += 1;
                    }
                }
            }
        }
        i += 16;
    }
    while i < n {
        let h = hi[i];
        if h > t_hi {
            c += 1;
        } else if h == t_hi && join(h, lo[i]) > t {
            c += 1;
        }
        i += 1;
    }
    c
}

/// Ranking with bounds: keep the candidate with the best exact value.
/// Discard any item whose lower bound cannot beat the current best.
pub fn argmin_progressive(hi: &[u16], lo: &[u16]) -> usize {
    let mut best_i = 0usize;
    let mut best = join(hi[0], lo[0]);
    for i in 1..hi.len() {
        let (min_v, _max_v) = bucket_bounds(hi[i]);
        if min_v >= best {
            continue;
        }
        let v = join(hi[i], lo[i]);
        if v < best {
            best = v;
            best_i = i;
        }
    }
    best_i
}

pub fn argmin_full(values: &[u32]) -> usize {
    values
        .iter()
        .enumerate()
        .min_by_key(|(_, v)| *v)
        .map(|(i, _)| i)
        .unwrap_or(0)
}

const BLOCK: usize = 1024;

fn block_bounds(values: &[u32]) -> (Vec<u32>, Vec<u32>) {
    let mut mins = Vec::new();
    let mut maxs = Vec::new();
    for chunk in values.chunks(BLOCK) {
        let mut mn = u32::MAX;
        let mut mx = 0u32;
        for &v in chunk {
            mn = mn.min(v);
            mx = mx.max(v);
        }
        mins.push(mn);
        maxs.push(mx);
    }
    (mins, maxs)
}

fn filter_blocks(values: &[u32], mins: &[u32], maxs: &[u32], t: u32) -> u64 {
    let mut c = 0u64;
    for (b, chunk) in values.chunks(BLOCK).enumerate() {
        if maxs[b] <= t {
            continue;
        }
        if mins[b] > t {
            c += chunk.len() as u64;
            continue;
        }
        c += chunk.iter().filter(|&&v| v > t).count() as u64;
    }
    c
}

fn gen(n: usize, seed: u64, clustered: bool) -> Vec<u32> {
    let mut rng = SmallRng::seed_from_u64(seed);
    if clustered {
        // Physically grouped by value so a block min/max is tight.
        let mut v: Vec<u32> = (0..n)
            .map(|_| 1_000_000 + rng.gen_range(0..50_000))
            .collect();
        v.sort_unstable();
        v
    } else {
        (0..n).map(|_| rng.gen::<u32>() >> 4).collect()
    }
}

pub fn correctness() -> Result<(), String> {
    for clustered in [true, false] {
        let values = gen(10_000, 4, clustered);
        let hi: Vec<u16> = values.iter().map(|&v| split(v).0).collect();
        let lo: Vec<u16> = values.iter().map(|&v| split(v).1).collect();
        let (mins, maxs) = block_bounds(&values);
        for t in [0u32, 50_000, 1_010_000, 2_000_000, u32::MAX - 10] {
            let a = filter_full(&values, t);
            let b = filter_progressive(&hi, &lo, t);
            let c = filter_progressive_avx2(&hi, &lo, t);
            let d = filter_blocks(&values, &mins, &maxs, t);
            if a != b || a != c || a != d {
                return Err(format!(
                    "filter mismatch clustered={clustered} t={t} full={a} prog={b} avx={c} blk={d}"
                ));
            }
        }
        if argmin_full(&values) != argmin_progressive(&hi, &lo) {
            let i1 = argmin_full(&values);
            let i2 = argmin_progressive(&hi, &lo);
            if values[i1] != values[i2] {
                return Err("argmin mismatch".into());
            }
        }
    }
    eprintln!("progressive correctness: filters + argmin match full decode");
    Ok(())
}

pub fn run(quick: bool) -> Vec<Record> {
    let mut out = Vec::new();
    let n = if quick { 1_000_000usize } else { 20_000_000usize };
    for clustered in [true, false] {
        let values = gen(n, 9, clustered);
        let hi: Vec<u16> = values.iter().map(|&v| split(v).0).collect();
        let lo: Vec<u16> = values.iter().map(|&v| split(v).1).collect();
        let (mins, maxs) = block_bounds(&values);
        let label = if clustered { "clustered" } else { "spread" };

        let thresholds = if clustered {
            [1_000_000u32, 1_010_000, 1_025_000, 2_000_000]
        } else {
            [1_000u32, 1_000_000, 50_000_000, 200_000_000]
        };

        for t in thresholds {
            let sel = filter_full(&values, t) as f64 / n as f64;
            let (val, times) = time_ns(2, 6, || filter_full(&values, t));
            out.push(record(
                "progressive",
                &format!("full_scan_{label}"),
                n as u64,
                json!({"t": t, "selectivity": sel}),
                times,
                n as u64,
                (n * 4) as u64,
                json!({"count": val}),
                "decode every u32",
            ));

            let (val, times) = time_ns(2, 6, || filter_progressive(&hi, &lo, t));
            out.push(record(
                "progressive",
                &format!("bucket_scalar_{label}"),
                n as u64,
                json!({"t": t, "selectivity": sel}),
                times,
                n as u64,
                (n * 2) as u64,
                json!({"count": val}),
                "16-bit bounds first; residual on boundary bucket only",
            ));

            let (val, times) = time_ns(2, 6, || filter_progressive_avx2(&hi, &lo, t));
            out.push(record(
                "progressive",
                &format!("bucket_avx2_{label}"),
                n as u64,
                json!({"t": t, "selectivity": sel}),
                times,
                n as u64,
                (n * 2) as u64,
                json!({"count": val}),
                "per-row 16-bit bounds; still one access per record",
            ));

            let (val, times) = time_ns(2, 6, || filter_blocks(&values, &mins, &maxs, t));
            let skipped = mins
                .iter()
                .zip(maxs.iter())
                .filter(|(&mn, &mx)| mx <= t || mn > t)
                .count();
            out.push(record(
                "progressive",
                &format!("block_bounds_{label}"),
                n as u64,
                json!({"t": t, "selectivity": sel, "blocks_pruned": skipped, "blocks": mins.len()}),
                times,
                n as u64,
                ((mins.len() - skipped) * BLOCK * 4) as u64,
                json!({"count": val, "blocks_pruned": skipped}),
                "one min/max per 1024-row block; skip or accept whole blocks",
            ));
        }

        let (val, times) = time_ns(2, 6, || argmin_full(&values));
        out.push(record(
            "progressive",
            &format!("argmin_full_{label}"),
            n as u64,
            json!({}),
            times,
            n as u64,
            (n * 4) as u64,
            json!({"i": val}),
            "full value scan",
        ));
        let (val, times) = time_ns(2, 6, || argmin_progressive(&hi, &lo));
        out.push(record(
            "progressive",
            &format!("argmin_bounds_{label}"),
            n as u64,
            json!({}),
            times,
            n as u64,
            (n * 2) as u64,
            json!({"i": val}),
            "skip buckets whose min cannot beat current best",
        ));
    }
    out
}
