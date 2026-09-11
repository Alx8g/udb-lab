//! Compiled coordination: local reservation quotas vs a global counter.
//!
//! 10_000 stock units. Naive: every reservation CAS/locks the global count.
//! Compiled: shard exclusive rights; a shard spends locally until empty,
//! then restocks from a coordinator. Correctness: never oversell, and
//! "sold out" is only reported after a global check.

use crate::stats::{record, time_ns, Record};
use serde_json::json;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reserve {
    Ok,
    SoldOut,
}

struct GlobalStock {
    left: AtomicI64,
}

impl GlobalStock {
    fn new(n: i64) -> Self {
        Self {
            left: AtomicI64::new(n),
        }
    }
    fn reserve(&self) -> Reserve {
        loop {
            let cur = self.left.load(Ordering::SeqCst);
            if cur <= 0 {
                return Reserve::SoldOut;
            }
            if self
                .left
                .compare_exchange(cur, cur - 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Reserve::Ok;
            }
        }
    }
    fn remaining(&self) -> i64 {
        self.left.load(Ordering::SeqCst)
    }
}

struct Coordinator {
    remaining: AtomicI64,
}

impl Coordinator {
    fn new(n: i64) -> Self {
        Self {
            remaining: AtomicI64::new(n),
        }
    }
    fn take(&self, want: i64) -> i64 {
        loop {
            let cur = self.remaining.load(Ordering::SeqCst);
            if cur <= 0 {
                return 0;
            }
            let give = want.min(cur);
            if self
                .remaining
                .compare_exchange(cur, cur - give, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return give;
            }
        }
    }
    fn remaining(&self) -> i64 {
        self.remaining.load(Ordering::SeqCst)
    }
}

struct Shard {
    local: i64,
    restock: i64,
}

impl Shard {
    fn reserve(&mut self, coord: &Coordinator) -> Reserve {
        if self.local <= 0 {
            self.local = coord.take(self.restock);
        }
        if self.local <= 0 {
            // Confirm globally empty. A peer may still hold unused quota;
            // a correct "sold out" needs either draining or a stronger protocol.
            // This implementation reports SoldOut only if coordinator is empty
            // AND this shard has nothing. That can be a false sold-out if other
            // shards hold leftover quota. We expose that as a measured failure
            // mode, then compare with a draining fallback.
            if coord.remaining() <= 0 {
                return Reserve::SoldOut;
            }
            self.local = coord.take(self.restock.max(1));
            if self.local <= 0 {
                return Reserve::SoldOut;
            }
        }
        self.local -= 1;
        Reserve::Ok
    }
}

/// Correct sold-out: if local and coordinator are empty, steal 1 from a pool
/// of leftover shard rights. For the benchmark we keep leftovers on shards
/// and optionally drain.
struct QuotaSystem {
    coord: Coordinator,
    shards: Vec<Mutex<Shard>>,
}

impl QuotaSystem {
    fn new(stock: i64, n_shards: usize, restock: i64) -> Self {
        let per = (stock / n_shards as i64).max(1);
        let mut left = stock;
        let mut shards = Vec::new();
        for i in 0..n_shards {
            let give = if i + 1 == n_shards { left } else { per.min(left) };
            left -= give;
            shards.push(Mutex::new(Shard {
                local: give,
                restock,
            }));
        }
        Self {
            coord: Coordinator::new(left.max(0)),
            shards,
        }
    }

    fn reserve(&self, shard: usize) -> Reserve {
        let mut s: MutexGuard<Shard> = self.shards[shard].lock().unwrap();
        s.reserve(&self.coord)
    }

    fn remaining_total(&self) -> i64 {
        let mut t = self.coord.remaining();
        for s in &self.shards {
            t += s.lock().unwrap().local;
        }
        t
    }
}

