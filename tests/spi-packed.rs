use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use udb_lab::spi::{Database, Entry, Error, Options};

fn path() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".working/tmp/packed-tests");
    fs::create_dir_all(&root).unwrap();
    root.join(format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}
fn rows(m: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<Entry> {
    m.iter()
        .map(|(k, v)| Entry {
            key: k.clone(),
            value: v.clone(),
        })
        .collect()
}
fn next(s: &mut u64) -> u64 {
    *s = s
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *s
}
fn initial(db: &Database) {
    let mut t = db.begin().unwrap();
    for i in 0..500u16 {
        t.set(i.to_be_bytes(), vec![1; 64]).unwrap();
    }
    t.set([], []).unwrap();
    t.commit().unwrap();
}

#[test]
fn packed_oracle_splits_deltas_binary_values_snapshots_and_reopen() {
    for (cache, buffer) in [(0, true), (1024, false), (8 * 1024 * 1024, true)] {
        let p = path();
        let opts = Options {
            packed_pages: true,
            cache_bytes: cache,
            append_buffer: buffer,
            ..Options::default()
        };
        let db = Database::create(&p, opts.clone()).unwrap();
        let mut oracle = BTreeMap::new();
        let mut state = 71;
        let mut pins = Vec::new();
        for round in 0..35 {
            let mut t = db.begin().unwrap();
            let count = if round == 0 { 400 } else { 20 };
            for _ in 0..count {
                let k = (next(&mut state) >> 32) as u16 % 500;
                let k = k.to_be_bytes().to_vec();
                if round > 0 && next(&mut state) % 4 == 0 {
                    t.delete(k.clone()).unwrap();
                    oracle.remove(&k);
                } else {
                    let v = vec![round as u8; (next(&mut state) % 300) as usize];
                    t.set(k.clone(), v.clone()).unwrap();
                    oracle.insert(k, v);
                }
            }
            // Force overflow, exact page boundary and empty/max binary keys.
            for (k, v) in [
                (vec![], vec![]),
                (vec![255; 4096], vec![round as u8; 70000]),
                (vec![0, 255, 0], vec![round as u8; 1024]),
                (vec![255, 0, 255], vec![round as u8; 1025]),
            ] {
                t.set(k.clone(), v.clone()).unwrap();
                oracle.insert(k, v);
            }
            assert_eq!(t.scan(&[], None).unwrap(), rows(&oracle));
            t.commit().unwrap();
            assert_eq!(db.scan_prefix(&[]).unwrap(), rows(&oracle));
            assert_eq!(db.verify().unwrap(), oracle.len() as u64);
            for (k, v) in oracle.iter().step_by(7) {
                assert_eq!(db.get(k).unwrap().as_ref(), Some(v));
            }
            let lo = 100u16.to_be_bytes().to_vec();
            let hi = 300u16.to_be_bytes().to_vec();
            let want: Vec<_> = oracle
                .range(lo.clone()..hi.clone())
                .map(|(k, v)| Entry {
                    key: k.clone(),
                    value: v.clone(),
                })
                .collect();
            assert_eq!(db.snapshot().unwrap().scan(&lo, Some(&hi)).unwrap(), want);
            if round % 9 == 0 {
                pins.push((db.snapshot().unwrap(), rows(&oracle)));
                db.compact().unwrap();
                db.collect().unwrap();
            }
            for (pin, want) in &pins {
                assert_eq!(pin.scan(&[], None).unwrap(), *want);
            }
            assert!(db.stats().unwrap().cache_bytes <= cache);
        }
        drop(pins);
        db.collect().unwrap();
        drop(db);
        let db = Database::open(&p, Options::default()).unwrap(); // Format inferred from manifest.
        assert_eq!(db.scan_prefix(&[]).unwrap(), rows(&oracle));
        let mut t = db.begin().unwrap();
        for k in oracle.keys() {
            t.delete(k.clone()).unwrap();
        }
        t.commit().unwrap();
        assert!(db.snapshot().unwrap().is_empty());
        db.compact().unwrap();
        assert_eq!(db.verify().unwrap(), 0);
        let mut t = db.begin().unwrap();
        t.delete(b"absent").unwrap();
        t.commit().unwrap();
        let mut t = db.begin().unwrap();
        t.set(b"restart", b"ok").unwrap();
        t.commit().unwrap();
        assert_eq!(db.get(b"restart").unwrap(), Some(b"ok".to_vec()));
        db.verify().unwrap();
    }
}

#[test]
fn packed_logical_conflicts_do_not_follow_physical_pages() {
    let db = Database::create(
        path(),
        Options {
            packed_pages: true,
            ..Options::default()
        },
    )
    .unwrap();
    initial(&db);
    let mut a = db.begin().unwrap();
    let mut b = db.begin().unwrap();
    a.set([0, 1], b"a").unwrap();
    b.set([0, 2], b"b").unwrap();
    a.commit().unwrap();
    b.commit().unwrap();
    let mut stale = db.begin().unwrap();
    stale.get(&[0, 1]).unwrap();
    let mut t = db.begin().unwrap();
    t.set([0, 1], b"other").unwrap();
    t.commit().unwrap();
    assert!(matches!(stale.commit(), Err(Error::Conflict)));
    let mut range = db.begin().unwrap();
    range.scan(b"t/", Some(b"u/")).unwrap();
    let mut t = db.begin().unwrap();
    t.set(b"t/new", b"yes").unwrap();
    t.commit().unwrap();
    assert!(matches!(range.commit(), Err(Error::Conflict)));
    let mut missing = db.begin().unwrap();
    missing.get(b"missing").unwrap();
    let mut t = db.begin().unwrap();
    t.set(b"missing", b"x").unwrap();
    t.commit().unwrap();
    let mut t = db.begin().unwrap();
    t.delete(b"missing").unwrap();
    t.commit().unwrap();
    assert!(matches!(missing.commit(), Err(Error::Conflict)));
    db.verify().unwrap();
}

#[test]
fn packed_full_scan_reduces_record_reads_and_storage_bytes() {
    let mut counts = Vec::new();
    for packed_pages in [false, true] {
        let db = Database::create(
            path(),
            Options {
                packed_pages,
                cache_bytes: 0,
                ..Options::default()
            },
        )
        .unwrap();
        initial(&db);
        db.clear_cache();
        let before = db.stats().unwrap();
        let actual = db.scan_prefix(&[]).unwrap();
        let after = db.stats().unwrap();
        assert_eq!(actual.len(), 501);
        counts.push((after.reads - before.reads, after.committed_bytes));
        db.verify().unwrap();
    }
    assert!(counts[1].0 < counts[0].0 / 5, "{counts:?}");
    assert!(counts[1].1 < counts[0].1 / 2, "{counts:?}");
}

#[test]
fn packed_crash_child() {
    let Some(p) = std::env::var_os("SPI_PACKED_CHILD") else {
        return;
    };
    let db = Database::open(PathBuf::from(p), Options::default()).unwrap();
    if std::env::var_os("SPI_PACKED_COMPACT").is_some() {
        db.compact().unwrap();
    } else {
        let mut t = db.begin().unwrap();
        t.set(b"a", b"new").unwrap();
        t.set(b"b", b"new").unwrap();
        t.commit().unwrap();
    }
    panic!("crash stage not reached");
}
#[test]
fn packed_process_crashes_at_each_publication_boundary() {
    for mode in 0..3 {
        for stage in [
            "record_header",
            "record_payload",
            "append_flush_half",
            "append_flush",
            "data_sync",
            "manifest_write",
            "manifest_sync",
            "manifest_replace",
            "directory_sync",
        ] {
            let stage = if mode == 2 && stage == "data_sync" {
                "compact_data_sync"
            } else {
                stage
            };
            let p = path();
            let db = Database::create(
                &p,
                Options {
                    packed_pages: true,
                    ..Options::default()
                },
            )
            .unwrap();
            if mode > 0 {
                let mut t = db.begin().unwrap();
                t.set(b"a", b"old").unwrap();
                t.set(b"b", b"old").unwrap();
                t.commit().unwrap();
            }
            let old = db.scan_prefix(&[]).unwrap();
            drop(db);
            let mut cmd = Command::new(std::env::current_exe().unwrap());
            cmd.args(["--exact", "packed_crash_child", "--nocapture"])
                .env("SPI_PACKED_CHILD", &p)
                .env("SPI_CRASH_AT", stage);
            if mode == 2 {
                cmd.env("SPI_PACKED_COMPACT", "1");
            }
            let out = cmd.output().unwrap();
            assert_eq!(out.status.code(), Some(86), "{stage}: {out:?}");
            let db = Database::open(&p, Options::default()).unwrap();
            let want = if mode < 2 && matches!(stage, "manifest_replace" | "directory_sync") {
                vec![
                    Entry {
                        key: b"a".to_vec(),
                        value: b"new".to_vec(),
                    },
                    Entry {
                        key: b"b".to_vec(),
                        value: b"new".to_vec(),
                    },
                ]
            } else {
                old
            };
            assert_eq!(db.scan_prefix(&[]).unwrap(), want);
            db.verify().unwrap();
        }
    }
}

#[test]
fn packed_errors_budget_and_corruption_remain_explicit() {
    let p = path();
    let db = Database::create(
        &p,
        Options {
            packed_pages: true,
            scan_bytes: 256,
            ..Options::default()
        },
    )
    .unwrap();
    let mut t = db.begin().unwrap();
    t.set(b"x", vec![3; 2000]).unwrap();
    t.commit().unwrap();
    assert!(matches!(db.scan_prefix(&[]), Err(Error::Budget(_))));
    drop(db);
    let mut data = fs::read(p.join("arena-0.spi")).unwrap();
    data[18] ^= 1;
    fs::write(p.join("arena-0.spi"), data).unwrap();
    let db = Database::open(&p, Options::default()).unwrap();
    assert!(matches!(db.get(b"x"), Err(Error::Corrupt(_))));
    assert!(db.verify().is_err());
}

#[test]
fn packed_cache_shares_budget_and_retirement_drops_all_capacity() {
    let p = path();
    let db = Database::create(
        &p,
        Options {
            packed_pages: true,
            cache_bytes: 128 * 1024,
            ..Options::default()
        },
    )
    .unwrap();
    initial(&db);
    db.clear_cache();
    assert_eq!(db.stats().unwrap().cache_page_entries, 0);
    let first = db.snapshot().unwrap();
    for i in 0..500u16 {
        first.get(&i.to_be_bytes()).unwrap();
    }
    assert!(db.stats().unwrap().cache_page_entries > 0);
    let before = db.stats().unwrap().reads;
    for i in 0..500u16 {
        assert_eq!(db.get(&i.to_be_bytes()).unwrap(), Some(vec![1; 64]));
    }
    assert_eq!(db.stats().unwrap().reads, before);
    assert!(db.stats().unwrap().cache_bytes <= 128 * 1024);
    db.clear_cache();
    db.scan_prefix(&[]).unwrap();
    assert_eq!(
        db.stats().unwrap().cache_page_entries,
        0,
        "scans must not admit packed records"
    );
    for _ in 0..3 {
        for i in 0..500u16 {
            db.get(&i.to_be_bytes()).unwrap();
        }
        db.compact().unwrap();
        assert_eq!(db.stats().unwrap().retired_cache_page_capacity, 0);
        assert_eq!(first.get(&7u16.to_be_bytes()).unwrap(), Some(vec![1; 64]));
        assert_eq!(db.stats().unwrap().retired_cache_page_capacity, 0);
    }
    drop(first);
    db.collect().unwrap();
}

#[test]
fn packed_fault_matrix_keeps_complete_snapshot_and_retry() {
    for populated in [false, true] {
        for at in 1..=30 {
            let p = path();
            let db = Database::create(
                &p,
                Options {
                    packed_pages: true,
                    ..Options::default()
                },
            )
            .unwrap();
            if populated {
                let mut t = db.begin().unwrap();
                t.set(b"a", b"old").unwrap();
                t.set(b"b", b"old").unwrap();
                t.commit().unwrap();
            }
            let old = db.scan_prefix(&[]).unwrap();
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
            let pinned = db.snapshot().unwrap();
            let mut t = db.begin().unwrap();
            t.set(b"a", b"new").unwrap();
            t.set(b"b", b"new").unwrap();
            let result = t.commit();
            if result.is_err() {
                assert!(matches!(db.get(b"a"), Err(Error::Poisoned)));
            }
            assert_eq!(pinned.scan_prefix(&[]).unwrap(), old);
            drop(pinned);
            drop(db);
            let db = Database::open(&p, Options::default()).unwrap();
            let new = vec![
                Entry {
                    key: b"a".to_vec(),
                    value: b"new".to_vec(),
                },
                Entry {
                    key: b"b".to_vec(),
                    value: b"new".to_vec(),
                },
            ];
            let actual = db.scan_prefix(&[]).unwrap();
            assert!(actual == old || actual == new);
            if result.is_ok() {
                assert_eq!(actual, new);
            }
            let mut t = db.begin().unwrap();
            t.set(b"retry", b"ok").unwrap();
            t.commit().unwrap();
            db.verify().unwrap();
        }
    }
}

#[test]
fn packed_delta_bound_matches_oracle_at_each_consolidation() {
    let p = path();
    let db = Database::create(
        &p,
        Options {
            packed_pages: true,
            ..Options::default()
        },
    )
    .unwrap();
    initial(&db);
    let mut pins = Vec::new();
    for n in 0..40u8 {
        let mut t = db.begin().unwrap();
        t.set([0, 1], vec![n; 64]).unwrap();
        t.commit().unwrap();
        assert_eq!(db.get(&[0, 1]).unwrap(), Some(vec![n; 64]));
        assert_eq!(db.get(&[0, 2]).unwrap(), Some(vec![1; 64]));
        if n % 7 == 0 {
            pins.push((db.snapshot().unwrap(), n));
        }
        assert_eq!(db.verify().unwrap(), 501);
    }
    for (pin, n) in &pins {
        assert_eq!(pin.get(&[0, 1]).unwrap(), Some(vec![*n; 64]));
    }
    drop(pins);
    drop(db);
    let db = Database::open(p, Options::default()).unwrap();
    assert_eq!(db.get(&[0, 1]).unwrap(), Some(vec![39; 64]));
    db.verify().unwrap();
}

#[test]
fn packed_and_legacy_manifest_identities_are_not_reinterpreted() {
    assert!(!Options::default().packed_pages);
    for packed in [false, true] {
        let p = path();
        let db = Database::create(
            &p,
            Options {
                packed_pages: packed,
                ..Options::default()
            },
        )
        .unwrap();
        initial(&db);
        drop(db);
        assert_eq!(
            &fs::read(p.join("manifest.spi")).unwrap()[..8],
            if packed { b"SPIMETA2" } else { b"SPIMETA1" }
        );
        let db = Database::open(
            &p,
            Options {
                packed_pages: !packed,
                ..Options::default()
            },
        )
        .unwrap();
        assert_eq!(db.stats().unwrap().packed_format, packed);
        let mut t = db.begin().unwrap();
        t.set(b"new", b"write").unwrap();
        t.commit().unwrap();
        db.verify().unwrap();
        assert_eq!(
            &fs::read(p.join("manifest.spi")).unwrap()[..8],
            if packed { b"SPIMETA2" } else { b"SPIMETA1" }
        );
    }
}

#[test]
fn packed_concurrent_commits_compaction_and_reopen_preserve_all_rows() {
    use std::sync::{Arc, Barrier};
    let p = path();
    let db = Database::create(
        &p,
        Options {
            packed_pages: true,
            ..Options::default()
        },
    )
    .unwrap();
    initial(&db);
    let barrier = Arc::new(Barrier::new(4));
    let mut workers = Vec::new();
    for worker in 0..3u16 {
        let db = db.clone();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            for iteration in 0..15u16 {
                let key = (1000 + worker * 100 + iteration).to_be_bytes();
                let mut t = db.begin().unwrap();
                t.set(key, iteration.to_le_bytes()).unwrap();
                t.commit().unwrap();
            }
        }));
    }
    barrier.wait();
    for _ in 0..8 {
        match db.compact() {
            Ok(()) | Err(Error::Conflict) => (),
            Err(e) => panic!("compaction {e}"),
        }
    }
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(db.verify().unwrap(), 546);
    db.compact().unwrap();
    db.collect().unwrap();
    drop(db);
    let db = Database::open(p, Options::default()).unwrap();
    for worker in 0..3u16 {
        for iteration in 0..15u16 {
            assert_eq!(
                db.get(&(1000 + worker * 100 + iteration).to_be_bytes())
                    .unwrap(),
                Some(iteration.to_le_bytes().to_vec())
            );
        }
    }
    assert_eq!(db.verify().unwrap(), 546);
}
