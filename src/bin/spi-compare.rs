//! In-process identical-operation SPI/SQLite benchmark. No SQL or durability
//! equivalence beyond the declared local binary KV contract is implied.
#[cfg(feature = "rusqlite")]
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
#[cfg(feature = "rusqlite")]
use std::path::Path;
use std::{collections::BTreeMap, error::Error, fs, path::PathBuf, time::Instant};
use udb_lab::spi::{Database, Entry, Options};

const IDENTITY_SCHEMA: u64 = 1;
const BENCHMARK_SCHEMA: u64 = 2;

fn build_info() -> Value {
    json!({
        "identity_schema": IDENTITY_SCHEMA,
        "benchmark_schema": BENCHMARK_SCHEMA,
        "debug_assertions": cfg!(debug_assertions),
        "profile_enabled": cfg!(feature = "spi-profile"),
        "engines": if cfg!(feature = "rusqlite") {
            vec!["spi", "spi-value-cache", "spi-unbuffered", "spi-grouped", "spi-packed", "sqlite"]
        } else {
            vec!["spi", "spi-value-cache", "spi-unbuffered", "spi-grouped", "spi-packed"]
        },
        "sources": {
            "Cargo.toml": include_str!("../../Cargo.toml"),
            "Cargo.lock": include_str!("../../Cargo.lock"),
            "src/lib.rs": include_str!("../lib.rs"),
            "src/spi/mod.rs": include_str!("../spi/mod.rs"),
            "src/spi/profile.rs": include_str!("../spi/profile.rs"),
            "src/spi/packed.rs": include_str!("../spi/packed.rs"),
            "src/spi/append_buffer.rs": include_str!("../spi/append_buffer.rs"),
            "src/spi/storage.rs": include_str!("../spi/storage.rs"),
            "src/spi/transaction.rs": include_str!("../spi/transaction.rs"),
            "src/bin/spi-compare.rs": include_str!("spi-compare.rs"),
        }
    })
}
type Result<T> = std::result::Result<T, Box<dyn Error>>;
trait Engine {
    fn batch(&mut self, rows: &[Entry]) -> Result<()>;
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn range(&mut self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Entry>>;
    fn clear(&mut self) -> Result<()>;
    fn reopen(&mut self) -> Result<()>;
    fn maintain(&mut self) -> Result<()>;
    fn stats(&self) -> Result<Value>;
}
struct Spi {
    db: Option<Database>,
    path: PathBuf,
    options: Options,
}
impl Spi {
    fn new(
        path: PathBuf,
        cache: usize,
        append_buffer: bool,
        value_cache: bool,
        grouped_updates: bool,
        packed_pages: bool,
    ) -> Result<Self> {
        let options = Options {
            cache_bytes: cache,
            append_buffer,
            value_cache,
            grouped_updates,
            packed_pages,
            ..Options::default()
        };
        Ok(Self {
            db: Some(Database::create(&path, options.clone())?),
            path,
            options,
        })
    }
    fn db(&self) -> &Database {
        self.db.as_ref().unwrap()
    }
}
impl Engine for Spi {
    fn batch(&mut self, rows: &[Entry]) -> Result<()> {
        let mut t = self.db().begin()?;
        for r in rows {
            t.set(r.key.clone(), r.value.clone())?;
        }
        t.commit()?;
        Ok(())
    }
    fn get(&mut self, k: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.db().get(k)?)
    }
    fn range(&mut self, s: &[u8], e: Option<&[u8]>) -> Result<Vec<Entry>> {
        Ok(self.db().snapshot()?.scan(s, e)?)
    }
    fn clear(&mut self) -> Result<()> {
        self.db().clear_cache();
        Ok(())
    }
    fn reopen(&mut self) -> Result<()> {
        self.db.take();
        self.db = Some(Database::open(&self.path, self.options.clone())?);
        Ok(())
    }
    fn maintain(&mut self) -> Result<()> {
        self.db().compact()?;
        self.db().collect()?;
        self.db().verify()?;
        Ok(())
    }
    fn stats(&self) -> Result<Value> {
        let mut stats = serde_json::to_value(self.db().stats()?)?;
        stats["profile"] = udb_lab::spi::profile::snapshot();
        stats["value_cache_enabled"] = json!(self.options.value_cache);
        stats["append_buffer_enabled"] = json!(self.options.append_buffer);
        stats["grouped_updates_enabled"] = json!(self.options.grouped_updates);
        stats["grouped_scratch_limit_bytes"] = json!(if self.options.grouped_updates {
            (self.options.transaction_bytes / 8).min(65536)
        } else {
            0
        });
        stats["append_buffer_capacity_bytes"] =
            json!(if self.options.append_buffer { 65536 } else { 0 });
        Ok(stats)
    }
}
#[cfg(feature = "rusqlite")]
struct Sqlite {
    connection: Option<Connection>,
    path: PathBuf,
    cache: usize,
}
#[cfg(feature = "rusqlite")]
impl Sqlite {
    fn open(path: &Path, cache: usize) -> Result<Connection> {
        let c = Connection::open(path)?;
        c.execute_batch(&format!("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA mmap_size=0; PRAGMA cache_size=-{};",(cache/1024).max(1)))?;
        Ok(c)
    }
    fn new(path: PathBuf, cache: usize) -> Result<Self> {
        fs::create_dir(&path)?;
        let c = Self::open(&path.join("sqlite.db"), cache)?;
        c.execute_batch(
            "CREATE TABLE kv(k BLOB PRIMARY KEY NOT NULL,v BLOB NOT NULL) WITHOUT ROWID;",
        )?;
        Ok(Self {
            connection: Some(c),
            path,
            cache,
        })
    }
    fn c(&self) -> &Connection {
        self.connection.as_ref().unwrap()
    }
}
#[cfg(feature = "rusqlite")]
impl Engine for Sqlite {
    fn batch(&mut self, rows: &[Entry]) -> Result<()> {
        let t = self.connection.as_mut().unwrap().transaction()?;
        {
            let mut s = t.prepare_cached(
                "INSERT INTO kv(k,v) VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            )?;
            for r in rows {
                s.execute(params![&r.key, &r.value])?;
            }
        }
        t.commit()?;
        Ok(())
    }
    fn get(&mut self, k: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .c()
            .prepare_cached("SELECT v FROM kv WHERE k=?1")?
            .query_row([k], |r| r.get(0))
            .optional()?)
    }
    fn range(&mut self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Entry>> {
        let sql = if end.is_some() {
            "SELECT k,v FROM kv WHERE k>=?1 AND k<?2 ORDER BY k"
        } else {
            "SELECT k,v FROM kv WHERE k>=?1 ORDER BY k"
        };
        let mut s = self.c().prepare_cached(sql)?;
        let mut rows = if let Some(end) = end {
            s.query(params![start, end])?
        } else {
            s.query([start])?
        };
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(Entry {
                key: r.get(0)?,
                value: r.get(1)?,
            });
        }
        Ok(out)
    }
    fn clear(&mut self) -> Result<()> {
        self.c().execute_batch("PRAGMA shrink_memory;")?;
        Ok(())
    }
    fn reopen(&mut self) -> Result<()> {
        self.connection.take();
        self.connection = Some(Self::open(&self.path.join("sqlite.db"), self.cache)?);
        Ok(())
    }
    fn maintain(&mut self) -> Result<()> {
        self.c().execute_batch(
            "PRAGMA wal_checkpoint(TRUNCATE); VACUUM; PRAGMA wal_checkpoint(TRUNCATE);",
        )?;
        let integrity: String = self
            .c()
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        if integrity != "ok" {
            return Err(integrity.into());
        }
        Ok(())
    }
    fn stats(&self) -> Result<Value> {
        let bytes: u64 = fs::read_dir(&self.path)?
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum();
        Ok(
            json!({"physical_bytes":bytes,"sqlite_version":rusqlite::version(),"journal_mode":"WAL","synchronous":"FULL","configured_cache_bytes":self.cache,"mmap_size":0}),
        )
    }
}
fn next(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state
}
fn key(i: u64) -> Vec<u8> {
    i.to_be_bytes().to_vec()
}
fn value(i: u64, seed: u64, size: usize) -> Vec<u8> {
    let mut s = i ^ seed;
    let mut v = vec![0; size];
    for b in &mut v {
        *b = (next(&mut s) >> 32) as u8;
    }
    v
}
fn measure(samples: Vec<u64>, ops: usize) -> Value {
    let sample_count = samples.len();
    let mut sorted = samples.clone();
    sorted.sort_unstable();
    let q = |p: f64| sorted[((sorted.len() - 1) as f64 * p).round() as usize];
    let total: u64 = samples.iter().sum();
    json!({"samples_ns":samples,"operations":ops,"sample_count":sample_count,"percentile_unit":"one timed API call or transaction batch, not per row","total_ns":total,"p50_ns":q(0.5),"p95_ns":q(0.95),"p99_ns":q(0.99)})
}
// Candidate for the spi-compare executable only. Library callers keep their allocator.
#[cfg(feature = "spi-profile")]
mod diagnostic_metrics {
    use serde_json::{json, Value};
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    pub struct TrackedAllocator;
    static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
    static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
    static LIVE_BYTES: AtomicU64 = AtomicU64::new(0);
    static PEAK_BYTES: AtomicU64 = AtomicU64::new(0);
    fn admitted(n: usize) {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(n as u64, Ordering::Relaxed);
        let live = LIVE_BYTES.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
        PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
    }
    unsafe impl GlobalAlloc for TrackedAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc(layout) };
            if !ptr.is_null() {
                admitted(layout.size());
            }
            ptr
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc_zeroed(layout) };
            if !ptr.is_null() {
                admitted(layout.size());
            }
            ptr
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) };
            LIVE_BYTES.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            let next = unsafe { System.realloc(ptr, layout, size) };
            if !next.is_null() {
                LIVE_BYTES.fetch_sub(layout.size() as u64, Ordering::Relaxed);
                admitted(size);
            }
            next
        }
    }

    #[cfg(windows)]
    fn cpu_ns() -> Option<u64> {
        #[repr(C)]
        #[derive(Default)]
        struct FileTime {
            low: u32,
            high: u32,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> *mut std::ffi::c_void;
            fn GetProcessTimes(
                p: *mut std::ffi::c_void,
                c: *mut FileTime,
                e: *mut FileTime,
                k: *mut FileTime,
                u: *mut FileTime,
            ) -> i32;
        }
        let (mut c, mut e, mut k, mut u) = (
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
        );
        let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
        (ok != 0).then(|| {
            (((k.high as u64) << 32 | k.low as u64) + ((u.high as u64) << 32 | u.low as u64)) * 100
        })
    }
    #[cfg(target_os = "linux")]
    fn cpu_ns() -> Option<u64> {
        #[repr(C)]
        struct Timespec {
            sec: std::ffi::c_long,
            nsec: std::ffi::c_long,
        }
        unsafe extern "C" {
            fn clock_gettime(id: i32, t: *mut Timespec) -> i32;
        }
        let mut t = Timespec { sec: 0, nsec: 0 };
        let ok = unsafe { clock_gettime(2, &mut t) }; // CLOCK_PROCESS_CPUTIME_ID on Linux.
        (ok == 0).then(|| t.sec as u64 * 1_000_000_000 + t.nsec as u64)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    fn cpu_ns() -> Option<u64> {
        None
    }

    #[cfg(windows)]
    fn memory() -> Value {
        #[repr(C)]
        #[derive(Default)]
        struct Counters {
            cb: u32,
            faults: u32,
            peak_working: usize,
            working: usize,
            peak_paged: usize,
            paged: usize,
            peak_nonpaged: usize,
            nonpaged: usize,
            pagefile: usize,
            peak_pagefile: usize,
            private: usize,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> *mut std::ffi::c_void;
        }
        #[link(name = "psapi")]
        unsafe extern "system" {
            fn GetProcessMemoryInfo(p: *mut std::ffi::c_void, c: *mut Counters, size: u32) -> i32;
        }
        let size = std::mem::size_of::<Counters>() as u32;
        let mut c = Counters {
            cb: size,
            ..Counters::default()
        };
        if unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c, size) } == 0 {
            return Value::Null;
        }
        json!({"working_set_bytes":c.working,"process_peak_working_set_bytes":c.peak_working,"private_commit_bytes":c.private})
    }
    #[cfg(target_os = "linux")]
    fn memory() -> Value {
        let Ok(text) = std::fs::read_to_string("/proc/self/status") else {
            return Value::Null;
        };
        let get = |key: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(key))
                .and_then(|v| v.split_whitespace().next())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|n| n * 1024)
        };
        json!({"working_set_bytes":get("VmRSS:"),"process_peak_working_set_bytes":get("VmHWM:")})
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    fn memory() -> Value {
        Value::Null
    }

    pub struct Phase {
        cpu: Option<u64>,
        calls: u64,
        bytes: u64,
        counters: Value,
        start: Instant,
    }
    impl Phase {
        pub fn start() -> Self {
            let counters = udb_lab::spi::profile::snapshot();
            Self {
                counters,
                calls: ALLOC_CALLS.load(Ordering::Relaxed),
                bytes: ALLOC_BYTES.load(Ordering::Relaxed),
                cpu: cpu_ns(),
                start: Instant::now(),
            }
        }
        pub fn finish(self, report: &mut Value, name: &str) {
            let elapsed = self.start.elapsed().as_nanos() as u64;
            let cpu = cpu_ns()
                .zip(self.cpu)
                .and_then(|(end, start)| end.checked_sub(start));
            let calls = ALLOC_CALLS.load(Ordering::Relaxed) - self.calls;
            let bytes = ALLOC_BYTES.load(Ordering::Relaxed) - self.bytes;
            let live = LIVE_BYTES.load(Ordering::Relaxed);
            let peak = PEAK_BYTES.load(Ordering::Relaxed);
            let end = udb_lab::spi::profile::snapshot();
            let mut delta = serde_json::Map::new();
            for (k, v) in end.as_object().unwrap() {
                delta.insert(
                    k.clone(),
                    json!(v.as_u64().unwrap() - self.counters[k].as_u64().unwrap()),
                );
            }
            if report.get("diagnostic_phases").is_none() {
                report["diagnostic_phases"] = json!({});
            }
            report["diagnostic_phases"][name] = json!({"phase_wall_ns_including_harness":elapsed,
                "process_cpu_ns_including_harness":cpu, "rust_allocation_calls":calls,
                "rust_requested_allocation_bytes":bytes,"rust_live_requested_bytes":live,
                "process_peak_rust_requested_bytes":peak,"os_memory_after_phase":memory(),"storage":delta});
        }
    }
}
#[cfg(feature = "spi-profile")]
#[global_allocator]
static DIAGNOSTIC_ALLOCATOR: diagnostic_metrics::TrackedAllocator =
    diagnostic_metrics::TrackedAllocator;
