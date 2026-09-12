use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{SystemTime, UNIX_EPOCH};
use udb_lab::spi::{Database, Error, Options};

fn path(name: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".working/tmp/spi-concurrency");
    fs::create_dir_all(&root).unwrap();
    root.join(format!(
        "{name}-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}
fn seed(db: &Database, n: u64) {
    let mut t = db.begin().unwrap();
    for i in 0..n {
        t.set(i.to_be_bytes(), i.to_le_bytes()).unwrap();
    }
    t.commit().unwrap();
}

#[test]
fn concurrent_compaction_never_loses_a_committed_write() {
    let p = path("concurrent-compaction");
    let db = Database::create(&p, Options::default()).unwrap();
    seed(&db, 2000);
    let pinned = db.snapshot().unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let worker_db = db.clone();
    let worker_barrier = barrier.clone();
    let worker = std::thread::spawn(move || {
        worker_barrier.wait();
        worker_db.compact()
    });
    barrier.wait();
    let mut tx = db.begin().unwrap();
    tx.set(b"during", b"committed").unwrap();
    tx.commit().unwrap();
    // Either ordering is legal here. Private checkpoint tests separately force
    // a write during preparation and require Conflict for the stale candidate.
    assert!(matches!(
        worker.join().unwrap(),
        Ok(()) | Err(Error::Conflict)
    ));
    assert_eq!(pinned.get(b"during").unwrap(), None);
    assert_eq!(db.get(b"during").unwrap(), Some(b"committed".to_vec()));
    assert_eq!(
        pinned.get(&0u64.to_be_bytes()).unwrap(),
        Some(0u64.to_le_bytes().to_vec())
    );
    drop(pinned);
    db.compact().unwrap();
    db.collect().unwrap();
    db.verify().unwrap();
    drop(db);
    let reopened = Database::open(&p, Options::default()).unwrap();
    assert_eq!(
        reopened.get(b"during").unwrap(),
        Some(b"committed".to_vec())
    );
}

#[test]
fn disjoint_transactions_progress_in_parallel_until_single_writer_commit() {
    let p = path("parallel");
    let db = Database::create(&p, Options::default()).unwrap();
    seed(&db, 128);
    let db = Arc::new(db);
    let barrier = Arc::new(Barrier::new(8));
    let commits = Arc::new(AtomicU64::new(0));
    let conflicts = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for tid in 0..8 {
        let d = db.clone();
        let b = barrier.clone();
        let c = commits.clone();
        let x = conflicts.clone();
        hs.push(std::thread::spawn(move || {
            b.wait();
            for j in 0..20 {
                loop {
                    let mut t = d.begin().unwrap();
                    let key = (128 + (tid * 20 + j) as u64).to_be_bytes();
                    t.set(key, [tid as u8, j as u8]).unwrap();
                    match t.commit() {
                        Ok(_) => {
                            c.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                        Err(Error::Conflict) => x.fetch_add(1, Ordering::Relaxed),
                        Err(e) => panic!("{e}"),
                    };
                }
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(commits.load(Ordering::Relaxed), 160);
    let rows = db.scan_prefix(&[]).unwrap();
    assert_eq!(rows.len(), 288);
    for tid in 0..8u64 {
        for j in 0..20u64 {
            let key = (128 + tid * 20 + j).to_be_bytes();
            assert_eq!(db.get(&key).unwrap(), Some(vec![tid as u8, j as u8]));
        }
    }
    db.verify().unwrap();
    println!(
        "commits={} conflicts={}",
        commits.load(Ordering::Relaxed),
        conflicts.load(Ordering::Relaxed)
    );
}
