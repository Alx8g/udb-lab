use serde::Serialize;
use std::hint::black_box;
use std::time::Instant;

#[derive(Clone, Debug, Serialize)]
pub struct Record {
    pub experiment: String,
    pub variant: String,
    pub n: u64,
    pub params: serde_json::Value,
    pub median_ns: f64,
    pub p99_ns: f64,
    pub min_ns: f64,
    pub mean_ns: f64,
    pub iters: u32,
    pub inner_reps: u32,
    pub below_timer_resolution: bool,
    pub ops: u64,
    pub bytes_touched: u64,
    pub extra: serde_json::Value,
    pub notes: String,
}

pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

pub fn summarize(times: &mut [f64]) -> (f64, f64, f64, f64) {
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let min = times[0];
    let median = percentile(times, 0.50);
    let p99 = percentile(times, 0.99);
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    (min, median, p99, mean)
}

/// Warm up, then time `iters` runs. Returns (last_output, times_ns, inner_reps).
/// Tiny bodies are repeated until a sample is at least 1 µs. The per-run time
/// is elapsed/reps in floating nanoseconds so sub-nanosecond work is not
/// reported as 0. Ratios from times below 1 µs are not latency claims.
pub fn time_ns<R>(warmup: u32, iters: u32, mut f: impl FnMut() -> R) -> (R, Vec<f64>, u32) {
    let mut last = None;
    for _ in 0..warmup {
        last = Some(black_box(f()));
    }
    let mut reps = 1u32;
    loop {
        let t0 = Instant::now();
        for _ in 0..reps {
            last = Some(black_box(f()));
        }
        let dt = t0.elapsed().as_secs_f64() * 1e9;
        if dt >= 1_000.0 || reps >= 1_000_000 {
            break;
        }
        reps = reps.saturating_mul(4);
    }
    let mut times = Vec::with_capacity(iters as usize);
    for _ in 0..iters {
        let t0 = Instant::now();
        for _ in 0..reps {
            last = Some(black_box(f()));
        }
        times.push(t0.elapsed().as_secs_f64() * 1e9 / f64::from(reps));
    }
    (last.expect("warmup+iters > 0"), times, reps)
}

/// Like `time_ns`, but `setup` is excluded from the sample. Use this when each
/// iteration must start from a fresh mutable snapshot whose copy would dominate.
pub fn time_ns_setup<S, R>(
    warmup: u32,
    iters: u32,
    mut setup: impl FnMut() -> S,
    mut f: impl FnMut(&mut S) -> R,
) -> (R, Vec<f64>, u32) {
    let mut last = None;
    for _ in 0..warmup {
        let mut s = setup();
        last = Some(black_box(f(&mut s)));
    }
    let mut times = Vec::with_capacity(iters as usize);
    for _ in 0..iters {
        let mut s = setup();
        let t0 = Instant::now();
        last = Some(black_box(f(&mut s)));
        times.push(t0.elapsed().as_secs_f64() * 1e9);
    }
    (last.expect("warmup+iters > 0"), times, 1)
}

pub fn record(
    experiment: &str,
    variant: &str,
    n: u64,
    params: serde_json::Value,
    mut times: Vec<f64>,
    inner_reps: u32,
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
        inner_reps,
        below_timer_resolution: median_ns < 1_000.0,
        ops,
        bytes_touched,
        extra,
        notes: notes.to_string(),
    }
}

pub fn ns_per_op(median_ns: f64, ops: u64) -> f64 {
    if ops == 0 {
        return 0.0;
    }
    median_ns / ops as f64
}

pub fn print_record(r: &Record) {
    let per = ns_per_op(r.median_ns, r.ops);
    let flag = if r.below_timer_resolution {
        "  [below 1us; not a latency ratio]"
    } else {
        ""
    };
    eprintln!(
        "{:<22} {:<36} n={:<12} median={:>12.1} ns  p99={:>12.1} ns  {:>8.2} ns/op  bytes={:<12}  {}{}",
        r.experiment,
        r.variant,
        r.n,
        r.median_ns,
        r.p99_ns,
        per,
        r.bytes_touched,
        r.notes,
        flag
    );
}
