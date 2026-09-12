//! Immutable, checksummed arena records and a pageable copy-on-write AVL index.
//! Offsets are local to an immutable epoch. There is no resident per-key map.
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::append_buffer::{AppendBuffer, APPEND_CAPACITY};
use super::transaction::Transaction;

const MAGIC: &[u8; 8] = b"SPIREC01";
const META: &[u8; 8] = b"SPIMETA1";
const HEADER: usize = 32;
const META_LEN: usize = 64;
pub const MAX_KEY: usize = 4096;
pub const MAX_VALUE: usize = 16 * 1024 * 1024;
const PREFIX: &str = "arena-";

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Corrupt(String),
    Conflict,
    Locked,
    Budget(String),
    Poisoned,
}
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O: {e}"),
            Self::Corrupt(e) => write!(f, "corrupt store: {e}"),
            Self::Conflict => write!(f, "transaction conflict"),
            Self::Locked => write!(f, "database is already open"),
            Self::Budget(e) => write!(f, "resource budget: {e}"),
            Self::Poisoned => write!(f, "I/O outcome uncertain: drop handles and reopen"),
        }
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Entry {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}
#[derive(Clone, Debug)]
pub struct Options {
    pub cache_bytes: usize,
    pub transaction_bytes: usize,
    pub scan_bytes: usize,
    /// Bounded logical mutation journal for predicate and missing-key validation.
    pub conflict_bytes: usize,
    /// Exclusive synthetic fault injection. None in production.
    pub fail_at: Option<usize>,
    /// Fragment writes to exercise complete-transfer handling. None normally.
    pub write_chunk: Option<usize>,
    /// Coalesce immutable records in a writer-private 64 KiB buffer.
    /// False retains the unbuffered control with identical durable barriers.
    pub append_buffer: bool,
    /// Admit small immutable values on point reads within the shared cache budget.
    /// Scans and compaction do not populate the value cache.
    pub value_cache: bool,
    /// Opt-in bulk creation and replacement-only multi-key copy-on-write.
    /// Mixed structural batches and single keys retain the sequential path.
    /// Temporary borrowed-entry storage is capped at 64 KiB and one eighth of
    /// transaction_bytes, separately from staging charge. No RSS bound implied.
    pub grouped_updates: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            cache_bytes: 8 * 1024 * 1024,
            transaction_bytes: 32 * 1024 * 1024,
            scan_bytes: 32 * 1024 * 1024,
            conflict_bytes: 2 * 1024 * 1024,
            fail_at: None,
            write_chunk: None,
            append_buffer: true,
            value_cache: false,
            grouped_updates: false,
        }
    }
}
#[derive(Default, Debug, serde::Serialize)]
pub struct Stats {
    pub generation: u64,
    pub keys: u64,
    pub committed_bytes: u64,
    pub physical_bytes: u64,
    pub cache_bytes: usize,
    pub cache_entries: usize,
    /// Usable container slots, not allocated bytes or process RSS.
    pub cache_map_capacity: usize,
    pub cache_order_capacity: usize,
    pub cache_value_entries: usize,
    pub cache_value_capacity: usize,
    /// Sum of container capacities across pinned retired epochs.
    pub retired_cache_map_capacity: usize,
    pub retired_cache_order_capacity: usize,
    pub retired_cache_value_capacity: usize,
    pub reads: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    /// Attempted arena positional writes, including partial transfers.
    pub arena_write_calls: u64,
    pub retained_epochs: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Root {
    pub generation: u64,
    pub epoch: u64,
    pub root: u64,
    pub end: u64,
    pub count: u64,
}
#[derive(Clone, Debug)]
pub(crate) struct Node {
    pub left: u64,
    pub right: u64,
    pub value: u64,
    pub revision: u64,
    pub height: u32,
    pub key: Vec<u8>,
}
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum CacheKey {
    Node(u64),
    Value(u64),
}
struct Cache {
    nodes: HashMap<u64, Arc<Node>>,
    values: HashMap<u64, Arc<Vec<u8>>>,
    order: VecDeque<CacheKey>,
    bytes: usize,
    limit: usize,
    admit_values: bool,
}
impl Cache {
    fn retire(&mut self) {
        self.nodes = HashMap::new();
        self.values = HashMap::new();
        self.order = VecDeque::new();
        self.bytes = 0;
        self.limit = 0;
        self.admit_values = false;
    }
    fn evict_until(&mut self, needed: usize) {
        while self.bytes + needed > self.limit {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            match old {
                CacheKey::Node(at) => {
                    if let Some(v) = self.nodes.remove(&at) {
                        self.bytes -= v.key.len() + 192;
                    }
                }
                CacheKey::Value(at) => {
                    if let Some(v) = self.values.remove(&at) {
                        self.bytes -= v.capacity() + 192;
                    }
                }
            }
        }
    }
    fn insert_node(&mut self, at: u64, n: Arc<Node>) {
        let size = n.key.len() + 192;
        if size > self.limit || self.nodes.contains_key(&at) {
            return;
        }
        self.evict_until(size);
        if self.bytes + size > self.limit {
            return;
        }
        self.bytes += size;
        self.order.push_back(CacheKey::Node(at));
        self.nodes.insert(at, n);
    }
    fn can_admit_value(&self, capacity: usize, len: usize) -> bool {
        self.admit_values
            && len <= 4096
            && capacity
                .checked_add(192)
                .is_some_and(|size| size <= self.limit / 8)
    }
    fn insert_value(&mut self, at: u64, value: Arc<Vec<u8>>) {
        let size = value.capacity() + 192;
        // Keep one value from consuming a large fraction of the shared cache.
        if !self.can_admit_value(value.capacity(), value.len()) || self.values.contains_key(&at) {
            return;
        }
        self.evict_until(size);
        if self.bytes + size > self.limit {
            return;
        }
        self.bytes += size;
        self.order.push_back(CacheKey::Value(at));
        self.values.insert(at, value);
    }
    fn node(&self, at: u64) -> Option<Arc<Node>> {
        self.nodes.get(&at).cloned()
    }
    fn value(&self, at: u64) -> Option<Arc<Vec<u8>>> {
        self.values.get(&at).cloned()
    }
    fn clear(&mut self) {
        self.nodes.clear();
        self.values.clear();
        self.order.clear();
        self.bytes = 0;
    }
}
struct Lease {
    _file: File,
}
pub(crate) struct Epoch {
    file: File,
    path: PathBuf,
    cache: Mutex<Cache>,
    _lease: Arc<Lease>,
    reads: AtomicU64,
    bytes_read: AtomicU64,
    bytes_written: AtomicU64,
    write_calls: AtomicU64,
    value_cache_enabled: bool,
}
impl Epoch {
    fn open(
        path: PathBuf,
        lease: Arc<Lease>,
        cache_bytes: usize,
        create: bool,
        admit_values: bool,
    ) -> Result<Arc<Self>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(create)
            .open(&path)?;
        Ok(Arc::new(Self {
            file,
            path,
            cache: Mutex::new(Cache {
                nodes: HashMap::new(),
                values: HashMap::new(),
                order: VecDeque::new(),
                bytes: 0,
                limit: cache_bytes,
                admit_values,
            }),
            _lease: lease,
            reads: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            write_calls: AtomicU64::new(0),
            value_cache_enabled: admit_values,
        }))
    }
    fn write(&self, bytes: &[u8], at: u64, chunk: Option<usize>) -> Result<()> {
        write_at_tracked(
            &self.file,
            bytes,
            at,
            chunk,
            Some((&self.write_calls, &self.bytes_written)),
        )?;
        Ok(())
    }
    fn record(&self, at: u64, end: u64, kind: u8) -> Result<Vec<u8>> {
        if at < 8 || at.checked_add(HEADER as u64).is_none_or(|v| v > end) {
            return Err(Error::Corrupt("record offset".into()));
        }
        let mut h = [0; HEADER];
        read_exact_at(&self.file, &mut h, at)?;
        let len = get32(&h, 24) as usize;
        let max = if kind == 1 { MAX_KEY + 40 } else { MAX_VALUE };
        if &h[..8] != MAGIC
            || h[8] != kind
            || len > max
            || at
                .checked_add((HEADER + len) as u64)
                .is_none_or(|v| v > end)
        {
            return Err(Error::Corrupt("record header/length".into()));
        }
        let expected = get32(&h, 28);
        h[28..32].fill(0);
        let mut payload = vec![0; len];
        read_exact_at(&self.file, &mut payload, at + HEADER as u64)?;
        if checksum(&[&h, &payload]) != expected {
            return Err(Error::Corrupt("record checksum".into()));
        }
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.bytes_read
            .fetch_add((HEADER + len) as u64, Ordering::Relaxed);
        Ok(payload)
    }
    pub(crate) fn node(&self, at: u64, end: u64) -> Result<Arc<Node>> {
        if let Some(n) = self.cache.lock().unwrap().node(at) {
            return Ok(n);
        }
        let p = self.record(at, end, 1)?;
        let n = decode_node(&p, at)?;
        self.cache.lock().unwrap().insert_node(at, n.clone());
        Ok(n)
    }
    /// Scan/maintenance reads may reuse admitted payload, but cannot pollute
    /// the value cache with one-off values.
    pub(crate) fn value(&self, at: u64, end: u64) -> Result<Vec<u8>> {
        self.read_value(at, end, false)
    }
    fn point_value(&self, at: u64, end: u64) -> Result<Vec<u8>> {
        self.read_value(at, end, true)
    }
    fn read_value(&self, at: u64, end: u64, admit: bool) -> Result<Vec<u8>> {
        // Preserve the original direct-read path when this expert is disabled.
        if !self.value_cache_enabled {
            return self.record(at, end, 2);
        }
        // Clone the immutable Arc under the mutex, then copy the caller's
        // returned value outside it. Callers never mutate cached storage.
        let cached = { self.cache.lock().unwrap().value(at) };
        if let Some(value) = cached {
            if at
                .checked_add((HEADER + value.len()) as u64)
                .is_none_or(|limit| limit > end)
            {
                return Err(Error::Corrupt("cached value outside snapshot".into()));
            }
            return Ok((*value).clone());
        }
        let value = self.record(at, end, 2)?;
        if admit {
            let should_admit = {
                self.cache
                    .lock()
                    .unwrap()
                    .can_admit_value(value.capacity(), value.len())
            };
            if should_admit {
                let shared = Arc::new(value);
                // Retirement can race with this read. Recheck admission while
                // holding the cache lock rather than resurrecting old capacity.
                self.cache.lock().unwrap().insert_value(at, shared.clone());
                return Ok((*shared).clone());
            }
        }
        // Large and uncached values are returned directly, not cloned in full.
        Ok(value)
    }
}
#[derive(Clone)]
pub struct Snapshot {
    pub(crate) epoch: Arc<Epoch>,
    pub(crate) root: Root,
    pub(crate) scan_budget: usize,
}
impl Snapshot {
    pub fn generation(&self) -> u64 {
        self.root.generation
    }
    pub fn len(&self) -> u64 {
        self.root.count
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .find(key)?
            .map(|n| self.epoch.point_value(n.value, self.root.end))
            .transpose()?)
    }
    pub(crate) fn find(&self, key: &[u8]) -> Result<Option<Arc<Node>>> {
        if key.len() > MAX_KEY {
            return Err(Error::Budget("key exceeds 4096 bytes".into()));
        }
        let mut at = self.root.root;
        while at != 0 {
            let n = self.epoch.node(at, self.root.end)?;
            match key.cmp(&n.key) {
                std::cmp::Ordering::Less => at = n.left,
                std::cmp::Ordering::Greater => at = n.right,
                std::cmp::Ordering::Equal => return Ok(Some(n)),
            }
        }
        Ok(None)
    }
    pub fn scan(&self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Entry>> {
        if start.len() > MAX_KEY || end.is_some_and(|e| e.len() > MAX_KEY || start > e) {
            return Err(Error::Budget("invalid range bounds".into()));
        }
        let mut out = Vec::new();
        let mut bytes = 0;
        for row in self.cursor(start, end)? {
            let n = row?;
            let value = self.epoch.value(n.value, self.root.end)?;
            bytes += n.key.len() + value.len() + 64;
            if bytes > self.scan_budget {
                return Err(Error::Budget(
                    "scan output exceeds configured budget".into(),
                ));
            }
            out.push(Entry {
                key: n.key.clone(),
                value,
            });
        }
        Ok(out)
    }
    pub fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<Entry>> {
        self.scan(prefix, prefix_end(prefix).as_deref())
    }
    pub(crate) fn cursor(&self, start: &[u8], end: Option<&[u8]>) -> Result<Cursor> {
        let mut c = Cursor {
            view: self.clone(),
            stack: Vec::new(),
            end: end.map(Vec::from),
            failed: false,
        };
        let mut at = self.root.root;
        while at != 0 {
            let n = self.epoch.node(at, self.root.end)?;
            if n.key.as_slice() < start {
                at = n.right;
            } else {
                at = n.left;
                c.stack.push(n);
            }
        }
        Ok(c)
    }
}
pub(crate) struct Cursor {
    view: Snapshot,
    stack: Vec<Arc<Node>>,
    end: Option<Vec<u8>>,
    failed: bool,
}
impl Iterator for Cursor {
    type Item = Result<Arc<Node>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let n = self.stack.pop()?;
        if self.end.as_ref().is_some_and(|e| n.key >= *e) {
            self.stack.clear();
            return None;
        }
        let mut at = n.right;
        while at != 0 {
            match self.view.epoch.node(at, self.view.root.end) {
                Ok(child) => {
                    at = child.left;
                    self.stack.push(child);
                }
                Err(e) => {
                    self.failed = true;
                    return Some(Err(e));
                }
            }
        }
        Some(Ok(n))
    }
}
pub(crate) struct State {
    pub view: Snapshot,
    pub retired: Vec<Arc<Epoch>>,
    pub poisoned: bool,
    pub(crate) changes: VecDeque<(u64, Vec<Vec<u8>>)>,
    pub(crate) history_floor: u64,
    change_bytes: usize,
}
impl State {
    fn record_changes(
        &mut self,
        generation: u64,
        writes: &std::collections::BTreeMap<Vec<u8>, Option<Vec<u8>>>,
        budget: usize,
    ) {
        let charge = 64 + writes.keys().map(|k| k.len() + 64).sum::<usize>();
        if charge > budget {
            self.changes.clear();
            self.change_bytes = 0;
            self.history_floor = generation;
            return;
        }
        while self.change_bytes + charge > budget {
            if let Some((g, keys)) = self.changes.pop_front() {
                self.change_bytes -= 64 + keys.iter().map(|k| k.len() + 64).sum::<usize>();
                self.history_floor = g;
            }
        }
        self.changes
            .push_back((generation, writes.keys().cloned().collect()));
        self.change_bytes += charge;
    }
}
pub(crate) struct Inner {
    pub state: Mutex<State>,
    // Lock order: maintenance, then state. Foreground operations never need
    // maintenance ownership. It protects candidate epochs from collection.
    maintenance: Mutex<()>,
    pub options: Options,
    dir: PathBuf,
    lease: Arc<Lease>,
}
#[derive(Clone)]
pub struct Database {
    pub(crate) inner: Arc<Inner>,
}
impl Database {
    pub fn create(path: impl AsRef<Path>, options: Options) -> Result<Self> {
        validate_options(&options)?;
        let dir = absolute(path.as_ref())?;
        fs::create_dir(&dir)?;
        sync_directory(dir.parent().unwrap())?;
        let lease = lock(&dir)?;
        let epoch = Epoch::open(
            dir.join("arena-0.spi"),
            lease.clone(),
            options.cache_bytes,
            true,
            options.value_cache,
        )?;
        write_at(&epoch.file, b"SPIARE01", 0, None)?;
        epoch.file.sync_all()?;
        sync_directory(&dir)?;
        let root = Root {
            generation: 0,
            epoch: 0,
            root: 0,
            end: 8,
            count: 0,
        };
        publish(&dir, root, &mut Fault::new(&options))?;
        Ok(Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    view: Snapshot {
                        epoch,
                        root,
                        scan_budget: options.scan_bytes,
                    },
                    retired: Vec::new(),
                    poisoned: false,
                    changes: VecDeque::new(),
                    history_floor: 0,
                    change_bytes: 0,
                }),
                maintenance: Mutex::new(()),
                options,
                dir,
                lease,
            }),
        })
    }
    pub fn open(path: impl AsRef<Path>, options: Options) -> Result<Self> {
        validate_options(&options)?;
        let dir = absolute(path.as_ref())?;
        let lease = lock(&dir)?;
        let root = read_manifest(&dir.join("manifest.spi"))?;
        let epoch = Epoch::open(
            dir.join(format!("arena-{}.spi", root.epoch)),
            lease.clone(),
            options.cache_bytes,
            false,
            options.value_cache,
        )?;
        if epoch.file.metadata()?.len() < root.end {
            return Err(Error::Corrupt("arena shorter than committed end".into()));
        }
        let mut h = [0; 8];
        read_exact_at(&epoch.file, &mut h, 0)?;
        if &h != b"SPIARE01" {
            return Err(Error::Corrupt("arena magic".into()));
        }
        // Crash tails are never appended through. The next writer starts at the committed end.
        if root.root != 0 {
            epoch.node(root.root, root.end)?;
        }
        Ok(Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    view: Snapshot {
                        epoch,
                        root,
                        scan_budget: options.scan_bytes,
                    },
                    retired: Vec::new(),
                    poisoned: false,
                    changes: VecDeque::new(),
                    history_floor: root.generation,
                    change_bytes: 0,
                }),
                maintenance: Mutex::new(()),
                options,
                dir,
                lease,
            }),
        })
    }
    pub fn snapshot(&self) -> Result<Snapshot> {
        let s = self.inner.state.lock().unwrap();
        if s.poisoned {
            return Err(Error::Poisoned);
        }
        Ok(s.view.clone())
    }
    pub fn begin(&self) -> Result<Transaction> {
        Transaction::new(self.clone())
    }
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.snapshot()?.get(key)
    }
    pub fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<Entry>> {
        self.snapshot()?.scan_prefix(prefix)
    }
    pub fn clear_cache(&self) {
        self.inner
            .state
            .lock()
            .unwrap()
            .view
            .epoch
            .cache
            .lock()
            .unwrap()
            .clear();
    }
    /// Validate every reachable node and value from disk, bypassing the cache.
    /// This detects corruption, not repairs it. It is explicit O(live data) work.
    pub fn verify(&self) -> Result<u64> {
        let view = self.snapshot()?;
        let disk = Epoch::open(
            view.epoch.path.clone(),
            self.inner.lease.clone(),
            0,
            false,
            false,
        )?;
        fn walk(
            e: &Epoch,
            root: Root,
            at: u64,
            lower: Option<&[u8]>,
            upper: Option<&[u8]>,
            depth: u32,
        ) -> Result<(u64, u32)> {
            if at == 0 {
                return Ok((0, 0));
            }
            if depth > 128 {
                return Err(Error::Corrupt("index depth".into()));
            }
            let n = e.node(at, root.end)?;
            if n.revision == 0
                || n.revision > root.generation
                || lower.is_some_and(|k| n.key.as_slice() <= k)
                || upper.is_some_and(|k| n.key.as_slice() >= k)
            {
                return Err(Error::Corrupt("key order/revision".into()));
            }
            e.value(n.value, root.end)?;
            let (lc, lh) = walk(e, root, n.left, lower, Some(&n.key), depth + 1)?;
            let (rc, rh) = walk(e, root, n.right, Some(&n.key), upper, depth + 1)?;
            if lh.abs_diff(rh) > 1 || n.height != 1 + lh.max(rh) {
                return Err(Error::Corrupt("AVL balance/height".into()));
            }
            Ok((lc + rc + 1, n.height))
        }
        let (count, _) = walk(&disk, view.root, view.root.root, None, None, 0)?;
        if count != view.root.count {
            return Err(Error::Corrupt("key count".into()));
        }
        Ok(count)
    }
    pub fn stats(&self) -> Result<Stats> {
        let s = self.inner.state.lock().unwrap();
        let e = &s.view.epoch;
        let c = e.cache.lock().unwrap();
        let (mut retired_map_capacity, mut retired_order_capacity, mut retired_value_capacity) =
            (0, 0, 0);
        for epoch in &s.retired {
            let cache = epoch.cache.lock().unwrap();
            retired_map_capacity += cache.nodes.capacity();
            retired_order_capacity += cache.order.capacity();
            retired_value_capacity += cache.values.capacity();
        }
        let mut physical = 0;
        for f in fs::read_dir(&self.inner.dir)? {
            let f = f?;
            if f.file_type()?.is_file() {
                physical += f.metadata()?.len();
            }
        }
        Ok(Stats {
            generation: s.view.root.generation,
            keys: s.view.root.count,
            committed_bytes: s.view.root.end,
            physical_bytes: physical,
            cache_bytes: c.bytes,
            cache_entries: c.nodes.len() + c.values.len(),
            cache_map_capacity: c.nodes.capacity(),
            cache_order_capacity: c.order.capacity(),
            cache_value_entries: c.values.len(),
            cache_value_capacity: c.values.capacity(),
            retired_cache_map_capacity: retired_map_capacity,
            retired_cache_order_capacity: retired_order_capacity,
            retired_cache_value_capacity: retired_value_capacity,
            reads: e.reads.load(Ordering::Relaxed),
            bytes_read: e.bytes_read.load(Ordering::Relaxed),
            bytes_written: e.bytes_written.load(Ordering::Relaxed),
            arena_write_calls: e.write_calls.load(Ordering::Relaxed),
            retained_epochs: s.retired.len(),
        })
    }
    pub fn collect(&self) -> Result<usize> {
        let _maintenance = self.inner.maintenance.lock().map_err(|_| Error::Poisoned)?;
        let mut s = self.inner.state.lock().map_err(|_| Error::Poisoned)?;
        if s.poisoned {
            return Err(Error::Poisoned);
        }
        let mut count = 0;
        let mut i = 0;
        while i < s.retired.len() {
            if Arc::strong_count(&s.retired[i]) == 1 {
                // Preserve all pin bookkeeping if unlink fails. File::open
                // shares delete access on Windows, so our unpinned FD may stay
                // open until the pathname is successfully removed.
                fs::remove_file(&s.retired[i].path)?;
                s.retired.remove(i);
                count += 1;
            } else {
                i += 1;
            }
        }
        // Exclusive database ownership means no snapshot from an earlier process survives.
        let current = s.view.epoch.path.clone();
        for f in fs::read_dir(&self.inner.dir)? {
            let f = f?;
            let name = f.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(PREFIX)
                && name.ends_with(".spi")
                && name[6..name.len() - 4].parse::<u64>().is_ok()
                && f.path() != current
                && !s.retired.iter().any(|e| e.path == f.path())
            {
                fs::remove_file(f.path())?;
                count += 1;
            }
        }
        sync_directory(&self.inner.dir)?;
        Ok(count)
    }
    /// Copy a pinned root without holding the foreground state mutex.
    /// Publication still serializes briefly with commits and performs sync I/O.
    /// If a foreground commit changed the root, return Conflict without
    /// publishing. Retry is explicit; continuous writes can starve compaction.
    pub fn compact(&self) -> Result<()> {
        self.compact_with_checkpoints(|| Ok(()), || Ok(()))
    }

    // Private test seams use the same implementation without public test gates
    // or global sleep-based synchronization. Production passes two no-ops.
    fn compact_with_checkpoints(
        &self,
        before_copy: impl FnOnce() -> Result<()>,
        after_copy: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let _maintenance = self.inner.maintenance.lock().map_err(|_| Error::Poisoned)?;
        let old = self.snapshot()?;
        let mut epoch_id = old
            .root
            .epoch
            .checked_add(1)
            .ok_or_else(|| Error::Budget("epoch overflow".into()))?;
        while self
            .inner
            .dir
            .join(format!("arena-{epoch_id}.spi"))
            .exists()
        {
            epoch_id = epoch_id
                .checked_add(1)
                .ok_or_else(|| Error::Budget("epoch overflow".into()))?;
        }
        let epoch = Epoch::open(
            self.inner.dir.join(format!("arena-{epoch_id}.spi")),
            self.inner.lease.clone(),
            0,
            true,
            self.inner.options.value_cache,
        )?;
        // Errors here affect only the unpublished candidate. Leave that file
        // for explicit collect/recovery and keep the current database usable.
        write_at(&epoch.file, b"SPIARE01", 0, self.inner.options.write_chunk)?;
        before_copy()?;
        let mut w = ArenaWriter::new(epoch.clone(), 8, old.root.generation, &self.inner.options);
        let root_at = w.copy_tree(&old, old.root.root)?;
        w.flush_append()?;
        epoch.file.sync_all()?;
        w.fault.hit("compact_data_sync")?;
        sync_directory(&self.inner.dir)?;
        after_copy()?;
        let root = Root {
            epoch: epoch_id,
            root: root_at,
            end: w.end,
            ..old.root
        };
        let mut s = self.inner.state.lock().map_err(|_| Error::Poisoned)?;
        if s.poisoned {
            return Err(Error::Poisoned);
        }
        if s.view.root != old.root {
            return Err(Error::Conflict);
        }
        if let Err(e) = publish(&self.inner.dir, root, &mut w.fault) {
            // Only the publication boundary can make the live root uncertain.
            s.poisoned = true;
            return Err(e);
        }
        old.epoch.cache.lock().unwrap().retire();
        epoch.cache.lock().unwrap().limit = self.inner.options.cache_bytes;
        s.retired.push(old.epoch.clone());
        s.view = Snapshot {
            epoch,
            root,
            scan_budget: old.scan_budget,
        };
        Ok(())
    }
    pub(crate) fn apply(
        &self,
        s: &mut State,
        writes: &std::collections::BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    ) -> Result<u64> {
        if writes.is_empty() {
            return Ok(s.view.root.generation);
        }
        let generation = s
            .view
            .root
            .generation
            .checked_add(1)
            .ok_or_else(|| Error::Budget("generation overflow".into()))?;
        let result = (|| {
            let mut w = ArenaWriter::new(
                s.view.epoch.clone(),
                s.view.root.end,
                generation,
                &self.inner.options,
            );
            let mut root = s.view.root.root;
            let mut count = s.view.root.count;
            let grouped = if self.inner.options.grouped_updates {
                w.try_grouped(root, writes, self.inner.options.transaction_bytes)?
            } else {
                None
            };
            if let Some(next) = grouped {
                if root == 0 {
                    count = writes.len() as u64;
                }
                root = next;
            } else {
                for (key, value) in writes {
                    let exists = w.find(root, key)?.is_some();
                    if let Some(value) = value {
                        let v = w.record(2, value)?;
                        root = w.insert(root, key, v, generation)?;
                        if !exists {
                            count += 1;
                        }
                    } else if exists {
                        root = w.delete(root, key)?;
                        count -= 1;
                    }
                }
            }
            w.flush_append()?;
            w.epoch.file.sync_all()?;
            w.fault.hit("data_sync")?;
            let next = Root {
                generation,
                root,
                end: w.end,
                count,
                ..s.view.root
            };
            publish(&self.inner.dir, next, &mut w.fault)?;
            Ok(next)
        })();
        match result {
            Ok(root) => {
                s.record_changes(generation, writes, self.inner.options.conflict_bytes);
                s.view.root = root;
                Ok(generation)
            }
            Err(e) => {
                s.poisoned = true;
                Err(e)
            }
        }
    }
}
struct ArenaWriter {
    epoch: Arc<Epoch>,
    end: u64,
    generation: u64,
    fault: Fault,
    append: AppendBuffer,
    buffering: bool,
}
impl ArenaWriter {
    fn new(epoch: Arc<Epoch>, end: u64, generation: u64, options: &Options) -> Self {
        Self {
            epoch,
            end,
            generation,
            fault: Fault::new(options),
            append: AppendBuffer::new(end, options.append_buffer),
            buffering: options.append_buffer,
        }
    }
    fn flush_append(&mut self) -> Result<()> {
        let epoch = &self.epoch;
        let fault = &mut self.fault;
        self.append.flush(|at, bytes| {
            // Exercise a genuine partial buffered transfer in crash/fault tests.
            let mid = bytes.len() / 2;
            epoch.write(&bytes[..mid], at, fault.chunk)?;
            fault.hit("append_flush_half")?;
            epoch.write(&bytes[mid..], at + mid as u64, fault.chunk)?;
            fault.hit("append_flush")?;
            // Match direct-write cache admission, but only after the records
            // exist on disk. Writer-private nodes never enter the shared cache.
            // Reuse the bounded serialized buffer, not an extra per-key map.
            let mut cache = epoch.cache.lock().unwrap();
            if cache.limit != 0 {
                let mut offset = 0;
                while offset < bytes.len() {
                    let record = &bytes[offset..];
                    if record.len() < HEADER {
                        return Err(Error::Corrupt("buffered record header".into()));
                    }
                    let end = HEADER + get32(record, 24) as usize;
                    if end > record.len() {
                        return Err(Error::Corrupt("buffered record length".into()));
                    }
                    if record[8] == 1 {
                        let position = at + offset as u64;
                        let node = decode_node(&record[HEADER..end], position)?;
                        cache.insert_node(position, node);
                    }
                    offset += end;
                }
            }
            Ok(())
        })
    }
    fn record(&mut self, kind: u8, payload: &[u8]) -> Result<u64> {
        let mut h = [0; HEADER];
        h[..8].copy_from_slice(MAGIC);
        h[8] = kind;
        put64(&mut h, 16, self.generation);
        put32(&mut h, 24, payload.len() as u32);
        let crc = checksum(&[&h, payload]);
        put32(&mut h, 28, crc);
        let at = self.end;
        let total = HEADER + payload.len();
        let end = at
            .checked_add(total as u64)
            .ok_or_else(|| Error::Budget("arena offset overflow".into()))?;
        if self.buffering && total <= APPEND_CAPACITY {
            if self.append.remaining() < total {
                self.flush_append()?;
            }
            self.append.extend(&h);
            self.fault.hit("record_header")?;
            self.append.extend(payload);
            self.fault.hit("record_payload")?;
            self.end = end;
        } else {
            self.flush_append()?;
            self.epoch.write(&h, at, self.fault.chunk)?;
            self.fault.hit("record_header")?;
            self.epoch
                .write(payload, at + HEADER as u64, self.fault.chunk)?;
            self.fault.hit("record_payload")?;
            self.end = end;
            self.append.advance_direct(end);
        }
        Ok(at)
    }
    fn node(&self, at: u64) -> Result<Arc<Node>> {
        if let Some(bytes) = self.append.pending(at) {
            if bytes.len() >= HEADER {
                let len = get32(bytes, 24) as usize;
                let total = HEADER
                    .checked_add(len)
                    .ok_or_else(|| Error::Corrupt("pending length".into()))?;
                if bytes.len() >= total {
                    let expected = get32(bytes, 28);
                    let mut h = bytes[..HEADER].to_vec();
                    h[28..32].fill(0);
                    if &h[..8] != MAGIC
                        || h[8] != 1
                        || checksum(&[&h, &bytes[HEADER..total]]) != expected
                    {
                        return Err(Error::Corrupt("pending node checksum".into()));
                    }
                    return decode_node(&bytes[HEADER..total], at);
                }
            }
        }
        self.epoch.node(at, self.append.start())
    }
    fn height(&self, at: u64) -> Result<u32> {
        if at == 0 {
            Ok(0)
        } else {
            Ok(self.node(at)?.height)
        }
    }
    fn save(&mut self, mut n: Node) -> Result<u64> {
        n.height = 1 + self.height(n.left)?.max(self.height(n.right)?);
        let mut p = vec![0; 40 + n.key.len()];
        put64(&mut p, 0, n.left);
        put64(&mut p, 8, n.right);
        put64(&mut p, 16, n.value);
        put64(&mut p, 24, n.revision);
        put32(&mut p, 32, n.height);
        put32(&mut p, 36, n.key.len() as u32);
        p[40..].copy_from_slice(&n.key);
        let at = self.record(1, &p)?;
        if at < self.append.start() {
            self.epoch
                .cache
                .lock()
                .unwrap()
                .insert_node(at, Arc::new(n));
        }
        Ok(at)
    }
    fn balance(&mut self, mut n: Node) -> Result<u64> {
        let hl = self.height(n.left)? as i64;
        let hr = self.height(n.right)? as i64;
        if hl - hr > 1 {
            let mut l = (*self.node(n.left)?).clone();
            if self.height(l.left)? < self.height(l.right)? {
                let mut m = (*self.node(l.right)?).clone();
                l.right = m.left;
                n.left = m.right;
                m.left = self.save(l)?;
                m.right = self.save(n)?;
                return self.save(m);
            }
            n.left = l.right;
            l.right = self.save(n)?;
            return self.save(l);
        }
        if hr - hl > 1 {
            let mut r = (*self.node(n.right)?).clone();
            if self.height(r.right)? < self.height(r.left)? {
                let mut m = (*self.node(r.left)?).clone();
                r.left = m.right;
                n.right = m.left;
                m.right = self.save(r)?;
                m.left = self.save(n)?;
                return self.save(m);
            }
            n.right = r.left;
            r.left = self.save(n)?;
            return self.save(r);
        }
        self.save(n)
    }
    fn find(&self, mut at: u64, key: &[u8]) -> Result<Option<Arc<Node>>> {
        while at != 0 {
            let n = self.node(at)?;
            match key.cmp(&n.key) {
                std::cmp::Ordering::Equal => return Ok(Some(n)),
                std::cmp::Ordering::Less => at = n.left,
                std::cmp::Ordering::Greater => at = n.right,
            }
        }
        Ok(None)
    }
    fn insert(&mut self, at: u64, key: &[u8], value: u64, revision: u64) -> Result<u64> {
        if at == 0 {
            return self.save(Node {
                left: 0,
                right: 0,
                value,
                revision,
                height: 1,
                key: key.to_vec(),
            });
        }
        let mut n = (*self.node(at)?).clone();
        match key.cmp(&n.key) {
            std::cmp::Ordering::Equal => {
                n.value = value;
                n.revision = revision;
            }
            std::cmp::Ordering::Less => n.left = self.insert(n.left, key, value, revision)?,
            std::cmp::Ordering::Greater => n.right = self.insert(n.right, key, value, revision)?,
        }
        self.balance(n)
    }
    fn delete(&mut self, at: u64, key: &[u8]) -> Result<u64> {
        if at == 0 {
            return Ok(0);
        }
        let mut n = (*self.node(at)?).clone();
        match key.cmp(&n.key) {
            std::cmp::Ordering::Less => n.left = self.delete(n.left, key)?,
            std::cmp::Ordering::Greater => n.right = self.delete(n.right, key)?,
            std::cmp::Ordering::Equal => {
                if n.left == 0 {
                    return Ok(n.right);
                }
                if n.right == 0 {
                    return Ok(n.left);
                }
                let mut next = self.node(n.right)?;
                while next.left != 0 {
                    next = self.node(next.left)?;
                }
                n.key = next.key.clone();
                n.value = next.value;
                n.revision = next.revision;
                n.right = self.delete(n.right, &n.key)?;
            }
        }
        self.balance(n)
    }
    fn try_grouped(
        &mut self,
        root: u64,
        writes: &std::collections::BTreeMap<Vec<u8>, Option<Vec<u8>>>,
        transaction_budget: usize,
    ) -> Result<Option<u64>> {
        let entry_bytes = std::mem::size_of::<(&[u8], &[u8])>();
        let scratch_limit = (transaction_budget / 8).min(64 * 1024);
        if writes.len() < 2
            || writes.len() > scratch_limit / entry_bytes
            || writes.values().any(Option::is_none)
        {
            return Ok(None);
        }
        // Decide eligibility against the immutable root before emitting anything.
        // One missing key makes the entire batch take the structural control.
        if root != 0 {
            for key in writes.keys() {
                if self.find(root, key)?.is_none() {
                    return Ok(None);
                }
            }
        }
        let mut entries = Vec::new();
        if entries.try_reserve_exact(writes.len()).is_err()
            || entries.capacity() > scratch_limit / entry_bytes
        {
            return Ok(None);
        }
        for (key, value) in writes {
            // Every value was checked above. Borrow rather than clone payloads.
            entries.push((key.as_slice(), value.as_ref().unwrap().as_slice()));
        }
        if root == 0 {
            self.build_sorted(&entries, self.generation).map(Some)
        } else {
            self.replace_existing(root, &entries, self.generation)
                .map(Some)
        }
    }
    fn build_sorted(&mut self, entries: &[(&[u8], &[u8])], revision: u64) -> Result<u64> {
        if entries.is_empty() {
            return Ok(0);
        }
        let middle = entries.len() / 2;
        let left = self.build_sorted(&entries[..middle], revision)?;
        let right = self.build_sorted(&entries[middle + 1..], revision)?;
        let value = self.record(2, entries[middle].1)?;
        self.save(Node {
            left,
            right,
            value,
            revision,
            height: 1,
            key: entries[middle].0.to_vec(),
        })
    }
    fn replace_existing(
        &mut self,
        at: u64,
        entries: &[(&[u8], &[u8])],
        revision: u64,
    ) -> Result<u64> {
        if entries.is_empty() {
            return Ok(at);
        }
        if at == 0 {
            return Err(Error::Corrupt("grouped replacement key missing".into()));
        }
        let mut node = (*self.node(at)?).clone();
        let split = entries.partition_point(|(key, _)| *key < node.key.as_slice());
        let (left_entries, at_or_right) = entries.split_at(split);
        let (current_entries, right_entries) = if at_or_right
            .first()
            .is_some_and(|(key, _)| *key == node.key.as_slice())
        {
            (&at_or_right[..1], &at_or_right[1..])
        } else {
            (&at_or_right[..0], at_or_right)
        };
        let left = self.replace_existing(node.left, left_entries, revision)?;
        let right = self.replace_existing(node.right, right_entries, revision)?;
        let mut changed = left != node.left || right != node.right;
        if let Some((_, value)) = current_entries.first() {
            node.value = self.record(2, value)?;
            node.revision = revision;
            changed = true;
        }
        if !changed {
            return Ok(at);
        }
        node.left = left;
        node.right = right;
        self.save(node)
    }
    fn copy_tree(&mut self, old: &Snapshot, at: u64) -> Result<u64> {
        if at == 0 {
            return Ok(0);
        }
        let mut n = (*old.epoch.node(at, old.root.end)?).clone();
        n.left = self.copy_tree(old, n.left)?;
        n.right = self.copy_tree(old, n.right)?;
        let v = old.epoch.value(n.value, old.root.end)?;
        n.value = self.record(2, &v)?;
        self.save(n)
    }
}
fn decode_node(payload: &[u8], at: u64) -> Result<Arc<Node>> {
    if payload.len() < 40
        || payload.len() != 40 + get32(payload, 36) as usize
        || payload.len() > MAX_KEY + 40
    {
        return Err(Error::Corrupt("node shape".into()));
    }
    let n = Arc::new(Node {
        left: get64(payload, 0),
        right: get64(payload, 8),
        value: get64(payload, 16),
        revision: get64(payload, 24),
        height: get32(payload, 32),
        key: payload[40..].to_vec(),
    });
    // Every child/value precedes its parent, forbidding cycles in the format.
    if n.value < 8
        || n.value >= at
        || n.left >= at
        || n.right >= at
        || n.height == 0
        || n.height > 128
    {
        return Err(Error::Corrupt("node links/height".into()));
    }
    Ok(n)
}

