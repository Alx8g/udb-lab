use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{SystemTime, UNIX_EPOCH};
use udb_lab::spi::{Database, Entry, Error, Options};

fn path(name: &str) -> PathBuf {
    static ID: AtomicU64 = AtomicU64::new(0);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".working/tmp/spi-tests");
    fs::create_dir_all(&root).unwrap();
    root.join(format!(
        "{name}-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        ID.fetch_add(1, Ordering::Relaxed)
    ))
}
fn put(db: &Database, key: &[u8], value: &[u8]) {
    let mut t = db.begin().unwrap();
    t.set(key, value).unwrap();
    t.commit().unwrap();
}
fn rows(m: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<Entry> {
    m.iter()
        .map(|(key, value)| Entry {
            key: key.clone(),
            value: value.clone(),
        })
        .collect()
}
fn rng(s: &mut u64) -> u64 {
    *s = s
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *s
}

#[test]
fn arbitrary_binary_values_snapshots_compaction_and_reopen() {
    let p = path("binary");
    let db = Database::create(&p, Options::default()).unwrap();
    let mut t = db.begin().unwrap();
    t.set([], []).unwrap();
    t.set([0, 255, 0], vec![37; 17000]).unwrap();
    t.set([255, 255], b"end").unwrap();
    t.commit().unwrap();
    let old = db.snapshot().unwrap();
    let another = old.clone();
    put(&db, &[0, 255, 0], b"new");
    db.compact().unwrap();
    assert_eq!(db.collect().unwrap(), 0);
    assert_eq!(old.get(&[0, 255, 0]).unwrap(), Some(vec![37; 17000]));
    drop(old);
    assert_eq!(db.collect().unwrap(), 0);
    drop(another);
    assert_eq!(db.collect().unwrap(), 1);
    assert_eq!(db.scan_prefix(&[255]).unwrap().len(), 1);
    assert_eq!(db.get(&[]).unwrap(), Some(vec![]));
    assert_eq!(db.verify().unwrap(), 3);
    drop(db);
    let db = Database::open(&p, Options::default()).unwrap();
    assert_eq!(db.get(&[0, 255, 0]).unwrap(), Some(b"new".to_vec()));
    assert_eq!(db.verify().unwrap(), 3);
}
#[test]
fn randomized_avl_matches_full_oracle_and_pinned_versions() {
    for seed in 1..=3 {
        let p = path("oracle");
        let db = Database::create(
            &p,
            Options {
                cache_bytes: 1024,
                ..Options::default()
            },
        )
        .unwrap();
        let mut state = seed;
        let mut oracle = BTreeMap::new();
        let mut pins = Vec::new();
        for batch in 0..35 {
            if batch % 9 == 0 {
                pins.push((db.snapshot().unwrap(), rows(&oracle)));
            }
            let mut tx = db.begin().unwrap();
            for _ in 0..24 {
                let k = ((rng(&mut state) >> 32) % 150).to_be_bytes().to_vec();
                if rng(&mut state) % 5 == 0 {
                    tx.delete(k.clone()).unwrap();
                    oracle.remove(&k);
                } else {
                    let v = rng(&mut state).to_le_bytes().to_vec();
                    tx.set(k.clone(), v.clone()).unwrap();
                    oracle.insert(k, v);
                }
            }
            assert_eq!(tx.scan_prefix(&[]).unwrap(), rows(&oracle));
            tx.commit().unwrap();
            assert_eq!(db.scan_prefix(&[]).unwrap(), rows(&oracle));
            assert_eq!(db.verify().unwrap(), oracle.len() as u64);
            if batch % 11 == 0 {
                db.compact().unwrap();
                db.collect().unwrap();
            }
            for (snap, expected) in &pins {
                assert_eq!(&snap.scan_prefix(&[]).unwrap(), expected);
            }
        }
        assert!(db.stats().unwrap().cache_bytes <= 1024);
        drop(pins);
        db.collect().unwrap();
        drop(db);
        let db = Database::open(&p, Options::default()).unwrap();
        assert_eq!(db.scan_prefix(&[]).unwrap(), rows(&oracle));
        db.verify().unwrap();
    }
}
#[test]
fn serializable_lost_update_write_skew_and_phantoms() {
    let p = path("conflicts");
    let db = Database::create(&p, Options::default()).unwrap();
    put(&db, b"a", b"1");
    put(&db, b"b", b"1");
    let mut a = db.begin().unwrap();
    let mut b = db.begin().unwrap();
    assert_eq!(a.get(b"b").unwrap(), Some(b"1".to_vec()));
    assert_eq!(b.get(b"a").unwrap(), Some(b"1".to_vec()));
    a.set(b"a", b"0").unwrap();
    b.set(b"b", b"0").unwrap();
    a.commit().unwrap();
    assert!(matches!(b.commit(), Err(Error::Conflict)));
    let mut a = db.begin().unwrap();
    let mut b = db.begin().unwrap();
    a.set(b"a", b"2").unwrap();
    b.set(b"a", b"3").unwrap();
    a.commit().unwrap();
    assert!(matches!(b.commit(), Err(Error::Conflict)));
    let mut range = db.begin().unwrap();
    assert!(range.scan_prefix(b"tenant/").unwrap().is_empty());
    put(&db, b"tenant/new", b"yes");
    range.set(b"result", b"empty").unwrap();
    assert!(matches!(range.commit(), Err(Error::Conflict)));
    let mut a = db.begin().unwrap();
    let mut b = db.begin().unwrap();
    a.set(b"disjoint/1", b"x").unwrap();
    b.set(b"disjoint/2", b"y").unwrap();
    a.commit().unwrap();
    b.commit().unwrap();
    let mut range = db.begin().unwrap();
    range.scan_prefix(b"tenant/").unwrap();
    put(&db, b"outside", b"ok");
    range.set(b"derived", b"ok").unwrap();
    range.commit().unwrap();
}
#[test]
fn range_read_own_writes_and_budget_rejections_are_atomic() {
    let p = path("budgets");
    let db = Database::create(
        &p,
        Options {
            transaction_bytes: 4096,
            scan_bytes: 256,
            cache_bytes: 0,
            ..Options::default()
        },
    )
    .unwrap();
    put(&db, b"a", b"old");
    let mut t = db.begin().unwrap();
    t.delete(b"a").unwrap();
    t.set(b"b", []).unwrap();
    assert_eq!(
        t.scan(b"a", Some(b"c")).unwrap(),
        vec![Entry {
            key: b"b".to_vec(),
            value: vec![]
        }]
    );
    let before = t.staged_bytes();
    assert!(matches!(
        t.set(b"bad", vec![0; 5000]),
        Err(Error::Budget(_))
    ));
    assert_eq!(t.staged_bytes(), before);
    assert_eq!(t.get(b"bad").unwrap(), None);
    t.commit().unwrap();
    let mut t = db.begin().unwrap();
    t.set(b"large", vec![0; 300]).unwrap();
    assert!(matches!(t.scan_prefix(&[]), Err(Error::Budget(_))));
    drop(t);
    let mut t = db.begin().unwrap();
    for _ in 0..20 {
        t.set(b"same", vec![1; 100]).unwrap();
    }
    assert!(t.staged_bytes() < 4096);
    t.commit().unwrap();
    assert!(matches!(
        db.snapshot().unwrap().scan(b"z", Some(b"a")),
        Err(Error::Budget(_))
    ));
}
#[test]
fn concurrent_atomic_transfers_and_lock_lifetime() {
    let p = path("threads");
    let db = Database::create(&p, Options::default()).unwrap();
    let mut t = db.begin().unwrap();
    t.set(b"a", 1000u64.to_le_bytes()).unwrap();
    t.set(b"b", 1000u64.to_le_bytes()).unwrap();
    t.commit().unwrap();
    assert!(matches!(
        Database::open(&p, Options::default()),
        Err(Error::Locked)
    ));
    let barrier = Arc::new(Barrier::new(4));
    let mut handles = Vec::new();
    for _ in 0..4 {
        let db = db.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for _ in 0..15 {
                loop {
                    let mut t = db.begin().unwrap();
                    let a = u64::from_le_bytes(t.get(b"a").unwrap().unwrap().try_into().unwrap());
                    let b = u64::from_le_bytes(t.get(b"b").unwrap().unwrap().try_into().unwrap());
                    assert_eq!(a + b, 2000);
                    t.set(b"a", (a - 1).to_le_bytes()).unwrap();
                    t.set(b"b", (b + 1).to_le_bytes()).unwrap();
                    match t.commit() {
                        Ok(_) => break,
                        Err(Error::Conflict) => continue,
                        Err(e) => panic!("{e}"),
                    }
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(db.get(b"a").unwrap(), Some(940u64.to_le_bytes().to_vec()));
    let snap = db.snapshot().unwrap();
    drop(db);
    assert!(matches!(
        Database::open(&p, Options::default()),
        Err(Error::Locked)
    ));
    drop(snap);
    Database::open(&p, Options::default())
        .unwrap()
        .verify()
        .unwrap();
}
#[test]
fn failed_append_recovery_ignores_tail_and_never_publishes_hybrid() {
    for at in 1..=25 {
        let p = path("fault");
        let db = Database::create(&p, Options::default()).unwrap();
        let mut t = db.begin().unwrap();
        t.set(b"a", b"old").unwrap();
        t.set(b"b", b"old").unwrap();
        t.commit().unwrap();
        drop(db);
        let db = Database::open(
            &p,
            Options {
                fail_at: Some(at),
                write_chunk: Some(3),
                ..Options::default()
            },
        )
        .unwrap();
        let mut t = db.begin().unwrap();
        t.set(b"a", b"new").unwrap();
        t.set(b"b", b"new").unwrap();
        let result = t.commit();
        if result.is_err() {
            assert!(matches!(db.get(b"a"), Err(Error::Poisoned)));
        }
        drop(db);
        let db = Database::open(&p, Options::default()).unwrap();
        let a = db.get(b"a").unwrap().unwrap();
        assert!(a == b"old" || a == b"new");
        assert_eq!(db.get(b"b").unwrap(), Some(a));
        db.verify().unwrap();
        put(&db, b"tail-retry", b"ok");
        drop(db);
        let db = Database::open(&p, Options::default()).unwrap();
        assert_eq!(db.get(b"tail-retry").unwrap(), Some(b"ok".to_vec()));
    }
}
#[test]
fn corruption_is_detected_on_raw_bytes_not_reconstructed_fields() {
    let p = path("corrupt");
    let db = Database::create(&p, Options::default()).unwrap();
    put(&db, b"key", b"value");
    drop(db);
    let mut data = fs::read(p.join("arena-0.spi")).unwrap();
    data[8 + 10] ^= 0x55;
    fs::write(p.join("arena-0.spi"), &data).unwrap();
    let db = Database::open(&p, Options::default()).unwrap();
    assert!(matches!(db.get(b"key"), Err(Error::Corrupt(_))));
    assert!(db.verify().is_err());
    drop(db);
    let mut m = fs::read(p.join("manifest.spi")).unwrap();
    m[55] ^= 1;
    fs::write(p.join("manifest.spi"), m).unwrap();
    assert!(matches!(
        Database::open(&p, Options::default()),
        Err(Error::Corrupt(_))
    ));
}
#[test]
fn garbage_tail_and_empty_compaction_reopen() {
    let p = path("tail");
    let db = Database::create(&p, Options::default()).unwrap();
    put(&db, b"k", b"v");
    drop(db);
    let mut f = OpenOptions::new()
        .append(true)
        .open(p.join("arena-0.spi"))
        .unwrap();
    f.write_all(&vec![99; 2048]).unwrap();
    drop(f);
    let db = Database::open(&p, Options::default()).unwrap();
    put(&db, b"k", b"new");
    let mut t = db.begin().unwrap();
    t.delete(b"k").unwrap();
    t.commit().unwrap();
    db.compact().unwrap();
    db.collect().unwrap();
    drop(db);
    let db = Database::open(&p, Options::default()).unwrap();
    assert_eq!(db.verify().unwrap(), 0);
    assert_eq!(db.stats().unwrap().committed_bytes, 8);
}
#[test]
fn compaction_faults_preserve_pinned_data_and_retry_after_orphans() {
    for at in 1..=16 {
        let p = path("compact-fault");
        let db = Database::create(&p, Options::default()).unwrap();
        put(&db, b"key", b"value");
        drop(db);
        let db = Database::open(
            &p,
            Options {
                fail_at: Some(at),
                ..Options::default()
            },
        )
        .unwrap();
        let _ = db.compact();
        drop(db);
        let db = Database::open(&p, Options::default()).unwrap();
        assert_eq!(db.get(b"key").unwrap(), Some(b"value".to_vec()));
        db.compact().unwrap();
        db.collect().unwrap();
        db.verify().unwrap();
    }
}
#[test]
fn existing_destinations_are_never_overwritten() {
    let p = path("existing");
    fs::create_dir(&p).unwrap();
    fs::write(p.join("important"), b"preserve").unwrap();
    assert!(Database::create(&p, Options::default()).is_err());
    assert_eq!(fs::read(p.join("important")).unwrap(), b"preserve");
}
#[test]
fn truncation_is_rejected() {
    let p = path("truncate");
    let db = Database::create(&p, Options::default()).unwrap();
    put(&db, b"key", b"value");
    drop(db);
    let mut f = OpenOptions::new()
        .write(true)
        .open(p.join("arena-0.spi"))
        .unwrap();
    let len = f.seek(SeekFrom::End(0)).unwrap();
    f.set_len(len - 1).unwrap();
    drop(f);
    assert!(Database::open(&p, Options::default()).is_err());
}

#[test]
fn missing_key_aba_and_bounded_validation_history_abort_safely() {
    let p = path("aba");
    let db = Database::create(&p, Options::default()).unwrap();
    let mut reader = db.begin().unwrap();
    assert_eq!(reader.get(b"ephemeral").unwrap(), None);
    put(&db, b"ephemeral", b"x");
    let mut deletion = db.begin().unwrap();
    deletion.delete(b"ephemeral").unwrap();
    deletion.commit().unwrap();
    reader
        .set(b"dependent", b"incorrect-if-interleaved")
        .unwrap();
    assert!(matches!(reader.commit(), Err(Error::Conflict)));
    let p = path("journal-bound");
    let db = Database::create(
        &p,
        Options {
            conflict_bytes: 128,
            ..Options::default()
        },
    )
    .unwrap();
    let mut old = db.begin().unwrap();
    old.get(b"absent").unwrap();
    put(&db, b"other", b"1");
    put(&db, b"another", b"2");
    old.set(b"result", b"x").unwrap();
    assert!(matches!(old.commit(), Err(Error::Conflict)));
}
