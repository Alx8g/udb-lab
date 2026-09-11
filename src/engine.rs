//! Engine-side remaining work: layout, SIMD, and batched point lookup.
//!
//! This is the other half of the goal. After work elimination, the leftover
//! scans and lookups still have to be fast. Measures:
//!   - 4,000 known-key lookups vs 4,000 independent binary searches
//!   - row-wise vs column-wise predicate + projection
//!   - scalar vs AVX2 column scan
//!   - payload size vs latency (the 20ms / 4k people bound)

use crate::stats::{record, time_ns, Record};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde_json::json;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

#[repr(C)]
#[derive(Clone, Copy)]
struct Row {
    id: u64,
    price: u32,
    qty: u32,
    flags: u32,
    pad: u32,
}

fn gen_rows(n: usize, seed: u64) -> Vec<Row> {
    let mut rng = SmallRng::seed_from_u64(seed);
    (0..n)
        .map(|i| Row {
            id: i as u64,
            price: rng.gen_range(1..10_000),
            qty: rng.gen_range(0..500),
            flags: rng.gen_range(0..16),
            pad: 0,
        })
        .collect()
}

fn scan_row(rows: &[Row], min_price: u32) -> (u64, u64) {
    let mut c = 0u64;
    let mut s = 0u64;
    for r in rows {
        if r.price > min_price && r.qty > 0 {
            c += 1;
            s += r.price as u64;
        }
    }
    (c, s)
}

fn scan_col(price: &[u32], qty: &[u32], min_price: u32) -> (u64, u64) {
    let mut c = 0u64;
    let mut s = 0u64;
    for i in 0..price.len() {
        if price[i] > min_price && qty[i] > 0 {
            c += 1;
            s += price[i] as u64;
        }
    }
    (c, s)
}

#[cfg(target_arch = "x86_64")]
fn scan_col_avx2(price: &[u32], qty: &[u32], min_price: u32) -> (u64, u64) {
    if !is_x86_feature_detected!("avx2") {
        return scan_col(price, qty, min_price);
    }
    unsafe { scan_col_avx2_inner(price, qty, min_price) }
}

#[cfg(not(target_arch = "x86_64"))]
fn scan_col_avx2(price: &[u32], qty: &[u32], min_price: u32) -> (u64, u64) {
    scan_col(price, qty, min_price)
}

#[cfg(target_arch = "x86_64")]
unsafe fn scan_col_avx2_inner(price: &[u32], qty: &[u32], min_price: u32) -> (u64, u64) {
    let n = price.len();
    let mut i = 0usize;
    let mut count = 0u64;
    let mut sum = 0u64;
    let minv = _mm256_set1_epi32(min_price as i32);
    let zero = _mm256_setzero_si256();
    while i + 8 <= n {
        let p = _mm256_loadu_si256(price.as_ptr().add(i) as *const __m256i);
        let q = _mm256_loadu_si256(qty.as_ptr().add(i) as *const __m256i);
        let gt = _mm256_cmpgt_epi32(p, minv);
        let qnz = _mm256_cmpgt_epi32(q, zero);
        let m = _mm256_and_si256(gt, qnz);
        let mask = _mm256_movemask_epi8(m) as u32;
        if mask != 0 {
            let kept = _mm256_and_si256(p, m);
            let mut tmp = [0i32; 8];
            _mm256_storeu_si256(tmp.as_mut_ptr() as *mut __m256i, kept);
            for x in tmp {
                if x > 0 {
                    count += 1;
                    sum += x as u64;
                }
            }
        }
        i += 8;
    }
    while i < n {
        if price[i] > min_price && qty[i] > 0 {
            count += 1;
            sum += price[i] as u64;
        }
        i += 1;
    }
    (count, sum)
}

fn gather_ids(ids: &[u64], keys: &[u64]) -> u64 {
    let mut s = 0u64;
    for &k in keys {
        if let Ok(i) = ids.binary_search(&k) {
            s ^= ids[i];
        }
    }
    s
}

/// Sorted 4000-key batch: merge-walk a sorted id column once.
fn batch_merge(ids: &[u64], mut keys: Vec<u64>) -> u64 {
    keys.sort_unstable();
    keys.dedup();
    let mut i = 0usize;
    let mut s = 0u64;
    for k in keys {
        while i < ids.len() && ids[i] < k {
            i += 1;
        }
        if i < ids.len() && ids[i] == k {
            s ^= ids[i];
            i += 1;
        }
    }
    s
}

/// Hash probe of 4000 keys into a dense id->row table (ids are 0..n).
fn batch_direct(payload: &[u32], keys: &[u64]) -> u64 {
    let mut s = 0u64;
    let n = payload.len() as u64;
    for &k in keys {
        if k < n {
            s += payload[k as usize] as u64;
        }
    }
    s
}

