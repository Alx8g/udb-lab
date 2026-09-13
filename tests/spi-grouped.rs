use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use udb_lab::spi::{Database, Entry, Error, Options};

fn path(name: &str) -> PathBuf {
    static ID: AtomicU64 = AtomicU64::new(0);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".working/tmp/spi-grouped");
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
fn rows(map: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<Entry> {
    map.iter()
        .map(|(key, value)| Entry {
            key: key.clone(),
            value: value.clone(),
        })
        .collect()
}
fn batch(db: &Database, items: &BTreeMap<Vec<u8>, Option<Vec<u8>>>) {
    let mut t = db.begin().unwrap();
    for (k, v) in items {
        match v {
            Some(v) => t.set(k.clone(), v.clone()).unwrap(),
            None => t.delete(k.clone()).unwrap(),
        }
    }
    t.commit().unwrap();
}
fn initial(n: u16) -> BTreeMap<Vec<u8>, Option<Vec<u8>>> {
    (0..n)
        .map(|i| (i.to_be_bytes().to_vec(), Some(vec![1; 64])))
        .collect()
}
fn next(s: &mut u64) -> u64 {
    *s = s
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *s
}

#[test]
fn grouped_bulk_and_replacements_reduce_actual_record_bytes() {
    assert!(!Options::default().grouped_updates);
    for buffered in [false, true] {
        let mut written = Vec::new();
        for grouped in [false, true] {
            let db = Database::create(
                path("bytes"),
                Options {
                    cache_bytes: 0,
                    append_buffer: buffered,
                    grouped_updates: grouped,
                    ..Options::default()
                },
            )
            .unwrap();
            batch(&db, &initial(256));
            let load = db.stats().unwrap().bytes_written;
            if grouped {
                // Exactly one value and one node record per key, excluding arena header.
                assert_eq!(load, 256 * (32 + 64 + 32 + 40 + 2));
            }
            let old = db.snapshot().unwrap();
            let updates = (0..256u16)
                .map(|i| (i.to_be_bytes().to_vec(), Some(vec![2; 64])))
                .collect();
            batch(&db, &updates);
            let replaced = db.stats().unwrap().bytes_written - load;
            if grouped {
                assert_eq!(replaced, load);
            }
            assert_eq!(db.verify().unwrap(), 256);
            assert!(old
                .scan(&[], None)
                .unwrap()
                .iter()
                .all(|r| r.value == vec![1; 64]));
            assert!(db
                .scan_prefix(&[])
                .unwrap()
                .iter()
                .all(|r| r.value == vec![2; 64]));
            written.push((load, replaced));
        }
        assert!(written[1].0 < written[0].0 / 2, "{written:?}");
        assert!(written[1].1 < written[0].1 / 2, "{written:?}");
    }
}

#[test]
fn grouped_randomized_matches_control_oracle_and_pinned_epochs() {
    for (cache_bytes, append_buffer, value_cache) in
        [(0, true, false), (1024, false, false), (8192, true, true)]
    {
        let base = Options {
            cache_bytes,
            append_buffer,
            value_cache,
            ..Options::default()
        };
        let paths = [path("oracle-control"), path("oracle-grouped")];
        let mut databases: Vec<_> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| {
                Database::create(
                    p,
                    Options {
                        grouped_updates: i == 1,
                        ..base.clone()
                    },
                )
                .unwrap()
            })
            .collect();
        let mut oracle: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut pins = Vec::new();
        let mut state = 71;
        for round in 0..45 {
            let mut writes = BTreeMap::new();
            if round == 0 {
                writes = initial(65);
                writes.insert(vec![], Some(vec![]));
                writes.insert(vec![255, 0, 255], Some(vec![3; 70000]));
                writes.insert(vec![255; 4096], Some(vec![8; 32]));
            } else if round % 3 == 0 {
                // Guaranteed replacement-only batches with clustered and sparse keys.
                for k in oracle.keys().step_by(3) {
                    writes.insert(k.clone(), Some(vec![round as u8; 65]));
                }
            } else {
                for _ in 0..24 {
                    let k = (next(&mut state) % 100) as u16;
                    let v = if next(&mut state) % 5 == 0 {
                        None
                    } else {
                        Some(vec![round as u8; (next(&mut state) % 80) as usize])
                    };
                    writes.insert(k.to_be_bytes().to_vec(), v);
                }
            }
            for db in &databases {
                batch(db, &writes);
            }
            for (k, v) in writes {
                if let Some(v) = v {
                    oracle.insert(k, v);
                } else {
                    oracle.remove(&k);
                }
            }
            let want = rows(&oracle);
            for db in &databases {
                assert_eq!(db.scan_prefix(&[]).unwrap(), want);
                assert_eq!(db.verify().unwrap(), oracle.len() as u64);
                for (k, v) in &oracle {
                    assert_eq!(db.get(k).unwrap().as_ref(), Some(v));
                }
                assert!(db.stats().unwrap().cache_bytes <= cache_bytes);
            }
            if round % 11 == 0 {
                pins.push((
                    databases
                        .iter()
                        .map(|db| db.snapshot().unwrap())
                        .collect::<Vec<_>>(),
                    want,
                ));
                for db in &databases {
                    db.compact().unwrap();
                    db.collect().unwrap();
                }
            }
            for (snapshots, want) in &pins {
                for snapshot in snapshots {
                    assert_eq!(&snapshot.scan(&[], None).unwrap(), want);
                }
            }
        }
        drop(pins);
        for db in &databases {
            db.collect().unwrap();
        }
        databases.clear();
        // The sequential reader/writer opens both physical layouts unchanged.
        for p in &paths {
            let db = Database::open(p, base.clone()).unwrap();
            assert_eq!(db.scan_prefix(&[]).unwrap(), rows(&oracle));
            assert_eq!(db.verify().unwrap(), oracle.len() as u64);
        }
    }
}

