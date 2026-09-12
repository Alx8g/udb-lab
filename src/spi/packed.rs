//! Experimental authoritative packed leaves and bounded immutable deltas.
//! Fence AVL nodes locate pages, not individual rows. Logical revisions stay per key.
use super::*;
use std::collections::BTreeMap;

pub(super) const PAGE_BYTES: usize = 16 * 1024;
const INLINE_BYTES: usize = 1024;
const DELTA_BYTES: usize = 4096;
const MAX_DEPTH: u32 = 8;
const BASE: &[u8; 8] = b"SPIPAGE1";
const DELTA: &[u8; 8] = b"SPIDELT1";

#[derive(Clone, Debug)]
pub(crate) enum Value {
    Inline(Vec<u8>),
    Overflow { at: u64, len: usize },
    Deleted,
}
#[derive(Clone, Debug)]
pub(crate) struct Row {
    pub key: Vec<u8>,
    pub revision: u64,
    pub value: Value,
}
impl Row {
    fn bytes(&self) -> usize {
        24 + self.key.len()
            + match &self.value {
                Value::Inline(v) => v.len(),
                _ => 0,
            }
    }
    pub fn value(&self, view: &Snapshot) -> Result<Vec<u8>> {
        match &self.value {
            Value::Inline(v) => Ok(v.clone()),
            Value::Overflow { at, len } => {
                let v = view.epoch.value(*at, view.root.end)?;
                if v.len() != *len {
                    return Err(Error::Corrupt("packed overflow length".into()));
                }
                Ok(v)
            }
            Value::Deleted => Err(Error::Corrupt("visible tombstone".into())),
        }
    }
}
fn encode_rows(rows: &[Row], out: &mut Vec<u8>) {
    for row in rows {
        out.extend_from_slice(&(row.key.len() as u32).to_le_bytes());
        let (len, at) = match &row.value {
            Value::Inline(v) => (v.len() as u32, 0),
            Value::Overflow { at, len } => (*len as u32, *at),
            Value::Deleted => (u32::MAX, 0),
        };
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&row.revision.to_le_bytes());
        out.extend_from_slice(&at.to_le_bytes());
        out.extend_from_slice(&row.key);
        if let Value::Inline(v) = &row.value {
            out.extend_from_slice(v);
        }
    }
}
// Validated immutable bytes plus compact row offsets. Cache owns one serialized
// representation, not cloned keys/values for every logical entry.
pub(super) struct Record {
    pub data: Vec<u8>,
    offsets: Vec<u32>,
    pub generation: u64,
    pub kind: u8,
    pub prev: u64,
    pub depth: u32,
    pub delta_bytes: usize,
}
impl Record {
    pub fn parse(kind: u8, data: Vec<u8>, at: u64, generation: u64) -> Result<Self> {
        let (header, n, prev, depth, delta_bytes) = if kind == 3 {
            if data.len() < 16 || &data[..8] != BASE || get32(&data, 12) != 0 {
                return Err(Error::Corrupt("packed base header".into()));
            }
            (16, get32(&data, 8) as usize, 0, 0, 0)
        } else if kind == 4 {
            if data.len() < 32 || &data[..8] != DELTA || get32(&data, 28) != 0 {
                return Err(Error::Corrupt("packed delta header".into()));
            }
            let (prev, depth, total) =
                (get64(&data, 8), get32(&data, 16), get32(&data, 24) as usize);
            if prev < 8
                || prev >= at
                || depth == 0
                || depth > MAX_DEPTH
                || total > DELTA_BYTES
                || total < data.len()
            {
                return Err(Error::Corrupt("packed delta bound/link".into()));
            }
            (32, get32(&data, 20) as usize, prev, depth, total)
        } else {
            return Err(Error::Corrupt("packed kind".into()));
        };
        if data.len() > PAGE_BYTES || n > (data.len() - header) / 24 || generation == 0 {
            return Err(Error::Corrupt("packed page size/count/generation".into()));
        }
        let mut offsets = Vec::with_capacity(n);
        let mut pos = header;
        let mut previous: Option<&[u8]> = None;
        for _ in 0..n {
            if data.len() - pos < 24 {
                return Err(Error::Corrupt("packed row header".into()));
            }
            let b = &data[pos..];
            let (kl, len, revision, overflow) =
                (get32(b, 0) as usize, get32(b, 4), get64(b, 8), get64(b, 16));
            if kl > MAX_KEY || kl > b.len() - 24 || revision == 0 || revision > generation {
                return Err(Error::Corrupt("packed key/revision".into()));
            }
            let key = &b[24..24 + kl];
            if previous.is_some_and(|p| p >= key) {
                return Err(Error::Corrupt("packed key order".into()));
            }
            previous = Some(key);
            let inline = if len == u32::MAX {
                if kind != 4 || overflow != 0 {
                    return Err(Error::Corrupt("packed tombstone".into()));
                }
                0
            } else if len as usize > MAX_VALUE {
                return Err(Error::Corrupt("packed value limit".into()));
            } else if overflow == 0 {
                if len as usize > INLINE_BYTES {
                    return Err(Error::Corrupt("packed inline length".into()));
                }
                len as usize
            } else {
                if overflow < 8 || overflow >= at || len as usize <= INLINE_BYTES {
                    return Err(Error::Corrupt("packed overflow reference".into()));
                }
                0
            };
            if inline > b.len() - 24 - kl {
                return Err(Error::Corrupt("packed inline bounds".into()));
            }
            offsets.push(pos as u32);
            pos += 24 + kl + inline;
        }
        if pos != data.len() {
            return Err(Error::Corrupt("packed trailing bytes".into()));
        }
        Ok(Self {
            data,
            offsets,
            generation,
            kind,
            prev,
            depth,
            delta_bytes,
        })
    }
    pub fn charge(&self) -> usize {
        self.data.capacity() + self.offsets.capacity() * 4 + 256
    }
    fn key(&self, i: usize) -> &[u8] {
        let b = &self.data[self.offsets[i] as usize..];
        &b[24..24 + get32(b, 0) as usize]
    }
    fn row_at(&self, i: usize) -> Row {
        let b = &self.data[self.offsets[i] as usize..];
        let kl = get32(b, 0) as usize;
        let len = get32(b, 4);
        let at = get64(b, 16);
        let value = if len == u32::MAX {
            Value::Deleted
        } else if at == 0 {
            Value::Inline(b[24 + kl..24 + kl + len as usize].to_vec())
        } else {
            Value::Overflow {
                at,
                len: len as usize,
            }
        };
        Row {
            key: self.key(i).to_vec(),
            revision: get64(b, 8),
            value,
        }
    }
    fn lower_bound(&self, key: &[u8]) -> usize {
        self.offsets.partition_point(|off| {
            let b = &self.data[*off as usize..];
            &b[24..24 + get32(b, 0) as usize] < key
        })
    }
    fn find(&self, key: &[u8]) -> Option<Row> {
        let i = self.lower_bound(key);
        (i < self.offsets.len() && self.key(i) == key).then(|| self.row_at(i))
    }
    fn row_count(&self) -> usize {
        self.offsets.len()
    }
    fn rows(&self) -> Vec<Row> {
        (0..self.offsets.len()).map(|i| self.row_at(i)).collect()
    }
}
struct Materialized {
    rows: Vec<Row>,
    depth: u32,
    delta_bytes: usize,
}
fn load(view: &Snapshot, at: u64, admit: bool) -> Result<Materialized> {
    load_record(
        view,
        view.epoch.packed_record(at, view.root.end, admit)?,
        admit,
    )
}
// Accept the already checked head so direct scans do not issue a second read
// before taking the exact delta-materialization fallback.
fn load_record(view: &Snapshot, mut record: Arc<Record>, admit: bool) -> Result<Materialized> {
    let mut deltas = Vec::new();
    let mut expected = None;
    let mut head_depth = 0;
    let mut head_bytes = 0;
    let base;
    loop {
        if record.generation > view.root.generation
            || expected.is_some_and(|(d, n, g)| {
                record.depth != d || record.delta_bytes != n || record.generation > g
            })
        {
            return Err(Error::Corrupt("packed lineage".into()));
        }
        if record.kind == 3 {
            base = record.rows();
            break;
        }
        if deltas.is_empty() {
            head_depth = record.depth;
            head_bytes = record.delta_bytes;
        }
        expected = Some((
            record.depth - 1,
            record.delta_bytes - record.data.len(),
            record.generation,
        ));
        let at = record.prev;
        deltas.push(record.rows());
        record = view.epoch.packed_record(at, view.root.end, admit)?;
    }
    let rows = if deltas.is_empty() {
        base
    } else {
        let mut rows: BTreeMap<_, _> = base.into_iter().map(|r| (r.key.clone(), r)).collect();
        for delta in deltas.into_iter().rev() {
            for row in delta {
                if matches!(row.value, Value::Deleted) {
                    rows.remove(&row.key);
                } else {
                    rows.insert(row.key.clone(), row);
                }
            }
        }
        rows.into_values().collect()
    };
    if 16 + rows.iter().map(Row::bytes).sum::<usize>() > PAGE_BYTES {
        return Err(Error::Corrupt("packed materialized size".into()));
    }
    Ok(Materialized {
        rows,
        depth: head_depth,
        delta_bytes: head_bytes,
    })
}
fn floor(view: &Snapshot, key: &[u8]) -> Result<Option<Arc<Node>>> {
    let mut at = view.root.root;
    let mut found = None;
    while at != 0 {
        let n = view.epoch.node(at, view.root.end)?;
        if n.key.as_slice() <= key {
            at = n.right;
            found = Some(n);
        } else {
            at = n.left;
        }
    }
    Ok(found)
}
fn successor(view: &Snapshot, key: &[u8]) -> Result<Option<Arc<Node>>> {
    let mut at = view.root.root;
    let mut found = None;
    while at != 0 {
        let n = view.epoch.node(at, view.root.end)?;
        if n.key.as_slice() > key {
            at = n.left;
            found = Some(n);
        } else {
            at = n.right;
        }
    }
    Ok(found)
}
pub(super) fn find(view: &Snapshot, key: &[u8]) -> Result<Option<Row>> {
    if key.len() > MAX_KEY {
        return Err(Error::Budget("key exceeds 4096 bytes".into()));
    }
    let Some(fence) = floor(view, key)? else {
        return Ok(None);
    };
    let mut at = fence.value;
    let mut expected = None;
    loop {
        let record = view.epoch.packed_record(at, view.root.end, true)?;
        if record.generation > view.root.generation
            || expected.is_some_and(|(d, n, g)| {
                record.depth != d || record.delta_bytes != n || record.generation > g
            })
        {
            return Err(Error::Corrupt("packed point lineage".into()));
        }
        if let Some(row) = record.find(key) {
            return Ok(if matches!(row.value, Value::Deleted) {
                None
            } else {
                Some(row)
            });
        }
        if record.kind == 3 {
            return Ok(None);
        }
        expected = Some((
            record.depth - 1,
            record.delta_bytes - record.data.len(),
            record.generation,
        ));
        at = record.prev;
    }
}