pub(crate) fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut e = prefix.to_vec();
    while let Some(x) = e.pop() {
        if x < 255 {
            e.push(x + 1);
            return Some(e);
        }
    }
    None
}
fn validate_options(o: &Options) -> Result<()> {
    if o.write_chunk == Some(0) || o.transaction_bytes == 0 || o.scan_bytes == 0 {
        return Err(Error::Budget("nonzero budgets required".into()));
    }
    Ok(())
}
fn absolute(p: &Path) -> Result<PathBuf> {
    Ok(if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    })
}
fn lock(dir: &Path) -> Result<Arc<Lease>> {
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("lock.spi"))?;
    match f.try_lock() {
        Ok(()) => Ok(Arc::new(Lease { _file: f })),
        Err(TryLockError::WouldBlock) => Err(Error::Locked),
        Err(TryLockError::Error(e)) => Err(Error::Io(e)),
    }
}
struct Fault {
    target: Option<usize>,
    at: usize,
    chunk: Option<usize>,
}
impl Fault {
    fn new(o: &Options) -> Self {
        Self {
            target: o.fail_at,
            at: 0,
            chunk: o.write_chunk,
        }
    }
    fn hit(&mut self, stage: &str) -> Result<()> {
        self.at += 1;
        if std::env::var("SPI_CRASH_AT").ok().as_deref() == Some(stage) {
            std::process::exit(86);
        }
        if self.target == Some(self.at) {
            return Err(Error::Io(io::Error::other(format!(
                "injected failure {stage}"
            ))));
        }
        Ok(())
    }
}
fn publish(dir: &Path, r: Root, f: &mut Fault) -> Result<()> {
    let mut b = [0; META_LEN];
    b[..8].copy_from_slice(META);
    put64(&mut b, 8, r.generation);
    put64(&mut b, 16, r.epoch);
    put64(&mut b, 24, r.root);
    put64(&mut b, 32, r.end);
    put64(&mut b, 40, r.count);
    let c = checksum(&[&b]);
    put32(&mut b, 60, c);
    let tmp = dir.join("manifest.pending");
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)?;
    write_at(&file, &b, 0, f.chunk)?;
    f.hit("manifest_write")?;
    file.sync_all()?;
    f.hit("manifest_sync")?;
    drop(file);
    replace(&tmp, &dir.join("manifest.spi"))?;
    f.hit("manifest_replace")?;
    sync_directory(dir)?;
    f.hit("directory_sync")?;
    Ok(())
}
fn read_manifest(p: &Path) -> Result<Root> {
    let mut f = File::open(p)?;
    if f.metadata()?.len() != META_LEN as u64 {
        return Err(Error::Corrupt("manifest size".into()));
    }
    let mut b = [0; META_LEN];
    f.read_exact(&mut b)?;
    let c = get32(&b, 60);
    b[60..64].fill(0);
    if &b[..8] != META || checksum(&[&b]) != c {
        return Err(Error::Corrupt("manifest checksum".into()));
    }
    let r = Root {
        generation: get64(&b, 8),
        epoch: get64(&b, 16),
        root: get64(&b, 24),
        end: get64(&b, 32),
        count: get64(&b, 40),
    };
    if r.end < 8 || (r.root == 0) != (r.count == 0) || r.root >= r.end {
        return Err(Error::Corrupt("manifest root".into()));
    }
    Ok(r)
}
#[cfg(unix)]
fn replace(a: &Path, b: &Path) -> io::Result<()> {
    fs::rename(a, b)
}
#[cfg(windows)]
fn replace(a: &Path, b: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(a: *const u16, b: *const u16, flags: u32) -> i32;
    }
    let a: Vec<u16> = a.as_os_str().encode_wide().chain(Some(0)).collect();
    let b: Vec<u16> = b.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), 1 | 8) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
