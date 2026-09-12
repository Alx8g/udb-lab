use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
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
fn compaction_blocks_commit_but_preserves_snapshot_and_serializability() {
    let p = path("blocking");
    let db = Database::create(
        &p,
        Options {
            cache_bytes: 8 * 1024 * 1024,
            ..Options::default()
        },
    )
    .unwrap();
    seed(&db, 6000);
    let pinned = db.snapshot().unwrap();
    let before = pinned.get(&0u64.to_be_bytes()).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let c = db.clone();
    let b = barrier.clone();
    let handle = std::thread::spawn(move || {
        b.wait();
        let start = Instant::now();
        c.compact().unwrap();
        start.elapsed()
    });
    barrier.wait();
    std::thread::sleep(Duration::from_millis(1));
    let mut attempts = 0;
    let started = Instant::now();
    loop {
        attempts += 1;
        let mut t = db.begin().unwrap();
        t.set(b"during", b"write").unwrap();
        match t.commit() {
            Ok(_) => break,
            Err(Error::Conflict) => continue,
            Err(e) => panic!("{e}"),
        }
    }
    let commit_elapsed = started.elapsed();
    let compact_elapsed = handle.join().unwrap();
    assert_eq!(pinned.get(&0u64.to_be_bytes()).unwrap(), before);
    assert_eq!(db.get(b"during").unwrap(), Some(b"write".to_vec()));
    db.verify().unwrap();
    assert!(attempts >= 1);
    println!(
        "compaction_ms={:.3} commit_ms={:.3} attempts={attempts}",
        compact_elapsed.as_secs_f64() * 1000.0,
        commit_elapsed.as_secs_f64() * 1000.0
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
