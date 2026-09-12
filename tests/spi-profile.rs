//! Diagnostic counters must describe actual committed I/O without changing visibility.
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use udb_lab::spi::{profile, Database, Options};

#[test]
fn diagnostic_counts_cover_io_cache_and_commit_without_changing_values() {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".working/tmp/profile-tests");
    fs::create_dir_all(&root).unwrap();
    let path = root.join(format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let db = Database::create(
        &path,
        Options {
            cache_bytes: 8192,
            value_cache: true,
            ..Options::default()
        },
    )
    .unwrap();
    let before = profile::snapshot();
    let mut tx = db.begin().unwrap();
    tx.set(b"k", b"original").unwrap();
    tx.commit().unwrap();
    let committed = profile::snapshot();
    if cfg!(feature = "spi-profile") {
        let difference =
            |name: &str| committed[name].as_u64().unwrap() - before[name].as_u64().unwrap();
        assert_eq!(difference("node_saves"), 1);
        assert_eq!(difference("value_records"), 1);
        // Value record: 32+8, node: 32+40+1, manifest: 64.
        assert_eq!(difference("write_bytes"), 40 + 73 + 64);
        assert_eq!(difference("write_calls"), 3); // Two halves of append and one manifest.
        assert_eq!(difference("checksum_calls"), 3);
        assert_eq!(difference("checksum_bytes"), 40 + 73 + 64);
    } else {
        assert!(before.is_null() && committed.is_null());
    }
    db.clear_cache();
    let before = profile::snapshot();
    assert_eq!(db.get(b"k").unwrap(), Some(b"original".to_vec()));
    assert_eq!(db.get(b"k").unwrap(), Some(b"original".to_vec()));
    let after = profile::snapshot();
    if cfg!(feature = "spi-profile") {
        let difference =
            |name: &str| after[name].as_u64().unwrap() - before[name].as_u64().unwrap();
        assert_eq!(difference("read_calls"), 4); // Header+payload of node and value.
        assert_eq!(difference("read_bytes"), 113);
        assert_eq!(difference("checksum_calls"), 2);
        assert_eq!(difference("node_cache_misses"), 1);
        assert_eq!(difference("node_cache_hits"), 1);
        assert_eq!(difference("value_cache_misses"), 1);
        assert_eq!(difference("value_cache_hits"), 1);
    }
    let pinned = db.snapshot().unwrap();
    let mut tx = db.begin().unwrap();
    tx.set(b"k", b"changed").unwrap();
    tx.commit().unwrap();
    db.compact().unwrap();
    assert_eq!(pinned.get(b"k").unwrap(), Some(b"original".to_vec()));
    assert_eq!(db.get(b"k").unwrap(), Some(b"changed".to_vec()));
    assert_eq!(db.verify().unwrap(), 1);
    drop(pinned);
    db.collect().unwrap();
    drop(db);
    let db = Database::open(path, Options::default()).unwrap();
    assert_eq!(db.get(b"k").unwrap(), Some(b"changed".to_vec()));
}
