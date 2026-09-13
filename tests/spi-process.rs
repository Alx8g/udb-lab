use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use udb_lab::spi::{Database, Options};
fn path() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".working/tmp/spi-process");
    fs::create_dir_all(&p).unwrap();
    p.join(format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}
fn populate(p: &Path) {
    let db = Database::create(p, Options::default()).unwrap();
    let mut t = db.begin().unwrap();
    t.set(b"a", b"old").unwrap();
    t.set(b"b", b"old").unwrap();
    t.commit().unwrap();
}
#[test]
fn process_exit_at_every_commit_publication_stage() {
    let operations = path().with_extension("json");
    fs::write(
        &operations,
        r#"[{"op":"set","key":"61","value":"6e6577"},{"op":"set","key":"62","value":"6e6577"}]"#,
    )
    .unwrap();
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
        let p = path();
        populate(&p);
        let output = Command::new(env!("CARGO_BIN_EXE_spi"))
            .args(["batch"])
            .arg(&p)
            .arg(&operations)
            .env("SPI_CRASH_AT", stage)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(86), "{stage}: {:?}", output);
        let db = Database::open(&p, Options::default()).unwrap();
        let a = db.get(b"a").unwrap().unwrap();
        if matches!(stage, "manifest_replace" | "directory_sync") {
            assert_eq!(a, b"new");
        } else {
            assert_eq!(a, b"old");
        }
        assert_eq!(db.get(b"b").unwrap(), Some(a));
        db.verify().unwrap();
        let mut t = db.begin().unwrap();
        t.set(b"after", b"recovery").unwrap();
        t.commit().unwrap();
    }
}
#[test]
fn process_exit_during_compaction_does_not_mix_epochs() {
    for stage in [
        "record_header",
        "record_payload",
        "append_flush_half",
        "append_flush",
        "compact_data_sync",
        "manifest_write",
        "manifest_sync",
        "manifest_replace",
        "directory_sync",
    ] {
        let p = path();
        populate(&p);
        let output = Command::new(env!("CARGO_BIN_EXE_spi"))
            .arg("compact")
            .arg(&p)
            .env("SPI_CRASH_AT", stage)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(86));
        let db = Database::open(&p, Options::default()).unwrap();
        assert_eq!(db.get(b"a").unwrap(), Some(b"old".to_vec()));
        assert_eq!(db.get(b"b").unwrap(), Some(b"old".to_vec()));
        db.compact().unwrap();
        db.collect().unwrap();
        db.verify().unwrap();
    }
}
#[test]
fn cli_init_batch_get_scan_verify_and_reopen() {
    let p = path();
    let cli = env!("CARGO_BIN_EXE_spi");
    let init = Command::new(cli).arg("init").arg(&p).output().unwrap();
    assert!(init.status.success());
    let set = Command::new(cli)
        .arg("set")
        .arg(&p)
        .args(["00ff", "ff0001"])
        .output()
        .unwrap();
    assert!(set.status.success());
    let get = Command::new(cli)
        .arg("get")
        .arg(&p)
        .arg("00ff")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(get.stdout).unwrap().trim(), "ff0001");
    let verify = Command::new(cli).arg("verify").arg(&p).output().unwrap();
    assert!(verify.status.success());
    assert_eq!(
        String::from_utf8(verify.stdout).unwrap().trim(),
        "verified_keys=1"
    );
    let invalid = Command::new(cli)
        .arg("set")
        .arg(&p)
        .args(["badhex", "00"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
}
