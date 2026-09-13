//! Additional native adapters for the shared binary-KV operation contract.
//! Every adapter copies complete outputs. Durability and cache differences are explicit.
use super::*;
#[cfg(feature = "redb")]
use std::ops::Bound::{Excluded, Included, Unbounded};

#[cfg(any(feature = "redb", feature = "lmdb", feature = "rocksdb"))]
fn owned_bytes(path: &PathBuf) -> Result<u64> {
    let mut total = 0;
    for item in fs::read_dir(path)? {
        let item = item?;
        if item.file_type()?.is_dir() {
            total += owned_bytes(&item.path())?;
        } else {
            total += item.metadata()?.len();
        }
    }
    Ok(total)
}

#[cfg(feature = "redb")]
pub struct Redb {
    db: Option<redb::Database>,
    path: PathBuf,
    cache: usize,
}
#[cfg(feature = "redb")]
const TABLE: redb::TableDefinition<&[u8], &[u8]> = redb::TableDefinition::new("kv");
#[cfg(feature = "redb")]
impl Redb {
    pub fn new(path: PathBuf, cache: usize) -> Result<Self> {
        fs::create_dir(&path)?;
        let db = redb::Database::builder()
            .set_cache_size(cache)
            .create(path.join("data.redb"))?;
        let mut tx = db.begin_write()?;
        tx.set_durability(redb::Durability::Immediate)?;
        {
            tx.open_table(TABLE)?;
        }
        tx.commit()?;
        Ok(Self {
            db: Some(db),
            path,
            cache,
        })
    }
    fn db(&self) -> &redb::Database {
        self.db.as_ref().unwrap()
    }
}
#[cfg(feature = "redb")]
impl Engine for Redb {
    fn batch(&mut self, rows: &[Entry]) -> Result<()> {
        let mut tx = self.db().begin_write()?;
        tx.set_durability(redb::Durability::Immediate)?;
        {
            let mut table = tx.open_table(TABLE)?;
            for r in rows {
                table.insert(r.key.as_slice(), r.value.as_slice())?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        use redb::ReadableDatabase;
        let tx = self.db().begin_read()?;
        let table = tx.open_table(TABLE)?;
        Ok(table.get(key)?.map(|v| v.value().to_vec()))
    }
    fn range(&mut self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Entry>> {
        use redb::ReadableDatabase;
        let tx = self.db().begin_read()?;
        let table = tx.open_table(TABLE)?;
        let mut out = Vec::new();
        for r in table.range::<&[u8]>((Included(start), end.map_or(Unbounded, Excluded)))? {
            let (k, v) = r?;
            out.push(Entry {
                key: k.value().to_vec(),
                value: v.value().to_vec(),
            });
        }
        Ok(out)
    }
    fn clear(&mut self) -> Result<()> {
        self.reopen()
    }
    fn reopen(&mut self) -> Result<()> {
        self.db.take();
        self.db = Some(
            redb::Database::builder()
                .set_cache_size(self.cache)
                .open(self.path.join("data.redb"))?,
        );
        Ok(())
    }
    fn maintain(&mut self) -> Result<()> {
        self.db.as_mut().unwrap().compact()?;
        Ok(())
    }
    fn stats(&self) -> Result<Value> {
        Ok(
            json!({"physical_bytes":owned_bytes(&self.path)?,"engine_version":"redb 4.1.0",
            "durability":"Immediate", "configured_cache_bytes":self.cache,
            "cache_clear":"close/reopen, not OS purge", "maintenance":"native compact; full reopened output checked by harness",
            "contract":"single-process serial operation stream; serializability under concurrent clients not compared"}),
        )
    }
}

#[cfg(feature = "lmdb")]
pub struct Lmdb {
    env: Option<lmdb::Environment>,
    db: lmdb::Database,
    path: PathBuf,
}
#[cfg(feature = "lmdb")]
impl Lmdb {
    pub fn new(path: PathBuf) -> Result<Self> {
        fs::create_dir(&path)?;
        let env = Self::open(&path)?;
        let db = env.open_db(None)?;
        Ok(Self {
            env: Some(env),
            db,
            path,
        })
    }
    fn open(path: &PathBuf) -> Result<lmdb::Environment> {
        // Virtual address reservation is NOT a RAM/cache budget. No NOSYNC,
        // NOMETASYNC, MAPASYNC or WRITEMAP flags are permitted.
        Ok(lmdb::Environment::new()
            .set_map_size(1024 * 1024 * 1024)
            .open(path)?)
    }
    fn env(&self) -> &lmdb::Environment {
        self.env.as_ref().unwrap()
    }
}
#[cfg(feature = "lmdb")]
impl Engine for Lmdb {
    fn batch(&mut self, rows: &[Entry]) -> Result<()> {
        use lmdb::Transaction;
        let mut tx = self.env().begin_rw_txn()?;
        for r in rows {
            tx.put(self.db, &r.key, &r.value, lmdb::WriteFlags::empty())?;
        }
        tx.commit()?;
        Ok(())
    }
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        use lmdb::Transaction;
        let tx = self.env().begin_ro_txn()?;
        match tx.get(self.db, &key) {
            Ok(v) => Ok(Some(v.to_vec())),
            Err(lmdb::Error::NotFound) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn range(&mut self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Entry>> {
        use lmdb::{Cursor, Transaction};
        let tx = self.env().begin_ro_txn()?;
        let mut cursor = tx.open_ro_cursor(self.db)?;
        let mut out = Vec::new();
        let iter = if start.is_empty() {
            cursor.iter_start()
        } else {
            cursor.iter_from(start)
        };
        for r in iter {
            let (k, v) = r?;
            if end.is_some_and(|e| k >= e) {
                break;
            }
            out.push(Entry {
                key: k.to_vec(),
                value: v.to_vec(),
            });
        }
        Ok(out)
    }
    fn clear(&mut self) -> Result<()> {
        Ok(())
    } // OS-managed mmap cache is deliberately NOT misrepresented as cleared.
    fn reopen(&mut self) -> Result<()> {
        self.env.take();
        let env = Self::open(&self.path)?;
        self.db = env.open_db(None)?;
        self.env = Some(env);
        Ok(())
    }
    fn maintain(&mut self) -> Result<()> {
        self.env().sync(true)?;
        Ok(())
    }
    fn stats(&self) -> Result<Value> {
        Ok(
            json!({"physical_bytes":owned_bytes(&self.path)?,"engine_version":"LMDB via lmdb-rkv 0.14.0",
            "durability":"default synchronous commit, no relaxed flags", "map_size_bytes":1073741824u64,
            "configured_cache_bytes":null,"cache_clear":"not supported; OS mmap cache stays warm",
            "maintenance":"forced environment sync, NOT compaction; full reopened output checked",
            "contract":"keys nonempty, <=511 bytes in this comparison; single writer, snapshot reads"}),
        )
    }
}

#[cfg(feature = "rocksdb")]
pub struct Rocks {
    db: Option<rocksdb::DB>,
    path: PathBuf,
    cache: usize,
}
#[cfg(feature = "rocksdb")]
impl Rocks {
    pub fn new(path: PathBuf, cache: usize) -> Result<Self> {
        fs::create_dir(&path)?;
        let db = Self::open(&path, cache)?;
        Ok(Self {
            db: Some(db),
            path,
            cache,
        })
    }
    fn open(path: &PathBuf, cache: usize) -> Result<rocksdb::DB> {
        let mut o = rocksdb::Options::default();
        o.create_if_missing(true);
        o.set_compression_type(rocksdb::DBCompressionType::None);
        o.set_write_buffer_size(4 * 1024 * 1024);
        o.set_max_write_buffer_number(2);
        o.set_max_background_jobs(2);
        let mut b = rocksdb::BlockBasedOptions::default();
        let c = rocksdb::Cache::new_lru_cache(cache);
        b.set_block_cache(&c);
        o.set_block_based_table_factory(&b);
        Ok(rocksdb::DB::open(&o, path)?)
    }
    fn db(&self) -> &rocksdb::DB {
        self.db.as_ref().unwrap()
    }
}
#[cfg(feature = "rocksdb")]
impl Engine for Rocks {
    fn batch(&mut self, rows: &[Entry]) -> Result<()> {
        let mut b = rocksdb::WriteBatch::default();
        for r in rows {
            b.put(&r.key, &r.value);
        }
        let mut w = rocksdb::WriteOptions::default();
        w.set_sync(true);
        w.disable_wal(false);
        self.db().write_opt(b, &w)?;
        Ok(())
    }
    fn get(&mut self, k: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.db().get(k)?)
    }
    fn range(&mut self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        for r in self.db().iterator(rocksdb::IteratorMode::From(
            start,
            rocksdb::Direction::Forward,
        )) {
            let (k, v) = r?;
            if end.is_some_and(|e| k.as_ref() >= e) {
                break;
            }
            out.push(Entry {
                key: k.to_vec(),
                value: v.to_vec(),
            });
        }
        Ok(out)
    }
    fn clear(&mut self) -> Result<()> {
        self.reopen()
    }
    fn reopen(&mut self) -> Result<()> {
        self.db.take();
        self.db = Some(Self::open(&self.path, self.cache)?);
        Ok(())
    }
    fn maintain(&mut self) -> Result<()> {
        self.db().flush()?;
        self.db().compact_range::<&[u8], &[u8]>(None, None);
        Ok(())
    }
    fn stats(&self) -> Result<Value> {
        Ok(json!({"physical_bytes":owned_bytes(&self.path)?,
        "engine_version":"rocksdb crate 0.25.0; native lib version pinned by Cargo.lock", "sync":true,"wal_enabled":true,
        "compression":"none", "configured_block_cache_bytes":self.cache,"write_buffer_size":4194304,
        "max_write_buffers":2,"max_background_jobs":2,
        "cache_clear":"close/reopen; OS cache unchanged, memtable/recovery state differs", "maintenance":"blocking flush plus compact_range, full output verified",
        "contract":"atomic WriteBatch with synchronous WAL; not optimistic transaction/conflict equivalence"}))
    }
}
