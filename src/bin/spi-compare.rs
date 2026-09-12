//! In-process identical-operation SPI/SQLite benchmark. No SQL or durability
//! equivalence beyond the declared local binary KV contract is implied.
#[cfg(feature = "rusqlite")]
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
#[cfg(feature = "rusqlite")]
use std::path::Path;
use std::{collections::BTreeMap, error::Error, fs, path::PathBuf, time::Instant};
use udb_lab::spi::{Database, Entry, Options};
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
    fn new(path: PathBuf, cache: usize) -> Result<Self> {
        let options = Options {
            cache_bytes: cache,
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
        Ok(serde_json::to_value(self.db().stats()?)?)
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
    let mut report = json!({});
    let mut samples = Vec::new();
    for batch in initial.chunks(256) {
        let t = Instant::now();
        engine.batch(batch)?;
        samples.push(t.elapsed().as_nanos() as u64);
    }
    report["load_batches_256"] = measure(samples, rows);
    let mut state = seed;
    let requests: Vec<_> = (0..rows)
        .map(|_| key(next(&mut state) % rows as u64))
        .collect();
    let mut samples = Vec::new();
    let mut outputs = Vec::new();
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
    report["warm_hits"] = measure(samples, requests.len());
    let mut samples = Vec::new();
    for i in 0..100 {
        let t = Instant::now();
        let found = engine.get(&key(rows as u64 + i))?;
        samples.push(t.elapsed().as_nanos() as u64);
        if found.is_some() {
            return Err("missing key mismatch".into());
        }
    }
    report["misses"] = measure(samples, 100);
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
    report["application_cache_cleared_hits_not_storage_cold"] = measure(samples, 100);
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
    report["ranges_up_to_100_keys"] = measure(samples, 30);
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
    for batch in updates.chunks(64) {
        let t = Instant::now();
        engine.batch(batch)?;
        samples.push(t.elapsed().as_nanos() as u64);
        for r in batch {
            expected.insert(r.key.clone(), r.value.clone());
        }
    }
    report["updates_batches_64"] = measure(samples, updates.len());
    let mut samples = Vec::new();
    for r in updates.iter().take(30) {
        let mutation = forced_mutation(&r.key, &expected);
        let t = Instant::now();
        engine.batch(std::slice::from_ref(&mutation))?;
        samples.push(t.elapsed().as_nanos() as u64);
        expected.insert(mutation.key.clone(), mutation.value.clone());
    }
    report["single_row_commits"] = measure(samples, 30);
    report["single_row_commit_contract"] =
        json!("every transaction flips a bit of the currently stored value");
    report["before_maintenance"] = engine.stats()?;
    let t = Instant::now();
    engine.reopen()?;
    report["reopen_ns"] = json!(t.elapsed().as_nanos() as u64);
    let found = engine.range(&[], None)?;
    let want: Vec<_> = expected
        .into_iter()
        .map(|(key, value)| Entry { key, value })
        .collect();
    if found != want {
        return Err("full reopened state mismatch".into());
    }
    let t = Instant::now();
    engine.maintain()?;
    report["maintenance_with_integrity_check_ns"] = json!(t.elapsed().as_nanos() as u64);
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
    if args.len() < 3 {
        return Err("usage: spi-compare NEW_OUTPUT_DIR spi|sqlite [rows=5000] [seed=1] [value_bytes=64] [cache_bytes=8388608]".into());
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
    if kind != "spi" && (kind != "sqlite" || !cfg!(feature = "rusqlite")) {
        return Err("unknown engine or SQLite feature not enabled".into());
    }
    fs::create_dir(&dir)?;
    let mut engine: Box<dyn Engine> = match kind.as_str() {
        "spi" => Box::new(Spi::new(dir.join("db"), cache)?),
        #[cfg(feature = "rusqlite")]
        "sqlite" => Box::new(Sqlite::new(dir.join("db"), cache)?),
        _ => return Err("unknown engine or SQLite feature not enabled".into()),
    };
    let mut report = run(engine.as_mut(), rows, seed, size)?;
    report["benchmark_schema"] = json!(2);
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