pub(crate) struct Cursor {
    view: Snapshot,
    fences: super::Cursor,
    page_record: Option<Arc<Record>>,
    page_index: usize,
    page_rows: std::vec::IntoIter<Row>,
    direct: bool,
    start: Vec<u8>,
    end: Option<Vec<u8>>,
    failed: bool,
}
impl Cursor {
    pub fn new(view: &Snapshot, start: &[u8], end: Option<&[u8]>) -> Result<Self> {
        Self::with_mode(view, start, end, !cfg!(feature = "spi-scan-materialized"))
    }
    fn with_mode(view: &Snapshot, start: &[u8], end: Option<&[u8]>, direct: bool) -> Result<Self> {
        let from = floor(view, start)?
            .map(|n| n.key.clone())
            .unwrap_or_default();
        Ok(Self {
            view: view.clone(),
            fences: view.cursor(&from, end)?,
            page_record: None,
            page_index: 0,
            page_rows: Vec::new().into_iter(),
            direct,
            start: start.to_vec(),
            end: end.map(Vec::from),
            failed: false,
        })
    }
}
impl Iterator for Cursor {
    type Item = Result<Row>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        loop {
            if let Some(record) = self.page_record.as_ref() {
                if self.page_index < record.row_count() {
                    // The start offset was selected before allocating any rows.
                    // Check the upper bound on borrowed bytes for the same reason.
                    if self
                        .end
                        .as_ref()
                        .is_some_and(|end| record.key(self.page_index) >= end.as_slice())
                    {
                        self.failed = true;
                        self.page_record = None;
                        return None;
                    }
                    let row = record.row_at(self.page_index);
                    self.page_index += 1;
                    return Some(Ok(row));
                }
                self.page_record = None;
            }
            if let Some(row) = self.page_rows.next() {
                if row.key < self.start {
                    continue;
                }
                if self.end.as_ref().is_some_and(|end| row.key >= *end) {
                    self.failed = true;
                    return None;
                }
                return Some(Ok(row));
            }
            match self.fences.next()? {
                Err(e) => {
                    self.failed = true;
                    return Some(Err(e));
                }
                Ok(n) => {
                    let next = (|| -> Result<()> {
                        if !self.direct {
                            self.page_rows = load(&self.view, n.value, false)?.rows.into_iter();
                            return Ok(());
                        }
                        let record =
                            self.view
                                .epoch
                                .packed_record(n.value, self.view.root.end, false)?;
                        if record.generation > self.view.root.generation {
                            return Err(Error::Corrupt("packed scan generation".into()));
                        }
                        if record.kind == 3 {
                            self.page_index = record.lower_bound(&self.start);
                            self.page_record = Some(record);
                        } else {
                            self.page_rows =
                                load_record(&self.view, record, false)?.rows.into_iter();
                        }
                        Ok(())
                    })();
                    if let Err(e) = next {
                        self.failed = true;
                        return Some(Err(e));
                    }
                }
            }
        }
    }
}
pub(super) fn scan(view: &Snapshot, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Entry>> {
    scan_with_mode(view, start, end, !cfg!(feature = "spi-scan-materialized"))
}
fn scan_with_mode(
    view: &Snapshot,
    start: &[u8],
    end: Option<&[u8]>,
    direct: bool,
) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    let mut bytes = 0;
    for row in Cursor::with_mode(view, start, end, direct)? {
        let row = row?;
        let value = if !direct {
            row.value(view)?
        } else {
            match row.value {
                Value::Inline(value) => value,
                Value::Overflow { at, len } => {
                    let value = view.epoch.value(at, view.root.end)?;
                    if value.len() != len {
                        return Err(Error::Corrupt("packed overflow length".into()));
                    }
                    value
                }
                Value::Deleted => return Err(Error::Corrupt("visible tombstone".into())),
            }
        };
        bytes += row.key.len() + value.len() + 64;
        if bytes > view.scan_budget {
            return Err(Error::Budget(
                "scan output exceeds configured budget".into(),
            ));
        }
        out.push(Entry {
            key: row.key,
            value,
        });
    }
    Ok(out)
}
fn save_base(w: &mut ArenaWriter, rows: &[Row]) -> Result<u64> {
    let mut bytes = Vec::with_capacity(16 + rows.iter().map(Row::bytes).sum::<usize>());
    bytes.extend_from_slice(BASE);
    bytes.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    encode_rows(rows, &mut bytes);
    if bytes.len() > PAGE_BYTES {
        return Err(Error::Budget("packed page overflow".into()));
    }
    w.save_packed(3, &bytes)
}
fn changed_row(w: &mut ArenaWriter, key: &[u8], value: &Option<Vec<u8>>) -> Result<Row> {
    let value = match value {
        None => Value::Deleted,
        Some(v) if v.len() <= INLINE_BYTES => Value::Inline(v.clone()),
        Some(v) => Value::Overflow {
            at: w.record(2, v)?,
            len: v.len(),
        },
    };
    Ok(Row {
        key: key.to_vec(),
        revision: w.generation,
        value,
    })
}
pub(super) fn apply(
    w: &mut ArenaWriter,
    view: &Snapshot,
    writes: &BTreeMap<Vec<u8>, Option<Vec<u8>>>,
) -> Result<(u64, u64)> {
    let mut root = view.root.root;
    let mut count = view.root.count;
    let mut writes = writes.iter().peekable();
    while let Some((first, _)) = writes.peek() {
        let fence = floor(view, first)?;
        if view.root.root != 0 && fence.is_none() {
            return Err(Error::Corrupt("missing packed sentinel".into()));
        }
        let fence_key = fence.as_ref().map(|n| n.key.clone()).unwrap_or_default();
        let upper = successor(view, &fence_key)?.map(|n| n.key.clone());
        let old = match &fence {
            Some(n) => load(view, n.value, false)?,
            None => Materialized {
                rows: vec![],
                depth: 0,
                delta_bytes: 0,
            },
        };
        let mut old_rows = old.rows.into_iter().peekable();
        let mut page = Vec::new();
        let mut page_bytes = 16;
        let mut changes = Vec::new();
        let mut changes_bytes = 32;
        let mut candidate = fence.is_some() && old.depth < MAX_DEPTH;
        let mut page_fence = fence_key.clone();
        let mut emitted = false;
        let mut changed = false;
        loop {
            let pending = writes
                .peek()
                .filter(|(k, _)| upper.as_ref().is_none_or(|end| *k < end));
            let row = match (old_rows.peek(), pending) {
                (None, None) => break,
                (Some(old), Some((key, _))) if old.key.as_slice() < key.as_slice() => {
                    old_rows.next().unwrap()
                }
                (Some(_), None) => old_rows.next().unwrap(),
                (_, Some(_)) => {
                    let (key, value) = writes.next().unwrap();
                    let present = old_rows.peek().is_some_and(|r| r.key == *key);
                    if present {
                        old_rows.next();
                    }
                    if value.is_none() && !present {
                        continue;
                    }
                    changed = true;
                    let row = changed_row(w, key, value)?;
                    changes_bytes += row.bytes();
                    if candidate && changes_bytes + old.delta_bytes <= DELTA_BYTES {
                        changes.push(row.clone());
                    } else {
                        candidate = false;
                        changes.clear();
                    }
                    if value.is_none() {
                        count -= 1;
                        continue;
                    }
                    if !present {
                        count += 1;
                    }
                    row
                }
            };
            if page_bytes + row.bytes() > PAGE_BYTES {
                if page.is_empty() {
                    return Err(Error::Budget("packed row too large".into()));
                }
                let at = save_base(w, &page)?;
                root = w.insert(root, &page_fence, at, w.generation)?;
                emitted = true;
                candidate = false;
                changes.clear();
                page.clear();
                page_bytes = 16;
                page_fence = row.key.clone();
            }
            page_bytes += row.bytes();
            page.push(row);
        }
        if !changed {
            continue;
        }
        if candidate && !changes.is_empty() && !emitted {
            let mut payload = Vec::with_capacity(changes_bytes);
            payload.extend_from_slice(DELTA);
            payload.extend_from_slice(&fence.as_ref().unwrap().value.to_le_bytes());
            payload.extend_from_slice(&(old.depth + 1).to_le_bytes());
            payload.extend_from_slice(&(changes.len() as u32).to_le_bytes());
            payload.extend_from_slice(&((old.delta_bytes + changes_bytes) as u32).to_le_bytes());
            payload.extend_from_slice(&0u32.to_le_bytes());
            encode_rows(&changes, &mut payload);
            let at = w.save_packed(4, &payload)?;
            root = w.insert(root, &fence_key, at, w.generation)?;
        } else {
            let at = save_base(w, &page)?;
            root = w.insert(root, &page_fence, at, w.generation)?;
        }
    }
    if count == 0 {
        root = 0;
    }
    Ok((root, count))
}
pub(super) fn copy_page(w: &mut ArenaWriter, view: &Snapshot, at: u64) -> Result<u64> {
    let mut rows = load(view, at, false)?.rows;
    for row in &mut rows {
        if let Value::Overflow { at, len } = row.value {
            let bytes = view.epoch.value(at, view.root.end)?;
            if bytes.len() != len {
                return Err(Error::Corrupt("packed overflow copy length".into()));
            }
            row.value = Value::Overflow {
                at: w.record(2, &bytes)?,
                len,
            };
        }
    }
    save_base(w, &rows)
}
pub(super) fn verify(view: &Snapshot) -> Result<u64> {
    let mut fences = view.cursor(&[], None)?.peekable();
    let mut count = 0;
    let mut first = true;
    while let Some(n) = fences.next() {
        let n = n?;
        if first && !n.key.is_empty() {
            return Err(Error::Corrupt("packed sentinel".into()));
        }
        first = false;
        let upper = match fences.peek() {
            Some(Ok(n)) => Some(n.key.as_slice()),
            _ => None,
        };
        for row in load(view, n.value, false)?.rows {
            if row.key < n.key || upper.is_some_and(|end| row.key.as_slice() >= end) {
                return Err(Error::Corrupt("packed fence coverage".into()));
            }
            row.value(view)?;
            count += 1;
        }
    }
    if count != view.root.count {
        return Err(Error::Corrupt("packed key count".into()));
    }
    Ok(count)
}