#[cfg(feature = "spi-profile")]
use diagnostic_metrics::Phase;
#[cfg(not(feature = "spi-profile"))]
struct Phase;
#[cfg(not(feature = "spi-profile"))]
impl Phase {
    #[inline]
    fn start() -> Self {
        Self
    }
    #[inline]
    fn finish(self, _report: &mut Value, _name: &str) {}
}

fn run(engine: &mut dyn Engine, rows: usize, seed: u64, size: usize) -> Result<Value> {
    let initial: Vec<_> = (0..rows as u64)
        .map(|i| Entry {
            key: key(i),
            value: value(i, seed, size),
        })
        .collect();
    let mut expected: BTreeMap<_, _> = initial
        .iter()
        .map(|r| (r.key.clone(), r.value.clone()))
        .collect();
    let mut report = json!({"diagnostic_only": cfg!(feature = "spi-profile")});
    let phase = Phase::start();
    let mut samples = Vec::new();
    for batch in initial.chunks(256) {
        let t = Instant::now();
        engine.batch(batch)?;
        samples.push(t.elapsed().as_nanos() as u64);
    }
    phase.finish(&mut report, "load");
    report["load_batches_256"] = measure(samples, rows);
    report["after_load"] = engine.stats()?;
    report["grouped_write_workload_extension"] = json!(1);
    let mut state = seed;
    let requests: Vec<_> = (0..rows)
        .map(|_| key(next(&mut state) % rows as u64))
        .collect();
    let mut samples = Vec::new();
    let mut outputs = Vec::new();
    let phase = Phase::start();
    for k in &requests {
        let t = Instant::now();
        outputs.push(engine.get(k)?);
        samples.push(t.elapsed().as_nanos() as u64);
    }
    for (k, v) in requests.iter().zip(outputs) {
        if v.as_ref() != expected.get(k) {
            return Err("point output mismatch".into());
        }
    }
    phase.finish(&mut report, "warm_hits");
    report["warm_hits"] = measure(samples, requests.len());
    let phase = Phase::start();
    let mut samples = Vec::new();
    for i in 0..100 {
        let t = Instant::now();
        let found = engine.get(&key(rows as u64 + i))?;
        samples.push(t.elapsed().as_nanos() as u64);
        if found.is_some() {
            return Err("missing key mismatch".into());
        }
    }
    phase.finish(&mut report, "misses");
    report["misses"] = measure(samples, 100);
    let phase = Phase::start();
    let mut samples = Vec::new();
    for k in requests.iter().take(100) {
        engine.clear()?;
        let t = Instant::now();
        let found = engine.get(k)?;
        samples.push(t.elapsed().as_nanos() as u64);
        if found.as_ref() != expected.get(k) {
            return Err("cache-cleared mismatch".into());
        }
    }
    phase.finish(&mut report, "cleared_hits");
    report["application_cache_cleared_hits_not_storage_cold"] = measure(samples, 100);
    let phase = Phase::start();
    let mut samples = Vec::new();
    for _ in 0..30 {
        let i = next(&mut state) % rows as u64;
        let start = key(i);
        let end = key((i + 100).min(rows as u64));
        let t = Instant::now();
        let found = engine.range(&start, Some(&end))?;
        samples.push(t.elapsed().as_nanos() as u64);
        let want: Vec<_> = expected
            .range(start..end)
            .map(|(k, v)| Entry {
                key: k.clone(),
                value: v.clone(),
            })
            .collect();
        if found != want {
            return Err("range mismatch".into());
        }
    }
    phase.finish(&mut report, "ranges");
    report["ranges_up_to_100_keys"] = measure(samples, 30);
    let phase = Phase::start();
    // Measure deliberate reuse independently from first-touch/streaming traffic.
    engine.clear()?;
    let reused_keys: Vec<_> = (0..16u64).map(key).collect();
    for k in &reused_keys {
        if engine.get(k)?.as_ref() != expected.get(k) {
            return Err("reuse warmup mismatch".into());
        }
    }
    let mut samples = Vec::new();
    let mut outputs = Vec::with_capacity(1000);
    for i in 0..1000 {
        let k = &reused_keys[i % reused_keys.len()];
        let t = Instant::now();
        let value = engine.get(k)?;
        let elapsed = t.elapsed().as_nanos() as u64;
        samples.push(elapsed);
        outputs.push(value);
    }
    for (i, v) in outputs.iter().enumerate() {
        if v.as_ref() != expected.get(&reused_keys[i % reused_keys.len()]) {
            return Err("reused value mismatch".into());
        }
    }
    phase.finish(&mut report, "reuse");
    report["reused_16_key_hits"] = measure(samples, 1000);
    report["cache_after_reused_hits"] = engine.stats()?;

    // Stream unique values through point reads. A fixed-stride permutation is
    // not needed: every key appears exactly once, guaranteeing no value reuse.
    engine.clear()?;
    let mut samples = Vec::new();
    let phase = Phase::start();
    for r in initial.iter().rev() {
        let t = Instant::now();
        let found = engine.get(&r.key)?;
        samples.push(t.elapsed().as_nanos() as u64);
        if found.as_ref() != expected.get(&r.key) {
            return Err("one-touch value mismatch".into());
        }
    }
    phase.finish(&mut report, "unique");
    report["unique_value_reads"] = measure(samples, rows);
    report["cache_after_unique_reads"] = engine.stats()?;

    engine.clear()?;
    let t = Instant::now();
    let phase = Phase::start();
    let scanned = engine.range(&[], None)?;
    let scan_elapsed = t.elapsed().as_nanos() as u64;
    phase.finish(&mut report, "scan");
    let want: Vec<_> = expected
        .iter()
        .map(|(key, value)| Entry {
            key: key.clone(),
            value: value.clone(),
        })
        .collect();
    if scanned != want {
        return Err("one-off scan mismatch".into());
    }
    report["one_off_scan"] = measure(vec![scan_elapsed], rows);
    report["cache_after_one_off_scan"] = engine.stats()?;
    report["value_cache_workload_extension"] = json!(1);
    let updates: Vec<_> = (0..512)
        .map(|_| {
            let i = next(&mut state) % rows as u64;
            let key = key(i);
            let mut value = expected[&key].clone();
            value[0] ^= 1;
            expected.insert(key.clone(), value.clone());
            Entry { key, value }
        })
        .collect();
    let mut samples = Vec::new();
    let phase = Phase::start();
    for batch in updates.chunks(64) {
        let t = Instant::now();
        engine.batch(batch)?;
        samples.push(t.elapsed().as_nanos() as u64);
        for r in batch {
            expected.insert(r.key.clone(), r.value.clone());
        }
    }
    phase.finish(&mut report, "updates");
    report["updates_batches_64"] = measure(samples, updates.len());
    report["after_updates"] = engine.stats()?;
    let mut samples = Vec::new();
    let phase = Phase::start();
    for r in updates.iter().take(30) {
        let mutation = forced_mutation(&r.key, &expected);
        let t = Instant::now();
        engine.batch(std::slice::from_ref(&mutation))?;
        samples.push(t.elapsed().as_nanos() as u64);
        expected.insert(mutation.key.clone(), mutation.value.clone());
    }
    phase.finish(&mut report, "single_commits");
    report["single_row_commits"] = measure(samples, 30);
    report["single_row_commit_contract"] =
        json!("every transaction flips a bit of the currently stored value");
    report["before_maintenance"] = engine.stats()?;
    let phase = Phase::start();
    let t = Instant::now();
    engine.reopen()?;
    report["reopen_ns"] = json!(t.elapsed().as_nanos() as u64);
    phase.finish(&mut report, "reopen");
    let found = engine.range(&[], None)?;
    let want: Vec<_> = expected
        .into_iter()
        .map(|(key, value)| Entry { key, value })
        .collect();
    if found != want {
        return Err("full reopened state mismatch".into());
    }
    let phase = Phase::start();
    let t = Instant::now();
    engine.maintain()?;
    report["maintenance_with_integrity_check_ns"] = json!(t.elapsed().as_nanos() as u64);
    phase.finish(&mut report, "maintenance");
    report["after_maintenance"] = engine.stats()?;
    engine.reopen()?;
    if engine.range(&[], None)? != want {
        return Err("compacted/reopened state mismatch".into());
    }
    report["full_output_validation"] = json!("PASS");
    Ok(report)
}
fn forced_mutation(key: &[u8], expected: &BTreeMap<Vec<u8>, Vec<u8>>) -> Entry {
    let mut value = expected[key].clone();
    value[0] ^= 1;
    Entry {
        key: key.to_vec(),
        value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn build_info_matches_compiled_engines_and_complete_storage_inventory() {
        let info = build_info();
        assert_eq!(
            info["engines"]
                .as_array()
                .unwrap()
                .contains(&json!("sqlite")),
            cfg!(feature = "rusqlite")
        );
        assert!(info["engines"]
            .as_array()
            .unwrap()
            .contains(&json!("spi-unbuffered")));
        assert!(info["engines"]
            .as_array()
            .unwrap()
            .contains(&json!("spi-grouped")));
        let modules: Vec<_> = fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src/spi"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.ends_with(".rs"))
            .collect();
        for name in modules {
            assert!(info["sources"].get(format!("src/spi/{name}")).is_some());
        }
        assert_eq!(
            info["sources"]["src/spi/append_buffer.rs"],
            include_str!("../spi/append_buffer.rs")
        );
        assert_eq!(
            info["sources"]["src/spi/profile.rs"],
            include_str!("../spi/profile.rs")
        );
    }
    #[test]
    fn single_commit_stream_changes_even_repeated_keys() {
        let key = vec![0, 255];
        let mut expected = BTreeMap::from([(key.clone(), vec![37])]);
        for _ in 0..100 {
            let next = forced_mutation(&key, &expected);
            assert_ne!(next.value, expected[&key]);
            expected.insert(next.key, next.value);
        }
    }
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).is_some_and(|arg| arg == "--build-info") {
        println!("{}", build_info());
        return Ok(());
    }
    if args.len() < 3 {
        return Err("usage: spi-compare NEW_OUTPUT_DIR spi|spi-value-cache|spi-unbuffered|spi-grouped|spi-packed|sqlite [rows=5000] [seed=1] [value_bytes=64] [cache_bytes=8388608]".into());
    }
    let dir = PathBuf::from(&args[1]);
    let kind = &args[2];
    let rows = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(5000);
    let seed = args.get(4).map(|s| s.parse()).transpose()?.unwrap_or(1);
    let size = args.get(5).map(|s| s.parse()).transpose()?.unwrap_or(64);
    let cache = args
        .get(6)
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(8 * 1024 * 1024);
    if rows < 100 || size == 0 || size > 4096 {
        return Err("rows>=100 and value_bytes in 1..4096 required".into());
    }
    if kind != "spi"
        && kind != "spi-value-cache"
        && kind != "spi-unbuffered"
        && kind != "spi-grouped"
        && kind != "spi-packed"
        && (kind != "sqlite" || !cfg!(feature = "rusqlite"))
    {
        return Err("unknown engine or SQLite feature not enabled".into());
    }
    fs::create_dir(&dir)?;
    let mut engine: Box<dyn Engine> = match kind.as_str() {
        "spi" => Box::new(Spi::new(dir.join("db"), cache, true, false, false, false)?),
        "spi-value-cache" => Box::new(Spi::new(dir.join("db"), cache, true, true, false, false)?),
        "spi-unbuffered" => Box::new(Spi::new(dir.join("db"), cache, false, false, false, false)?),
        "spi-grouped" => Box::new(Spi::new(dir.join("db"), cache, true, false, true, false)?),
        "spi-packed" => Box::new(Spi::new(dir.join("db"), cache, true, false, false, true)?),
        #[cfg(feature = "rusqlite")]
        "sqlite" => Box::new(Sqlite::new(dir.join("db"), cache)?),
        _ => return Err("unknown engine or SQLite feature not enabled".into()),
    };
    let mut report = run(engine.as_mut(), rows, seed, size)?;
    report["benchmark_schema"] = json!(BENCHMARK_SCHEMA);
    report["engine"] = json!(kind);
    report["rows"] = json!(rows);
    report["seed"] = json!(seed);
    report["value_bytes"] = json!(size);
    report["cache_bytes"] = json!(cache);
    report["os"] = json!(std::env::consts::OS);
    report["arch"] = json!(std::env::consts::ARCH);
    report["limits"] = json!([
        "No SQL feature-equivalence claim",
        "OS cache remains uncontrolled",
        "App cache clearing is not storage cold",
        "Latency samples are closed-loop isolated operations, not service p99 under load"
    ]);
    let bytes = serde_json::to_vec_pretty(&report)?;
    fs::write(dir.join("result.json"), bytes)?;
    println!(
        "{}",
        json!({"engine":kind,"rows":rows,"validation":"PASS","load_ms":report["load_batches_256"]["total_ns"].as_u64().unwrap()as f64/1e6,"warm_read_ms":report["warm_hits"]["total_ns"].as_u64().unwrap()as f64/1e6,"single_commit_p50_us":report["single_row_commits"]["p50_ns"].as_u64().unwrap()as f64/1e3,"result":dir.join("result.json")})
    );
    Ok(())
}
