use std::collections::BTreeMap;

use super::storage::{prefix_end, Database, Entry, Error, Result, Snapshot, MAX_KEY, MAX_VALUE};

/// Serializable optimistic transaction. Reads use an immutable root. Commit
/// validates logical keys and predicates under the single writer mutex.
/// A bounded mutation journal detects phantoms and insert/delete ABA cycles.
/// Transactions older than retained validation history abort conservatively.
/// Read-only snapshots need no validation. Read-only transactions may conflict.
pub struct Transaction {
    db: Database,
    snapshot: Snapshot,
    reads: BTreeMap<Vec<u8>, u64>,
    ranges: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    writes: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    bytes: usize,
}

impl Transaction {
    pub(crate) fn new(db: Database) -> Result<Self> {
        let snapshot = db.snapshot()?;
        Ok(Self {
            db,
            snapshot,
            reads: BTreeMap::new(),
            ranges: Vec::new(),
            writes: BTreeMap::new(),
            bytes: 0,
        })
    }
    pub fn generation(&self) -> u64 {
        self.snapshot.generation()
    }
    pub fn staged_bytes(&self) -> usize {
        self.bytes
    }
    pub fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.check_key(key)?;
        self.capture(key)?;
        if let Some(value) = self.writes.get(key) {
            return Ok(value.clone());
        }
        self.snapshot.get(key)
    }
    /// Half-open bytewise range [start, end). None means no upper bound.
    pub fn scan(&mut self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Entry>> {
        self.check_key(start)?;
        if let Some(end) = end {
            self.check_key(end)?;
            if start > end {
                return Err(Error::Budget("range start exceeds end".into()));
            }
        }
        let charge = start.len() + end.map_or(0, <[u8]>::len) + 128;
        self.budget(self.bytes + charge)?;
        let mut rows = BTreeMap::new();
        let mut bytes = 0;
        for node in self.snapshot.cursor(start, end)? {
            let node = node?;
            if self.writes.contains_key(&node.key) {
                continue;
            }
            let value = self
                .snapshot
                .epoch
                .value(node.value, self.snapshot.root.end)?;
            bytes += node.key.len() + value.len() + 128;
            if bytes > self.db.inner.options.scan_bytes {
                return Err(Error::Budget("scan output exceeds budget".into()));
            }
            rows.insert(node.key.clone(), value);
        }
        for (key, value) in self.writes.range(start.to_vec()..) {
            if end.is_some_and(|e| key.as_slice() >= e) {
                break;
            }
            if let Some(value) = value {
                bytes += key.len() + value.len() + 128;
                if bytes > self.db.inner.options.scan_bytes {
                    return Err(Error::Budget("scan output exceeds budget".into()));
                }
                rows.insert(key.clone(), value.clone());
            }
        }
        self.bytes += charge;
        self.ranges.push((start.to_vec(), end.map(Vec::from)));
        Ok(rows
            .into_iter()
            .map(|(key, value)| Entry { key, value })
            .collect())
    }
    pub fn scan_prefix(&mut self, prefix: &[u8]) -> Result<Vec<Entry>> {
        self.scan(prefix, prefix_end(prefix).as_deref())
    }
    pub fn set(&mut self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Result<()> {
        let key = key.into();
        let value = value.into();
        if value.len() > MAX_VALUE {
            return Err(Error::Budget("value exceeds 16 MiB".into()));
        }
        self.stage(key, Some(value))
    }
    pub fn delete(&mut self, key: impl Into<Vec<u8>>) -> Result<()> {
        self.stage(key.into(), None)
    }
    fn stage(&mut self, key: Vec<u8>, value: Option<Vec<u8>>) -> Result<()> {
        self.check_key(&key)?;
        let old = self
            .writes
            .get(&key)
            .map_or(0, |v| key.len() + v.as_ref().map_or(0, Vec::len) + 192);
        let read_charge = if self.reads.contains_key(&key) {
            0
        } else {
            key.len() + 128
        };
        let size =
            self.bytes - old + key.len() + value.as_ref().map_or(0, Vec::len) + 192 + read_charge;
        self.budget(size)?;
        self.capture(&key)?;
        self.writes.insert(key, value);
        self.bytes = size;
        Ok(())
    }
    pub fn commit(self) -> Result<u64> {
        let mut state = self.db.inner.state.lock().map_err(|_| Error::Poisoned)?;
        if state.poisoned {
            return Err(Error::Poisoned);
        }
        for (key, expected) in &self.reads {
            if state.view.find(key)?.map(|n| n.revision).unwrap_or(0) != *expected {
                return Err(Error::Conflict);
            }
        }
        if state.view.generation() != self.snapshot.generation() {
            if self.snapshot.generation() < state.history_floor {
                return Err(Error::Conflict);
            }
            for (generation, keys) in &state.changes {
                if *generation <= self.snapshot.generation() {
                    continue;
                }
                for key in keys {
                    if self.reads.contains_key(key)
                        || self.ranges.iter().any(|(start, end)| {
                            key >= start && end.as_ref().is_none_or(|end| key < end)
                        })
                    {
                        return Err(Error::Conflict);
                    }
                }
            }
        }
        self.db.apply(&mut state, &self.writes)
    }
    fn capture(&mut self, key: &[u8]) -> Result<()> {
        if !self.reads.contains_key(key) {
            let size = self.bytes + key.len() + 128;
            self.budget(size)?;
            let version = self.snapshot.find(key)?.map(|n| n.revision).unwrap_or(0);
            self.reads.insert(key.to_vec(), version);
            self.bytes = size;
        }
        Ok(())
    }
    fn budget(&self, size: usize) -> Result<()> {
        if size > self.db.inner.options.transaction_bytes {
            Err(Error::Budget(
                "transaction staging/read metadata exceeds budget".into(),
            ))
        } else {
            Ok(())
        }
    }
    fn check_key(&self, key: &[u8]) -> Result<()> {
        if key.len() > MAX_KEY {
            Err(Error::Budget("key exceeds 4096 bytes".into()))
        } else {
            Ok(())
        }
    }
}