#[test]
fn grouped_fallback_is_byte_identical_for_deletes_mixed_single_and_scratch_limits() {
    for budget in [4096, 32 * 1024 * 1024] {
        let paths = [path("fallback-control"), path("fallback-grouped")];
        let databases: Vec<_> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| {
                Database::create(
                    p,
                    Options {
                        grouped_updates: i == 1,
                        transaction_bytes: budget,
                        cache_bytes: 0,
                        ..Options::default()
                    },
                )
                .unwrap()
            })
            .collect();
        let mut batches = vec![
            BTreeMap::from([(vec![0], None), (vec![1], None)]), // Empty-tree delete must not unwrap.
            BTreeMap::from([(vec![0], Some(vec![])), (vec![1], None)]),
            BTreeMap::from([(vec![2], Some(vec![1]))]),
            BTreeMap::from([(vec![0], None), (vec![1], Some(vec![3]))]),
        ];
        if budget == 4096 {
            // Exercise a small accepted staging budget as well as the default.
            batches.push(BTreeMap::from([(vec![1], None), (vec![2], None)]));
        }
        for writes in batches {
            for db in &databases {
                batch(db, &writes);
                db.verify().unwrap();
            }
            assert_eq!(
                fs::read(paths[0].join("arena-0.spi")).unwrap(),
                fs::read(paths[1].join("arena-0.spi")).unwrap()
            );
        }
    }
    // More than 64 KiB of reference slots, but comfortably inside staging budget.
    let paths = [path("scratch-control"), path("scratch-grouped")];
    for (i, p) in paths.iter().enumerate() {
        let db = Database::create(
            p,
            Options {
                grouped_updates: i == 1,
                ..Options::default()
            },
        )
        .unwrap();
        batch(&db, &initial(2049));
        db.verify().unwrap();
    }
    assert_eq!(
        fs::read(paths[0].join("arena-0.spi")).unwrap(),
        fs::read(paths[1].join("arena-0.spi")).unwrap()
    );
}

#[test]
fn grouped_preserves_logical_revisions_and_rejects_stale_reads_and_ranges() {
    let db = Database::create(
        path("conflicts"),
        Options {
            grouped_updates: true,
            ..Options::default()
        },
    )
    .unwrap();
    batch(&db, &initial(7)); // Midpoint key 3 is the bulk-built root.
    let mut unaffected = db.begin().unwrap();
    unaffected.get(&3u16.to_be_bytes()).unwrap();
    let mut stale = db.begin().unwrap();
    stale.get(&0u16.to_be_bytes()).unwrap();
    let mut range = db.begin().unwrap();
    range.scan(&[], None).unwrap();
    batch(
        &db,
        &BTreeMap::from([
            (0u16.to_be_bytes().to_vec(), Some(vec![2])),
            (6u16.to_be_bytes().to_vec(), Some(vec![2])),
        ]),
    );
    // Root was copied, but its own logical revision must not change.
    unaffected.set(b"independent", b"ok").unwrap();
    unaffected.commit().unwrap();
    assert!(matches!(stale.commit(), Err(Error::Conflict)));
    assert!(matches!(range.commit(), Err(Error::Conflict)));
    let mut duplicate = db.begin().unwrap();
    duplicate.set([0, 0], b"first").unwrap();
    duplicate.delete([0, 0]).unwrap();
    duplicate.set([0, 0], b"last").unwrap();
    duplicate.set([0, 1], b"second").unwrap();
    duplicate.commit().unwrap();
    assert_eq!(db.get(&[0, 0]).unwrap(), Some(b"last".to_vec()));
    db.verify().unwrap();
}