pub fn correctness() -> Result<(), String> {
    let rows = gen_rows(5_000, 1);
    let price: Vec<u32> = rows.iter().map(|r| r.price).collect();
    let qty: Vec<u32> = rows.iter().map(|r| r.qty).collect();
    let a = scan_row(&rows, 100);
    let b = scan_col(&price, &qty, 100);
    let c = scan_col_avx2(&price, &qty, 100);
    if a != b || a != c {
        return Err(format!("scan mismatch row={a:?} col={b:?} avx={c:?}"));
    }
    let ids: Vec<u64> = (0..5_000).collect();
    let keys = vec![0, 10, 11, 4999, 9_999];
    let g = gather_ids(&ids, &keys);
    let m = batch_merge(&ids, keys.clone());
    if g != m {
        return Err("batch lookup mismatch".into());
    }
    eprintln!("engine correctness: row/col/avx2 scans and batch lookup match");
    Ok(())
}

pub fn run(quick: bool) -> Vec<Record> {
    let mut out = Vec::new();
    let n = if quick { 1_000_000usize } else { 20_000_000usize };
    let rows = gen_rows(n, 7);
    let price: Vec<u32> = rows.iter().map(|r| r.price).collect();
    let qty: Vec<u32> = rows.iter().map(|r| r.qty).collect();
    let mut rng = SmallRng::seed_from_u64(21);

    let (val, times) = time_ns(2, 5, || scan_row(&rows, 2500));
    out.push(record(
        "engine",
        "row_scan_predicate",
        n as u64,
        json!({"min_price": 2500, "count": val.0}),
        times,
        n as u64,
        (n * std::mem::size_of::<Row>()) as u64,
        json!({"count": val.0, "sum": val.1}),
        "AoS: filter+project touches full rows",
    ));

    let (val, times) = time_ns(2, 5, || scan_col(&price, &qty, 2500));
    out.push(record(
        "engine",
        "col_scan_scalar",
        n as u64,
        json!({"min_price": 2500, "count": val.0}),
        times,
        n as u64,
        (n * 8) as u64,
        json!({"count": val.0, "sum": val.1}),
        "SoA: only the two referenced columns",
    ));

    let (val, times) = time_ns(2, 5, || scan_col_avx2(&price, &qty, 2500));
    out.push(record(
        "engine",
        "col_scan_avx2",
        n as u64,
        json!({"min_price": 2500, "count": val.0}),
        times,
        n as u64,
        (n * 8) as u64,
        json!({"count": val.0, "sum": val.1}),
        "same columns, vectorized remaining work",
    ));

    // 4000-key batch, several n to show data-size independence of a covering index.
    let ns: Vec<usize> = if quick {
        vec![n]
    } else {
        vec![1_000_000, n]
    };
    for nn in ns {
        let ids_n: Vec<u64> = (0..nn as u64).collect();
        let payload: Vec<u32> = (0..nn).map(|i| (i as u32).wrapping_mul(17)).collect();
        let keys: Vec<u64> = (0..4_000)
            .map(|_| rng.gen_range(0..nn as u64))
            .collect();

        let (val, times) = time_ns(3, 10, || gather_ids(&ids_n, &keys));
        out.push(record(
            "engine",
            "lookup_4k_independent_bsearch",
            nn as u64,
            json!({"k": 4000}),
            times,
            4000,
            4000 * 64,
            json!({"xor": val}),
            "4,000 separate binary searches",
        ));

        let (val, times) = time_ns(3, 10, || batch_merge(&ids_n, keys.clone()));
        out.push(record(
            "engine",
            "lookup_4k_sorted_merge",
            nn as u64,
            json!({"k": 4000}),
            times,
            4000,
            0,
            json!({"xor": val}),
            "sort keys, one forward scan of the id column",
        ));

        let (val, times) = time_ns(4, 16, || batch_direct(&payload, &keys));
        out.push(record(
            "engine",
            "lookup_4k_direct_id",
            nn as u64,
            json!({"k": 4000}),
            times,
            4000,
            4000 * 4,
            json!({"sum": val}),
            "dense primary key: 4,000 direct loads",
        ));
    }

    // Payload arithmetic for the 20ms claim (not a timed benchmark).
    for bytes_per in [256u64, 4096] {
        let total = 4000 * bytes_per;
        let t_1gbit_ms = (total as f64 * 8.0) / 1e9 * 1e3;
        let t_10gbit_ms = (total as f64 * 8.0) / 1e10 * 1e3;
        out.push(record(
            "engine",
            "payload_bound",
            4000,
            json!({
                "bytes_per_row": bytes_per,
                "total_bytes": total,
                "ms_at_1gbit_ideal": t_1gbit_ms,
                "ms_at_10gbit_ideal": t_10gbit_ms
            }),
            vec![0],
            4000,
            total,
            json!({}),
            "network payload lower bound; independent of engine speed",
        ));
    }
    out
}