#[cfg(unix)]
fn sync_directory(p: &Path) -> io::Result<()> {
    File::open(p)?.sync_all()
}
// Windows manifest publication uses MOVEFILE_WRITE_THROUGH, not a silently skipped file flush.
#[cfg(windows)]
fn sync_directory(_p: &Path) -> io::Result<()> {
    Ok(())
}
fn read_exact_at(f: &File, mut b: &mut [u8], mut off: u64) -> io::Result<()> {
    while !b.is_empty() {
        #[cfg(unix)]
        let n = {
            use std::os::unix::fs::FileExt;
            f.read_at(b, off)
        };
        #[cfg(windows)]
        let n = {
            use std::os::windows::fs::FileExt;
            f.seek_read(b, off)
        };
        match n {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read")),
            Ok(n) => {
                off += n as u64;
                b = &mut b[n..];
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
fn write_at(f: &File, b: &[u8], off: u64, chunk: Option<usize>) -> io::Result<()> {
    write_at_tracked(f, b, off, chunk, None)
}
fn write_at_tracked(
    f: &File,
    mut b: &[u8],
    mut off: u64,
    chunk: Option<usize>,
    counters: Option<(&AtomicU64, &AtomicU64)>,
) -> io::Result<()> {
    while !b.is_empty() {
        let part = &b[..b.len().min(chunk.unwrap_or(b.len()))];
        if let Some((calls, _)) = counters {
            calls.fetch_add(1, Ordering::Relaxed);
        }
        #[cfg(unix)]
        let n = {
            use std::os::unix::fs::FileExt;
            f.write_at(part, off)
        };
        #[cfg(windows)]
        let n = {
            use std::os::windows::fs::FileExt;
            f.seek_write(part, off)
        };
        match n {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "zero write")),
            Ok(n) => {
                if let Some((_, written)) = counters {
                    written.fetch_add(n as u64, Ordering::Relaxed);
                }
                off += n as u64;
                b = &b[n..];
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
fn get32(b: &[u8], p: usize) -> u32 {
    u32::from_le_bytes(b[p..p + 4].try_into().unwrap())
}
fn get64(b: &[u8], p: usize) -> u64 {
    u64::from_le_bytes(b[p..p + 8].try_into().unwrap())
}
fn put32(b: &mut [u8], p: usize, v: u32) {
    b[p..p + 4].copy_from_slice(&v.to_le_bytes())
}
fn put64(b: &mut [u8], p: usize, v: u64) {
    b[p..p + 8].copy_from_slice(&v.to_le_bytes())
}
fn checksum(parts: &[&[u8]]) -> u32 {
    let mut c = !0u32;
    for b in parts {
        for &v in *b {
            c ^= v as u32;
            for _ in 0..8 {
                c = (c >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(c & 1));
            }
        }
    }
    !c
}

#[cfg(test)]
mod compaction_tests {
    use super::*;
    use std::sync::mpsc::sync_channel;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn database(name: &str, options: Options) -> Database {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".working/tmp/compaction-checkpoints");
        fs::create_dir_all(&parent).unwrap();
        let path = parent.join(format!(
            "{name}-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let db = Database::create(&path, options).unwrap();
        let mut tx = db.begin().unwrap();
        for i in 0..128u64 {
            tx.set(i.to_be_bytes(), i.to_le_bytes()).unwrap();
        }
        tx.commit().unwrap();
        db
    }

    fn pause(
        entered: std::sync::mpsc::SyncSender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) -> Result<()> {
        entered
            .send(())
            .map_err(|_| Error::Io(io::Error::other("test observer disconnected")))?;
        release.recv_timeout(Duration::from_secs(10)).map_err(|_| {
            Error::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "test release missing",
            ))
        })?;
        Ok(())
    }

    #[test]
    fn foreground_commit_during_preparation_invalidates_candidate() {
        for after_copy in [false, true] {
            let db = database("concurrent-write", Options::default());
            let original = db.snapshot().unwrap();
            let original_manifest = fs::read(db.inner.dir.join("manifest.spi")).unwrap();
            let (entered_tx, entered_rx) = sync_channel(1);
            let (release_tx, release_rx) = sync_channel(1);
            let worker = db.clone();
            let compaction = std::thread::spawn(move || {
                if after_copy {
                    worker.compact_with_checkpoints(|| Ok(()), || pause(entered_tx, release_rx))
                } else {
                    worker.compact_with_checkpoints(|| pause(entered_tx, release_rx), || Ok(()))
                }
            });
            entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            // The maintenance lock protects the candidate, not foreground state.
            assert!(db.inner.maintenance.try_lock().is_err());
            assert!(db.inner.state.try_lock().is_ok());
            assert!(db.inner.dir.join("arena-1.spi").exists());
            assert_eq!(
                fs::read(db.inner.dir.join("manifest.spi")).unwrap(),
                original_manifest
            );
            let foreground = db.clone();
            let (done_tx, done_rx) = sync_channel(1);
            let transaction = std::thread::spawn(move || {
                let result = (|| {
                    assert_eq!(
                        foreground.snapshot()?.get(&0u64.to_be_bytes())?,
                        Some(0u64.to_le_bytes().to_vec())
                    );
                    let mut tx = foreground.begin()?;
                    tx.set(b"new", b"committed during compaction")?;
                    tx.commit()
                })();
                done_tx.send(result).unwrap();
            });
            let result = done_rx.recv_timeout(Duration::from_secs(10));
            // Release the compaction even if the foreground assertion fails.
            release_tx.send(()).unwrap();
            let outcome = compaction.join().unwrap();
            transaction.join().unwrap();
            result
                .expect("foreground blocked during private preparation")
                .unwrap();
            assert!(matches!(outcome, Err(Error::Conflict)));
            assert_eq!(original.get(b"new").unwrap(), None);
            assert_eq!(
                db.get(b"new").unwrap(),
                Some(b"committed during compaction".to_vec())
            );
            assert_eq!(db.collect().unwrap(), 1);
            db.compact().unwrap();
            db.verify().unwrap();
            let path = db.inner.dir.clone();
            drop(original);
            drop(db);
            let reopened = Database::open(path, Options::default()).unwrap();
            assert_eq!(
                reopened.get(b"new").unwrap(),
                Some(b"committed during compaction".to_vec())
            );
        }
    }

    #[test]
    fn candidate_only_failure_keeps_current_database_usable() {
        for after_copy in [false, true] {
            let db = database("candidate-error", Options::default());
            let before = fs::read(db.inner.dir.join("manifest.spi")).unwrap();
            let fail = || {
                Err(Error::Io(io::Error::other(
                    "candidate-only synthetic error",
                )))
            };
            let result = if after_copy {
                db.compact_with_checkpoints(|| Ok(()), fail)
            } else {
                db.compact_with_checkpoints(fail, || Ok(()))
            };
            assert!(matches!(result, Err(Error::Io(_))));
            assert_eq!(fs::read(db.inner.dir.join("manifest.spi")).unwrap(), before);
            let mut tx = db.begin().unwrap();
            tx.set(b"survives", b"yes").unwrap();
            tx.commit().unwrap();
            assert_eq!(db.collect().unwrap(), 1);
            db.compact().unwrap();
            db.verify().unwrap();
        }
    }

    #[test]
    fn collection_cannot_remove_an_active_compaction_candidate() {
        let db = database("collector", Options::default());
        let pinned = db.snapshot().unwrap();
        let (entered_tx, entered_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        let worker = db.clone();
        let compaction = std::thread::spawn(move || {
            worker.compact_with_checkpoints(|| Ok(()), || pause(entered_tx, release_rx))
        });
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(db.inner.maintenance.try_lock().is_err());
        let collector = db.clone();
        let (started_tx, started_rx) = sync_channel(1);
        let (done_tx, done_rx) = sync_channel(1);
        let collection = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx.send(collector.collect()).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(done_rx.try_recv().is_err());
        assert!(db.inner.dir.join("arena-1.spi").exists());
        assert!(db.inner.state.try_lock().is_ok());
        release_tx.send(()).unwrap();
        compaction.join().unwrap().unwrap();
        assert_eq!(
            done_rx
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap(),
            0
        );
        collection.join().unwrap();
        assert_eq!(
            pinned.get(&0u64.to_be_bytes()).unwrap(),
            Some(0u64.to_le_bytes().to_vec())
        );
        drop(pinned);
        assert_eq!(db.collect().unwrap(), 1);
        db.verify().unwrap();
    }
}