#[test]
fn grouped_injected_failures_publish_only_complete_old_or_new_roots() {
    for populate in [false, true] {
        for buffered in [false, true] {
            let mut failures = 0;
            let mut successes = 0;
            for at in 1..=24 {
                let p = path("fault");
                let db = Database::create(&p, Options::default()).unwrap();
                if populate {
                    batch(&db, &initial(2));
                }
                let old = db.scan_prefix(&[]).unwrap();
                drop(db);
                let db = Database::open(
                    &p,
                    Options {
                        grouped_updates: true,
                        append_buffer: buffered,
                        cache_bytes: 0,
                        fail_at: Some(at),
                        write_chunk: Some(3),
                        ..Options::default()
                    },
                )
                .unwrap();
                let pinned = db.snapshot().unwrap();
                let mut t = db.begin().unwrap();
                for k in 0..2u16 {
                    t.set(k.to_be_bytes(), vec![2; 64]).unwrap();
                }
                let result = t.commit();
                if result.is_err() {
                    failures += 1;
                    assert!(matches!(db.get(&[0, 0]), Err(Error::Poisoned)));
                } else {
                    successes += 1;
                }
                assert_eq!(pinned.scan(&[], None).unwrap(), old);
                drop(pinned);
                drop(db);
                let db = Database::open(&p, Options::default()).unwrap();
                let want: Vec<_> = (0..2u16)
                    .map(|k| Entry {
                        key: k.to_be_bytes().to_vec(),
                        value: vec![2; 64],
                    })
                    .collect();
                let actual = db.scan_prefix(&[]).unwrap();
                assert!(actual == old || actual == want, "{at}");
                if result.is_ok() {
                    assert_eq!(actual, want);
                }
                db.verify().unwrap();
                batch(
                    &db,
                    &BTreeMap::from([(b"retry".to_vec(), Some(b"ok".to_vec()))]),
                );
                db.verify().unwrap();
            }
            assert!(failures > 0 && successes > 0);
        }
    }
}

// Runs only in an explicitly spawned child. No environment mutation in the parent.
#[test]
fn grouped_crash_child() {
    let Some(p) = std::env::var_os("SPI_GROUPED_CHILD_PATH") else {
        return;
    };
    let db = Database::open(
        PathBuf::from(p),
        Options {
            grouped_updates: true,
            cache_bytes: 0,
            ..Options::default()
        },
    )
    .unwrap();
    let mut t = db.begin().unwrap();
    for k in 0..2u16 {
        t.set(k.to_be_bytes(), vec![2; 64]).unwrap();
    }
    t.commit().unwrap();
    panic!("requested crash stage was not reached");
}

#[test]
fn grouped_process_exit_covers_bulk_and_replacement_publication() {
    for populate in [false, true] {
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
            let p = path("process");
            let db = Database::create(&p, Options::default()).unwrap();
            if populate {
                batch(&db, &initial(2));
            }
            let old = db.scan_prefix(&[]).unwrap();
            drop(db);
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "grouped_crash_child", "--nocapture"])
                .env("SPI_GROUPED_CHILD_PATH", &p)
                .env("SPI_CRASH_AT", stage)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(86), "{stage}: {output:?}");
            let db = Database::open(&p, Options::default()).unwrap();
            let want: Vec<_> = (0..2u16)
                .map(|k| Entry {
                    key: k.to_be_bytes().to_vec(),
                    value: vec![2; 64],
                })
                .collect();
            assert_eq!(
                db.scan_prefix(&[]).unwrap(),
                if matches!(stage, "manifest_replace" | "directory_sync") {
                    want
                } else {
                    old
                }
            );
            db.verify().unwrap();
        }
    }
}