#[cfg(test)]
mod codec_tests {
    use super::*;
    fn base(rows: &[Row]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(BASE);
        bytes.extend_from_slice(&(rows.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        encode_rows(rows, &mut bytes);
        bytes
    }
    fn fixture() -> Vec<Row> {
        vec![
            Row {
                key: vec![],
                revision: 1,
                value: Value::Inline(vec![]),
            },
            Row {
                key: vec![0, 255],
                revision: 2,
                value: Value::Inline(vec![13; 1024]),
            },
            Row {
                key: vec![255; 4096],
                revision: 3,
                value: Value::Overflow { at: 8, len: 1025 },
            },
        ]
    }
    #[test]
    fn packed_codec_rejects_truncation_and_malformed_valid_crc_payloads() {
        let bytes = base(&fixture());
        let valid = Record::parse(3, bytes.clone(), 10000, 3).unwrap();
        assert_eq!(valid.rows().len(), 3);
        assert_eq!(valid.find(&[]).unwrap().revision, 1);
        assert!(valid.find(&[1]).is_none());
        for n in 0..bytes.len() {
            assert!(
                Record::parse(3, bytes[..n].to_vec(), 10000, 3).is_err(),
                "truncation {n}"
            );
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(Record::parse(3, trailing, 10000, 3).is_err());
        for (at, value) in [(8, u32::MAX), (12, 1), (16, 4097), (20, u32::MAX)] {
            let mut bad = bytes.clone();
            put32(&mut bad, at, value);
            assert!(Record::parse(3, bad, 10000, 3).is_err(), "u32 at {at}");
        }
        for (at, value) in [(24, 0), (24, 4), (32, 10000)] {
            let mut bad = bytes.clone();
            put64(&mut bad, at, value);
            assert!(Record::parse(3, bad, 10000, 3).is_err(), "u64 at {at}");
        }
        assert!(Record::parse(3, bytes.clone(), 10000, 0).is_err());
        assert!(Record::parse(2, bytes, 10000, 3).is_err());
        let same = base(&[
            Row {
                key: vec![1],
                revision: 1,
                value: Value::Inline(vec![]),
            },
            Row {
                key: vec![1],
                revision: 1,
                value: Value::Inline(vec![]),
            },
        ]);
        assert!(Record::parse(3, same, 10000, 1).is_err());
    }
    fn delta() -> Vec<u8> {
        let changes = vec![Row {
            key: b"a".to_vec(),
            revision: 2,
            value: Value::Deleted,
        }];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(DELTA);
        bytes.extend_from_slice(&8u64.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&57u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        encode_rows(&changes, &mut bytes);
        bytes
    }
    #[test]
    fn packed_codec_delta_limits_and_overflow_links_are_checked() {
        let bytes = delta();
        assert_eq!(bytes.len(), 57);
        assert!(matches!(
            Record::parse(4, bytes.clone(), 10000, 2)
                .unwrap()
                .find(b"a")
                .unwrap()
                .value,
            Value::Deleted
        ));
        for (at, value) in [
            (16, 0),
            (16, MAX_DEPTH + 1),
            (20, 2),
            (24, 56),
            (24, DELTA_BYTES as u32 + 1),
            (28, 1),
        ] {
            let mut bad = bytes.clone();
            put32(&mut bad, at, value);
            assert!(
                Record::parse(4, bad, 10000, 2).is_err(),
                "delta {at}={value}"
            );
        }
        for value in [0, 7, 10000, u64::MAX] {
            let mut bad = bytes.clone();
            put64(&mut bad, 8, value);
            assert!(Record::parse(4, bad, 10000, 2).is_err());
        }
        let inline_too_large = base(&[Row {
            key: vec![],
            revision: 1,
            value: Value::Inline(vec![0; 1025]),
        }]);
        assert!(Record::parse(3, inline_too_large, 10000, 1).is_err());
        for (at, len) in [(7, 1025), (10000, 1025), (8, 1024), (8, MAX_VALUE + 1)] {
            let bad = base(&[Row {
                key: vec![],
                revision: 1,
                value: Value::Overflow { at, len },
            }]);
            assert!(Record::parse(3, bad, 10000, 1).is_err());
        }
    }
}
// Private seams compare direct and materialized execution on identical snapshots.
#[cfg(test)]
mod direct_scan_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn database() -> Database {
        static ID: AtomicU64 = AtomicU64::new(0);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".working/tmp/direct-scan-tests");
        fs::create_dir_all(&root).unwrap();
        let path = root.join(format!(
            "{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        let db = Database::create(
            path,
            Options {
                packed_pages: true,
                cache_bytes: 0,
                ..Options::default()
            },
        )
        .unwrap();
        let mut tx = db.begin().unwrap();
        for i in 0..100u16 {
            tx.set(i.to_be_bytes(), vec![i as u8; 64]).unwrap();
        }
        tx.commit().unwrap();
        db
    }

    fn compare(view: &Snapshot, start: &[u8], end: Option<&[u8]>) {
        let before = view.epoch.reads.load(Ordering::Relaxed);
        let direct = scan_with_mode(view, start, end, true).unwrap();
        let after = view.epoch.reads.load(Ordering::Relaxed);
        let control = scan_with_mode(view, start, end, false).unwrap();
        let last = view.epoch.reads.load(Ordering::Relaxed);
        assert_eq!(direct, control);
        assert_eq!(
            after - before,
            last - after,
            "direct scan must not reread delta head"
        );
    }

    #[test]
    fn direct_base_ranges_seek_without_materializing_unreturned_rows() {
        let db = database();
        let view = db.snapshot().unwrap();
        let start = 90u16.to_be_bytes();
        let end = 92u16.to_be_bytes();
        let mut direct = Cursor::with_mode(&view, &start, Some(&end), true).unwrap();
        assert_eq!(direct.next().unwrap().unwrap().key, start);
        assert_eq!(direct.page_index, 91);
        assert_eq!(
            direct.page_rows.len(),
            0,
            "base page must remain serialized"
        );
        assert_eq!(direct.next().unwrap().unwrap().key, 91u16.to_be_bytes());
        assert!(direct.next().is_none());
        assert!(direct.next().is_none());
        compare(&view, &start, Some(&end));
        compare(&view, &start, Some(&start));
        compare(&view, &[], None);
        compare(&view, &[255], None);
        // Each returned row charges 2 key bytes + 64 value bytes + 64 metadata bytes.
        let limited = Snapshot {
            scan_budget: 260,
            ..view.clone()
        };
        assert_eq!(
            scan_with_mode(&limited, &start, Some(&end), true)
                .unwrap()
                .len(),
            2
        );
        let limited = Snapshot {
            scan_budget: 259,
            ..view
        };
        assert!(matches!(
            scan_with_mode(&limited, &start, Some(&end), true),
            Err(Error::Budget(_))
        ));
        assert!(matches!(
            scan_with_mode(&limited, &start, Some(&end), false),
            Err(Error::Budget(_))
        ));
    }

    #[test]
    fn direct_and_materialized_scans_reject_future_base_and_delta_generations() {
        let db = database();
        let mut old = db.snapshot().unwrap();
        old.root.generation = 0;
        for direct in [false, true] {
            assert!(matches!(
                scan_with_mode(&old, &[], None, direct),
                Err(Error::Corrupt(_))
            ));
        }
        let before = db.snapshot().unwrap();
        let mut tx = db.begin().unwrap();
        tx.set(50u16.to_be_bytes(), b"updated").unwrap();
        tx.commit().unwrap();
        let mut stale = db.snapshot().unwrap();
        stale.root.generation = before.root.generation;
        for direct in [false, true] {
            assert!(matches!(
                scan_with_mode(&stale, &[], None, direct),
                Err(Error::Corrupt(_))
            ));
        }
    }

    #[test]
    fn direct_delta_fallback_has_equal_reads_visibility_and_output_ownership() {
        let db = database();
        let pinned = db.snapshot().unwrap();
        for generation in 0..12u16 {
            let mut tx = db.begin().unwrap();
            tx.set(50u16.to_be_bytes(), generation.to_le_bytes())
                .unwrap();
            if generation % 2 == 0 {
                tx.delete(20u16.to_be_bytes()).unwrap();
            } else {
                tx.set(20u16.to_be_bytes(), b"reinserted").unwrap();
            }
            tx.commit().unwrap();
            let view = db.snapshot().unwrap();
            compare(&view, &[], None);
            compare(&view, &19u16.to_be_bytes(), Some(&52u16.to_be_bytes()));
            compare(&pinned, &[], None);
        }
        // Overflow rows remain output-owned and use the existing checked read.
        let mut tx = db.begin().unwrap();
        tx.set(b"overflow", vec![7; 4096]).unwrap();
        tx.commit().unwrap();
        let view = db.snapshot().unwrap();
        compare(&view, b"overflow", None);
        let mut found = scan_with_mode(&view, b"overflow", None, true).unwrap();
        found[0].value[0] = 99;
        assert_eq!(view.get(b"overflow").unwrap(), Some(vec![7; 4096]));
        db.compact().unwrap();
        compare(&db.snapshot().unwrap(), &[], None);
        compare(&pinned, &[], None);
        assert_eq!(db.stats().unwrap().cache_page_entries, 0);
        assert_eq!(db.stats().unwrap().retired_cache_page_capacity, 0);
    }
}
