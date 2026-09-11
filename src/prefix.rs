//! Prefix aggregate SUM(value) WHERE dense_unique_key <= T.
//! Matches database_theory_round1/src/engine_lab.cpp scan_lab:
//! row vs column, 512-row block summaries, Fenwick specialist.
//! Updates are value += delta at a row index; keys stay unique and dense.

use crate::stats::{record, time_ns_setup, Record};
use rand::rngs::SmallRng;
use rand::{seq::SliceRandom, Rng, SeedableRng};
use serde_json::json;

const BLOCK: usize = 512;

#[repr(C)]
#[derive(Clone, Copy)]
struct Row {
    key: u32,
    value: i32,
    pad: [u8; 56],
}

#[derive(Clone, Copy)]
struct Block {
    lo: u32,
    hi: u32,
    sum: i64,
}

fn make_blocks(keys: &[u32], vals: &[i32]) -> Vec<Block> {
    let mut bs = Vec::new();
    let mut i = 0usize;
    while i < keys.len() {
        let end = (i + BLOCK).min(keys.len());
        let mut lo = u32::MAX;
        let mut hi = 0u32;
        let mut s = 0i64;
        for j in i..end {
            lo = lo.min(keys[j]);
            hi = hi.max(keys[j]);
            s += vals[j] as i64;
        }
        bs.push(Block { lo, hi, sum: s });
        i = end;
    }
    bs
}

fn scan_rows(rows: &[Row], t: u32) -> i64 {
    let mut s = 0i64;
    for r in rows {
        if r.key <= t {
            s += r.value as i64;
        }
    }
    s
}

fn scan_cols(keys: &[u32], vals: &[i32], t: u32) -> i64 {
    let mut s = 0i64;
    for i in 0..keys.len() {
        if keys[i] <= t {
            s += vals[i] as i64;
        }
    }
    s
}

fn block_query_rows(rows: &[Row], bs: &[Block], t: u32) -> i64 {
    let mut s = 0i64;
    for (g, b) in bs.iter().enumerate() {
        if b.lo > t {
            continue;
        }
        if b.hi <= t {
            s += b.sum;
            continue;
        }
        let start = g * BLOCK;
        let end = (start + BLOCK).min(rows.len());
        for r in &rows[start..end] {
            if r.key <= t {
                s += r.value as i64;
            }
        }
    }
    s
}

fn block_query_cols(keys: &[u32], vals: &[i32], bs: &[Block], t: u32) -> i64 {
    let mut s = 0i64;
    for (g, b) in bs.iter().enumerate() {
        if b.lo > t {
            continue;
        }
        if b.hi <= t {
            s += b.sum;
            continue;
        }
        let start = g * BLOCK;
        let end = (start + BLOCK).min(keys.len());
        for j in start..end {
            if keys[j] <= t {
                s += vals[j] as i64;
            }
        }
    }
    s
}

#[derive(Clone)]
struct Fenwick {
    t: Vec<i64>,
}

impl Fenwick {
    fn new(keys: &[u32], vals: &[i32]) -> Self {
        let n = keys.len();
        let mut t = vec![0i64; n + 1];
        for i in 0..n {
            t[keys[i] as usize + 1] = vals[i] as i64;
        }
        for i in 1..t.len() {
            let p = i + (i & i.wrapping_neg());
            if p < t.len() {
                t[p] += t[i];
            }
        }
        Self { t }
    }
    fn add(&mut self, mut x: usize, d: i32) {
        x += 1;
        while x < self.t.len() {
            self.t[x] += d as i64;
            x += x & x.wrapping_neg();
        }
    }
    fn get(&self, mut x: usize) -> i64 {
        let mut r = 0i64;
        x += 1;
        while x > 0 {
            r += self.t[x];
            x -= x & x.wrapping_neg();
        }
        r
    }
}

#[derive(Clone, Copy)]
struct Op {
    query: bool,
    at: u32,
    delta: i32,
}

pub fn correctness() -> Result<(), String> {
    let n = 4_096usize;
    let mut keys: Vec<u32> = (0..n as u32).collect();
    keys.shuffle(&mut SmallRng::seed_from_u64(1));
    let mut vals: Vec<i32> = (0..n).map(|i| (i as i32) % 17 - 8).collect();
    let mut rows: Vec<Row> = keys
        .iter()
        .zip(vals.iter())
        .map(|(&k, &v)| Row {
            key: k,
            value: v,
            pad: [0; 56],
        })
        .collect();
    let mut bs = make_blocks(&keys, &vals);
    let mut fw = Fenwick::new(&keys, &vals);
    let ops = [
        Op {
            query: false,
            at: 10,
            delta: 3,
        },
        Op {
            query: true,
            at: 100,
            delta: 0,
        },
        Op {
            query: false,
            at: 2000,
            delta: -5,
        },
        Op {
            query: true,
            at: 4095,
            delta: 0,
        },
    ];
    for o in ops {
        if !o.query {
            let i = o.at as usize;
            rows[i].value += o.delta;
            vals[i] += o.delta;
            bs[i / BLOCK].sum += o.delta as i64;
            fw.add(keys[i] as usize, o.delta);
        } else {
            let a = scan_rows(&rows, o.at);
            let b = scan_cols(&keys, &vals, o.at);
            let c = block_query_rows(&rows, &bs, o.at);
            let d = block_query_cols(&keys, &vals, &bs, o.at);
            let e = fw.get(o.at as usize);
            if a != b || a != c || a != d || a != e {
                return Err(format!("prefix mismatch t={} {a} {b} {c} {d} {e}", o.at));
            }
        }
    }
    eprintln!("prefix correctness: row/col/block/fenwick agree after value updates");
    Ok(())
}

