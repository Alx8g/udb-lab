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

#[test]
fn buffered_and_direct_arenas_match_with_zero_cache_large_values_and_rotations() {
    let mut artifacts = Vec::new();
    for buffered in [false, true] {
        let p = path("buffer-control");
        let options = Options {
            cache_bytes: 0,
            append_buffer: buffered,
            ..Options::default()
        };
        let db = Database::create(&p, options.clone()).unwrap();
        let empty = db.snapshot().unwrap();
        let mut t = db.begin().unwrap();
        // Permuted keys exercise both double rotations and repeated flushes.
        for i in 0..600u64 {
            let k = (i * 331 % 601).to_be_bytes();
            let v = if i % 100 == 0 {
                vec![(i % 251) as u8; 70000]
            } else {
                i.to_le_bytes().to_vec()
            };
            t.set(k, v).unwrap();
        }
        t.commit().unwrap();
        let pinned = db.snapshot().unwrap();
        let before = pinned.scan_prefix(&[]).unwrap();
        let mut t = db.begin().unwrap();
        for i in (0..600u64).step_by(3) {
            t.delete((i * 331 % 601).to_be_bytes()).unwrap();
        }
        t.commit().unwrap();
        let after = db.scan_prefix(&[]).unwrap();
        assert_eq!(db.verify().unwrap(), 400);
        assert!(empty.scan_prefix(&[]).unwrap().is_empty());
        assert_eq!(pinned.scan_prefix(&[]).unwrap(), before);
        let stats = db.stats().unwrap();
        let arena = fs::read(p.join("arena-0.spi")).unwrap();
        assert_eq!(stats.cache_bytes, 0);
        assert_eq!(stats.bytes_written + 8, arena.len() as u64);
        db.compact().unwrap();
        assert_eq!(db.collect().unwrap(), 0);
        assert_eq!(pinned.scan_prefix(&[]).unwrap(), before);
        drop(pinned);
        drop(empty);
        db.collect().unwrap();
        assert_eq!(db.scan_prefix(&[]).unwrap(), after);
        db.verify().unwrap();
        drop(db);
        let db = Database::open(&p, options).unwrap();
        assert_eq!(db.scan_prefix(&[]).unwrap(), after);
        artifacts.push((arena, stats.arena_write_calls, stats.bytes_written));
    }
    assert_eq!(
        artifacts[0].0, artifacts[1].0,
        "buffering changed file bytes"
    );
    assert_eq!(artifacts[0].2, artifacts[1].2);
    assert!(
        artifacts[1].1 * 10 < artifacts[0].1,
        "buffering did not reduce write calls"
    );
}

