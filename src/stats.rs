use serde::Serialize;
use std::hint::black_box;
use std::time::Instant;

#[derive(Clone, Debug, Serialize)]
pub struct Record {
    pub experiment: String,
    pub variant: String,
    pub n: u64,
    pub params: serde_json::Value,
    pub median_ns: u128,
    pub p99_ns: u128,
    pub min_ns: u128,
    pub mean_ns: f64,
    pub iters: u32,
    pub ops: u64,
    pub bytes_touched: u64,
    pub extra: serde_json::Value,
    pub notes: String,
}

pub fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

pub fn summarize(times: &mut [u128]) -> (u128, u128, u128, f64) {
    times.sort_unstable();
    let min = times[0];
    let median = percentile(times, 0.50);
    let p99 = percentile(times, 0.99);
    let mean = times.iter().map(|&t| t as f64).sum::<f64>() / times.len() as f64;
    (min, median, p99, mean)
}

/// Warm up, then time `iters` runs. Returns (last_output, times_ns).
pub fn time_ns<R>(warmup: u32, iters: u32, mut f: impl FnMut() -> R) -> (R, Vec<u128>) {
    let mut last = None;
    for _ in 0..warmup {
        last = Some(black_box(f()));
    }
    let mut times = Vec::with_capacity(iters as usize);
    for _ in 0..iters {
        let t0 = Instant::now();
        last = Some(black_box(f()));
        times.push(t0.elapsed().as_nanos());
    }
    (last.expect("warmup+iters > 0"), times)
}

pub fn record(
    experiment: &str,
    variant: &str,
    n: u64,
    params: serde_json::Value,
    mut times: Vec<u128>,
    ops: u64,
    bytes_touched: u64,
    extra: serde_json::Value,
    notes: &str,
) -> Record {
    let (min_ns, median_ns, p99_ns, mean_ns) = summarize(&mut times);
    Record {
        experiment: experiment.to_string(),
        variant: variant.to_string(),
        n,
        params,
        median_ns,
        p99_ns,
        min_ns,
        mean_ns,
        iters: times.len() as u32,
        ops,
        bytes_touched,
        extra,
        notes: notes.to_string(),
    }
}

pub fn ns_per_op(median_ns: u128, ops: u64) -> f64 {
    if ops == 0 {
        return 0.0;
    }
    median_ns as f64 / ops as f64
}

pub fn print_record(r: &Record) {
    let per = ns_per_op(r.median_ns, r.ops);
    println!(
        "{:<22} {:<28} n={:<12} median={:>10} ns  p99={:>10} ns  {:>8.2} ns/op  bytes={:<12}  {}",
        r.experiment,
        r.variant,
        r.n,
        r.median_ns,
        r.p99_ns,
        per,
        r.bytes_touched,
        r.notes
    );
}