pub fn run(quick: bool) -> Vec<Record> {
    let mut out = Vec::new();
    let n = if quick { 1 << 16 } else { 1 << 21 };
    let nq = if quick { 16usize } else { 48usize };
    let upq = 4usize;
    let mut rng = SmallRng::seed_from_u64(5);

    for (label, clustered) in [("clustered", true), ("shuffled", false)] {
        let mut keys: Vec<u32> = (0..n as u32).collect();
        if !clustered {
            keys.shuffle(&mut rng);
        }
        let initial: Vec<i32> = (0..n).map(|_| rng.gen_range(-1000..=1000)).collect();
        let mut ops = Vec::new();
        for _ in 0..nq {
            for _ in 0..upq {
                ops.push(Op {
                    query: false,
                    at: rng.gen_range(0..n as u32),
                    delta: rng.gen_range(-10..=10),
                });
            }
            ops.push(Op {
                query: true,
                at: (n / 20 + rng.gen_range(0..(9 * n / 10))) as u32,
                delta: 0,
            });
        }

        let rows0: Vec<Row> = keys
            .iter()
            .zip(initial.iter())
            .map(|(&k, &v)| Row {
                key: k,
                value: v,
                pad: [7; 56],
            })
            .collect();
        let bs0 = make_blocks(&keys, &initial);
        let fw0 = Fenwick::new(&keys, &initial);

        let (val, times, reps) = time_ns_setup(
            1,
            3,
            || rows0.clone(),
            |rows| {
                let mut acc = 0i64;
                for o in &ops {
                    if o.query {
                        acc ^= scan_rows(rows, o.at);
                    } else {
                        rows[o.at as usize].value += o.delta;
                    }
                }
                acc
            },
        );
        out.push(record(
            "prefix",
            &format!("row_scan_{label}"),
            n as u64,
            json!({"queries": nq, "updates": nq * upq}),
            times,
            reps,
            (n * nq) as u64,
            (n * nq * 64) as u64,
            json!({"xor": val}),
            "64-byte rows, scan every record",
        ));

        let (val, times, reps) = time_ns_setup(
            1,
            3,
            || initial.clone(),
            |v| {
                let mut acc = 0i64;
                for o in &ops {
                    if o.query {
                        acc ^= scan_cols(&keys, v, o.at);
                    } else {
                        v[o.at as usize] += o.delta;
                    }
                }
                acc
            },
        );
        out.push(record(
            "prefix",
            &format!("col_scan_{label}"),
            n as u64,
            json!({"queries": nq, "updates": nq * upq}),
            times,
            reps,
            (n * nq) as u64,
            (n * nq * 8) as u64,
            json!({"xor": val}),
            "8-byte columns, still every record",
        ));

        let (val, times, reps) = time_ns_setup(
            1,
            3,
            || (rows0.clone(), bs0.clone()),
            |(rows, bs)| {
                let mut acc = 0i64;
                for o in &ops {
                    if o.query {
                        acc ^= block_query_rows(rows, bs, o.at);
                    } else {
                        let i = o.at as usize;
                        rows[i].value += o.delta;
                        bs[i / BLOCK].sum += o.delta as i64;
                    }
                }
                acc
            },
        );
        out.push(record(
            "prefix",
            &format!("row_blocks_{label}"),
            n as u64,
            json!({"queries": nq, "updates": nq * upq, "block": BLOCK}),
            times,
            reps,
            nq as u64,
            0,
            json!({"xor": val}),
            "512-row min/max/sum; clustered can skip, shuffled cannot",
        ));

        let (val, times, reps) = time_ns_setup(
            1,
            3,
            || (initial.clone(), bs0.clone()),
            |(v, bs)| {
                let mut acc = 0i64;
                for o in &ops {
                    if o.query {
                        acc ^= block_query_cols(&keys, v, bs, o.at);
                    } else {
                        let i = o.at as usize;
                        v[i] += o.delta;
                        bs[i / BLOCK].sum += o.delta as i64;
                    }
                }
                acc
            },
        );
        out.push(record(
            "prefix",
            &format!("col_blocks_{label}"),
            n as u64,
            json!({"queries": nq, "updates": nq * upq, "block": BLOCK}),
            times,
            reps,
            nq as u64,
            0,
            json!({"xor": val}),
            "column layout plus block summaries",
        ));

        let (build_n, build_times, build_reps) =
            crate::stats::time_ns(2, 5, || Fenwick::new(&keys, &initial).t.len());
        out.push(record(
            "prefix",
            &format!("fenwick_build_{label}"),
            n as u64,
            json!({"index_bytes": (n + 1) * 8}),
            build_times,
            build_reps,
            n as u64,
            (n * 8) as u64,
            json!({"nodes": build_n}),
            "Fenwick construction, charged separately from the stream",
        ));

        let (val, times, reps) = time_ns_setup(
            2,
            5,
            || fw0.clone(),
            |fw| {
                let mut acc = 0i64;
                for o in &ops {
                    if o.query {
                        acc ^= fw.get(o.at as usize);
                    } else {
                        let i = o.at as usize;
                        fw.add(keys[i] as usize, o.delta);
                    }
                }
                acc
            },
        );
        out.push(record(
            "prefix",
            &format!("fenwick_{label}"),
            n as u64,
            json!({"queries": nq, "updates": nq * upq, "index_bytes": (n + 1) * 8}),
            times,
            reps,
            (nq as u64) * ((n as f64).log2() as u64),
            0,
            json!({"xor": val}),
            "specialist prefix index on dense keys; extra ~8n bytes",
        ));
    }
    out
}
