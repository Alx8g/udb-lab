//! Official DuckDB C ABI loaded from an explicit library path; hash recorded.
//! Boundary declarations follow duckdb.h v1.5.5. All handles/results have RAII.
use super::*;
use libloading::Library;
use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;
type Handle = *mut c_void;
#[repr(C)]
#[derive(Default)]
struct RawResult {
    columns: u64,
    rows: u64,
    changed: u64,
    column_data: Handle,
    error: *mut c_char,
    internal: Handle,
}
#[repr(C)]
struct Blob {
    data: Handle,
    size: u64,
}
struct Api {
    open: unsafe extern "C" fn(*const c_char, *mut Handle) -> u32,
    close: unsafe extern "C" fn(*mut Handle),
    connect: unsafe extern "C" fn(Handle, *mut Handle) -> u32,
    disconnect: unsafe extern "C" fn(*mut Handle),
    query: unsafe extern "C" fn(Handle, *const c_char, *mut RawResult) -> u32,
    destroy: unsafe extern "C" fn(*mut RawResult),
    error: unsafe extern "C" fn(*mut RawResult) -> *const c_char,
    prepare: unsafe extern "C" fn(Handle, *const c_char, *mut Handle) -> u32,
    prepare_error: unsafe extern "C" fn(Handle) -> *const c_char,
    destroy_prepare: unsafe extern "C" fn(*mut Handle),
    bind_blob: unsafe extern "C" fn(Handle, u64, *const c_void, u64) -> u32,
    execute: unsafe extern "C" fn(Handle, *mut RawResult) -> u32,
    row_count: unsafe extern "C" fn(*mut RawResult) -> u64,
    value_blob: unsafe extern "C" fn(*mut RawResult, u64, u64) -> Blob,
    is_null: unsafe extern "C" fn(*mut RawResult, u64, u64) -> bool,
    free: unsafe extern "C" fn(Handle),
    version: unsafe extern "C" fn() -> *const c_char,
    _library: Library,
}
impl Api {
    fn load() -> Result<Self> {
        let path = std::env::var_os("SPI_DUCKDB_LIBRARY")
            .ok_or("SPI_DUCKDB_LIBRARY must name an official shared library")?;
        unsafe {
            let l = Library::new(path)?;
            let version =
                *l.get::<unsafe extern "C" fn() -> *const c_char>(b"duckdb_library_version\0")?;
            if message(version()) != "v1.5.5" {
                return Err("DuckDB C ABI adapter requires version v1.5.5".into());
            }
            macro_rules! sym {
                ($n:literal) => {
                    *l.get(concat!($n, "\0").as_bytes())?
                };
            }
            Ok(Self {
                open: sym!("duckdb_open"),
                close: sym!("duckdb_close"),
                connect: sym!("duckdb_connect"),
                disconnect: sym!("duckdb_disconnect"),
                query: sym!("duckdb_query"),
                destroy: sym!("duckdb_destroy_result"),
                error: sym!("duckdb_result_error"),
                prepare: sym!("duckdb_prepare"),
                prepare_error: sym!("duckdb_prepare_error"),
                destroy_prepare: sym!("duckdb_destroy_prepare"),
                bind_blob: sym!("duckdb_bind_blob"),
                execute: sym!("duckdb_execute_prepared"),
                row_count: sym!("duckdb_row_count"),
                value_blob: sym!("duckdb_value_blob"),
                is_null: sym!("duckdb_value_is_null"),
                free: sym!("duckdb_free"),
                version: sym!("duckdb_library_version"),
                _library: l,
            })
        }
    }
}
fn message(p: *const c_char) -> String {
    if p.is_null() {
        "DuckDB operation failed".into()
    } else {
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}
struct QueryResult<'a> {
    raw: RawResult,
    api: &'a Api,
}
impl Drop for QueryResult<'_> {
    fn drop(&mut self) {
        unsafe { (self.api.destroy)(&mut self.raw) }
    }
}
impl QueryResult<'_> {
    fn blob(&mut self, col: u64, row: u64) -> Result<Vec<u8>> {
        if unsafe { (self.api.is_null)(&mut self.raw, col, row) } {
            return Err("unexpected DuckDB NULL BLOB".into());
        }
        let b = unsafe { (self.api.value_blob)(&mut self.raw, col, row) };
        if b.data.is_null() && b.size != 0 {
            return Err("DuckDB BLOB allocation failed".into());
        }
        let data = if b.size == 0 {
            vec![]
        } else {
            unsafe { std::slice::from_raw_parts(b.data as *const u8, usize::try_from(b.size)?) }
                .to_vec()
        };
        unsafe { (self.api.free)(b.data) };
        Ok(data)
    }
}
pub struct Duck {
    api: Api,
    db: Handle,
    connection: Handle,
    statements: Vec<Handle>,
    path: PathBuf,
    cache: usize,
}
impl Drop for Duck {
    fn drop(&mut self) {
        self.close();
    }
}
impl Duck {
    pub fn new(path: PathBuf, cache: usize) -> Result<Self> {
        fs::create_dir(&path)?;
        let mut this = Self {
            api: Api::load()?,
            db: ptr::null_mut(),
            connection: ptr::null_mut(),
            statements: vec![],
            path,
            cache,
        };
        this.open()?;
        this.query("CREATE TABLE kv(k BLOB PRIMARY KEY, v BLOB NOT NULL)")?;
        this.prepare()?;
        Ok(this)
    }
    fn open(&mut self) -> Result<()> {
        let path = CString::new(
            self.path
                .join("data.duckdb")
                .to_str()
                .ok_or("non UTF8 DB path")?,
        )?;
        if unsafe { (self.api.open)(path.as_ptr(), &mut self.db) } != 0 {
            return Err("DuckDB open failed".into());
        }
        if unsafe { (self.api.connect)(self.db, &mut self.connection) } != 0 {
            return Err("DuckDB connect failed".into());
        }
        // DuckDB is not viable with 1 KiB whole-engine memory. Declare actual min.
        self.query(&format!(
            "SET memory_limit='{}B'; SET threads=1",
            self.cache.max(64 * 1024 * 1024)
        ))?;
        Ok(())
    }
    fn close(&mut self) {
        unsafe {
            for s in &mut self.statements {
                (self.api.destroy_prepare)(s);
            }
            self.statements.clear();
            if !self.connection.is_null() {
                (self.api.disconnect)(&mut self.connection);
            }
            if !self.db.is_null() {
                (self.api.close)(&mut self.db);
            }
        }
    }
    fn query(&self, sql: &str) -> Result<QueryResult<'_>> {
        let c = CString::new(sql)?;
        let mut r = QueryResult {
            raw: RawResult::default(),
            api: &self.api,
        };
        if unsafe { (self.api.query)(self.connection, c.as_ptr(), &mut r.raw) } != 0 {
            return Err(message(unsafe { (self.api.error)(&mut r.raw) }).into());
        }
        Ok(r)
    }
    fn prepare(&mut self) -> Result<()> {
        for sql in [
            "INSERT INTO kv VALUES (?,?) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            "SELECT v FROM kv WHERE k=?",
            "SELECT k,v FROM kv WHERE k>=? AND k<? ORDER BY k",
            "SELECT k,v FROM kv WHERE k>=? ORDER BY k",
        ] {
            let c = CString::new(sql)?;
            let mut stmt = ptr::null_mut();
            if unsafe { (self.api.prepare)(self.connection, c.as_ptr(), &mut stmt) } != 0 {
                let e = message(unsafe { (self.api.prepare_error)(stmt) });
                unsafe { (self.api.destroy_prepare)(&mut stmt) };
                return Err(e.into());
            }
            self.statements.push(stmt);
        }
        Ok(())
    }
    fn execute(&self, index: usize, params: &[&[u8]]) -> Result<QueryResult<'_>> {
        let stmt = self.statements[index];
        for (i, p) in params.iter().enumerate() {
            if unsafe {
                (self.api.bind_blob)(
                    stmt,
                    i as u64 + 1,
                    p.as_ptr() as *const c_void,
                    p.len() as u64,
                )
            } != 0
            {
                return Err("DuckDB bind failed".into());
            }
        }
        let mut r = QueryResult {
            raw: RawResult::default(),
            api: &self.api,
        };
        if unsafe { (self.api.execute)(stmt, &mut r.raw) } != 0 {
            return Err(message(unsafe { (self.api.error)(&mut r.raw) }).into());
        }
        Ok(r)
    }
}
impl Engine for Duck {
    fn batch(&mut self, rows: &[Entry]) -> Result<()> {
        self.query("BEGIN TRANSACTION")?;
        let result = (|| -> Result<()> {
            for r in rows {
                self.execute(0, &[&r.key, &r.value])?;
            }
            self.query("COMMIT")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = self.query("ROLLBACK");
        }
        result
    }
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut r = self.execute(1, &[key])?;
        let n = unsafe { (self.api.row_count)(&mut r.raw) };
        match n {
            0 => Ok(None),
            1 => Ok(Some(r.blob(0, 0)?)),
            _ => Err("duplicate DuckDB key".into()),
        }
    }
    fn range(&mut self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Entry>> {
        let mut r = match end {
            Some(e) => self.execute(2, &[start, e])?,
            None => self.execute(3, &[start])?,
        };
        let n = unsafe { (self.api.row_count)(&mut r.raw) };
        let mut out = Vec::new();
        for row in 0..n {
            out.push(Entry {
                key: r.blob(0, row)?,
                value: r.blob(1, row)?,
            });
        }
        Ok(out)
    }
    fn clear(&mut self) -> Result<()> {
        self.reopen()
    }
    fn reopen(&mut self) -> Result<()> {
        self.close();
        self.open()?;
        self.prepare()
    }
    fn maintain(&mut self) -> Result<()> {
        self.query("CHECKPOINT")?;
        Ok(())
    }
    fn stats(&self) -> Result<Value> {
        let bytes = fs::read_dir(&self.path)?.try_fold(0u64, |sum, e| -> Result<u64> {
            Ok(sum + e?.metadata()?.len())
        })?;
        Ok(
            json!({"physical_bytes":bytes,"duckdb_version":message(unsafe{(self.api.version)()}),
            "durability":"persistent DB default WAL+commit synchronization", "requested_cache_bytes":self.cache,
            "actual_memory_limit_bytes":self.cache.max(64*1024*1024),"threads":1,
            "cache_clear":"close/reopen and reprepare, OS cache uncontrolled", "maintenance":"CHECKPOINT, not VACUUM/full integrity utility",
            "contract":"prepared BLOB primary-key operations, not an analytics throughput benchmark; native result BLOB copies included"}),
        )
    }
}