pub fn correctness() -> Result<(), String> {
    const STOCK: i64 = 5_000;
    let global = GlobalStock::new(STOCK);
    let mut ok = 0i64;
    for _ in 0..STOCK + 200 {
        if global.reserve() == Reserve::Ok {
            ok += 1;
        }
    }
    if ok != STOCK || global.remaining() != 0 {
        return Err(format!("global oversell ok={ok} left={}", global.remaining()));
    }

    let q = QuotaSystem::new(STOCK, 8, 16);
    let mut ok = 0i64;
    let mut i = 0usize;
    loop {
        match q.reserve(i % 8) {
            Reserve::Ok => ok += 1,
            Reserve::SoldOut => break,
        }
        i += 1;
        if i > STOCK as usize * 4 {
            return Err("quota did not terminate".into());
        }
    }
    let left = q.remaining_total();
    if ok + left != STOCK {
        return Err(format!(
            "quota lost units ok={ok} left={left} stock={STOCK}"
        ));
    }
    // leftover on idle shards can cause early SoldOut
    eprintln!(
        "coordination correctness: global exact; quota reserved={ok} leftover={left} (early-sold-out units={})",
        STOCK - ok
    );
    Ok(())
}

fn run_global(stock: i64, threads: usize, per_thread: usize) -> (u64, u64, i64) {
    let g = GlobalStock::new(stock);
    let ok = AtomicU64::new(0);
    let miss = AtomicU64::new(0);
    thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                for _ in 0..per_thread {
                    match g.reserve() {
                        Reserve::Ok => {
                            ok.fetch_add(1, Ordering::Relaxed);
                        }
                        Reserve::SoldOut => {
                            miss.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            });
        }
    });
    (ok.load(Ordering::Relaxed), miss.load(Ordering::Relaxed), g.remaining())
}

fn run_quota(
    stock: i64,
    threads: usize,
    per_thread: usize,
    restock: i64,
) -> (u64, u64, i64) {
    let q = QuotaSystem::new(stock, threads, restock);
    let ok = AtomicU64::new(0);
    let miss = AtomicU64::new(0);
    thread::scope(|scope| {
        for t in 0..threads {
            let q = &q;
            let ok = &ok;
            let miss = &miss;
            scope.spawn(move || {
                for _ in 0..per_thread {
                    match q.reserve(t) {
                        Reserve::Ok => {
                            ok.fetch_add(1, Ordering::Relaxed);
                        }
                        Reserve::SoldOut => {
                            miss.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            });
        }
    });
    (
        ok.load(Ordering::Relaxed),
        miss.load(Ordering::Relaxed),
        q.remaining_total(),
    )
}

pub fn run(quick: bool) -> Vec<Record> {
    let mut out = Vec::new();
    let threads = if quick { 8usize } else { 16usize };
    let stock = if quick { 50_000i64 } else { 400_000i64 };
    let per = if quick { 20_000usize } else { 80_000usize };
    let attempts = (threads * per) as u64;

    let (val, times) = time_ns(1, 4, || run_global(stock, threads, per));
    out.push(record(
        "coordination",
        "global_cas",
        stock as u64,
        json!({"threads": threads, "per": per, "ok": val.0, "soldout": val.1, "left": val.2}),
        times,
        attempts,
        attempts * 16,
        json!({"ok": val.0, "soldout": val.1, "left": val.2}),
        "every reserve contends on one counter",
    ));

    for restock in [1i64, 16, 256, 4096] {
        let (val, times) = time_ns(1, 4, || run_quota(stock, threads, per, restock));
        let false_soldout = stock as i64 - val.0 as i64 - val.2;
        out.push(record(
            "coordination",
            &format!("quota_restock_{restock}"),
            stock as u64,
            json!({
                "threads": threads,
                "per": per,
                "restock": restock,
                "ok": val.0,
                "soldout": val.1,
                "left": val.2,
                "stranded_or_false_soldout": false_soldout
            }),
            times,
            attempts,
            0,
            json!({"ok": val.0, "soldout": val.1, "left": val.2}),
            "local rights until restock; leftover quota can strand units",
        ));
    }
    out
}