#[test]
fn half_flushed_buffer_is_not_published_and_short_writes_remain_complete() {
    let p = path("half-flush");
    let db = Database::create(&p, Options::default()).unwrap();
    put(&db, b"k", b"old");
    let committed = db.stats().unwrap().committed_bytes;
    drop(db);
    // A replacement emits one value and one node. Event 5 is the half flush.
    let db = Database::open(
        &p,
        Options {
            fail_at: Some(5),
            write_chunk: Some(3),
            ..Options::default()
        },
    )
    .unwrap();
    let mut t = db.begin().unwrap();
    t.set(b"k", b"new").unwrap();
    assert!(matches!(t.commit(), Err(Error::Io(_))));
    assert!(fs::metadata(p.join("arena-0.spi")).unwrap().len() > committed);
    assert!(matches!(db.get(b"k"), Err(Error::Poisoned)));
    drop(db);
    let db = Database::open(
        &p,
        Options {
            write_chunk: Some(1),
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(db.get(b"k").unwrap(), Some(b"old".to_vec()));
    put(&db, b"k", b"recovered");
    db.verify().unwrap();
    drop(db);
    assert_eq!(
        Database::open(&p, Options::default())
            .unwrap()
            .get(b"k")
            .unwrap(),
        Some(b"recovered".to_vec())
    );
}

#[test]
fn buffered_publication_preserves_post_commit_node_cache() {
    for buffering in [false, true] {
        let db = Database::create(
            path("post-commit-cache"),
            Options {
                append_buffer: buffering,
                cache_bytes: 4 * 1024 * 1024,
                ..Options::default()
            },
        )
        .unwrap();
        let mut t = db.begin().unwrap();
        for i in 0..250u64 {
            t.set(i.to_be_bytes(), i.to_le_bytes()).unwrap();
        }
        t.commit().unwrap();
        let before = db.stats().unwrap();
        assert!(before.cache_entries >= 250);
        for i in 0..250u64 {
            assert_eq!(
                db.get(&i.to_be_bytes()).unwrap(),
                Some(i.to_le_bytes().to_vec())
            );
        }
        // Values are uncached in both controls. No additional index reads
        // should be necessary immediately after this small commit.
        assert_eq!(db.stats().unwrap().reads - before.reads, 250);
        assert!(db.stats().unwrap().cache_bytes <= 4 * 1024 * 1024);
    }
}

#[test]
fn retired_epoch_cache_releases_container_capacity() {
    let p = path("retired-cache");
    let db = Database::create(
        &p,
        Options {
            cache_bytes: 4 * 1024 * 1024,
            ..Options::default()
        },
    )
    .unwrap();
    let mut t = db.begin().unwrap();
    for i in 0..500u64 {
        t.set(i.to_be_bytes(), i.to_le_bytes()).unwrap();
    }
    t.commit().unwrap();
    for i in 0..500u64 {
        assert!(db.get(&i.to_be_bytes()).unwrap().is_some());
    }
    let old = db.snapshot().unwrap();
    let warm = db.stats().unwrap();
    assert!(warm.cache_map_capacity > 0 || warm.cache_order_capacity > 0);
    db.compact().unwrap();
    let retired = db.stats().unwrap();
    assert_eq!(retired.retired_cache_map_capacity, 0);
    assert_eq!(retired.retired_cache_order_capacity, 0);
    assert_eq!(retired.cache_map_capacity, 0);
    assert_eq!(retired.cache_order_capacity, 0);
    assert_eq!(
        old.get(&0u64.to_be_bytes()).unwrap(),
        Some(0u64.to_le_bytes().to_vec())
    );
    assert!(db.get(&0u64.to_be_bytes()).unwrap().is_some());
    let current = db.stats().unwrap();
    assert!(current.cache_map_capacity > 0 || current.cache_order_capacity > 0);
    drop(old);
    db.collect().unwrap();
}

#[test]
fn repeated_pinned_retirements_release_capacity_and_stay_readable() {
    let db = Database::create(
        path("repeated-retirement"),
        Options {
            cache_bytes: 2 * 1024 * 1024,
            ..Options::default()
        },
    )
    .unwrap();
    let mut pins = Vec::new();
    for generation in 0..5u64 {
        let mut tx = db.begin().unwrap();
        for i in 0..300u64 {
            tx.set(i.to_be_bytes(), (generation * 1000 + i).to_le_bytes())
                .unwrap();
        }
        tx.commit().unwrap();
        for i in 0..300u64 {
            db.get(&i.to_be_bytes()).unwrap().unwrap();
        }
        let warmed = db.stats().unwrap();
        assert!(warmed.cache_map_capacity > 0);
        assert!(warmed.cache_order_capacity > 0);
        // A normal cache clear should preserve capacity for later reuse.
        db.clear_cache();
        let cleared = db.stats().unwrap();
        assert_eq!(cleared.cache_map_capacity, warmed.cache_map_capacity);
        assert_eq!(cleared.cache_order_capacity, warmed.cache_order_capacity);
        assert_eq!(cleared.cache_entries, 0);
        for i in 0..300u64 {
            db.get(&i.to_be_bytes()).unwrap().unwrap();
        }
        pins.push((db.snapshot().unwrap(), generation));
        db.compact().unwrap();
        assert_eq!(db.collect().unwrap(), 0);
        for (snapshot, version) in &pins {
            for i in 0..300u64 {
                assert_eq!(
                    snapshot.get(&i.to_be_bytes()).unwrap(),
                    Some((version * 1000 + i).to_le_bytes().to_vec())
                );
            }
        }
        let stats = db.stats().unwrap();
        assert_eq!(stats.retained_epochs, pins.len());
        assert_eq!(stats.retired_cache_map_capacity, 0);
        assert_eq!(stats.retired_cache_order_capacity, 0);
    }
    drop(pins);
    assert_eq!(db.collect().unwrap(), 5);
    db.verify().unwrap();
}
