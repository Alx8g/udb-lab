use std::env;
use std::path::PathBuf;
use std::time::Instant;

use udb_lab::spi::{Database, Options};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let path = PathBuf::from(args.next().unwrap_or_else(|| "target/spi-bench-db".into()));
    let rows: usize = args.next().as_deref().unwrap_or("10000").parse()?;
    let rounds: usize = args.next().as_deref().unwrap_or("3").parse()?;
    let batch_size: usize = args.next().as_deref().unwrap_or("1").parse()?;
    if batch_size == 0 {
        return Err("batch size must be positive".into());
    }
    if path.exists() {
        return Err("benchmark destination exists; use a new path".into());
    }
    let db = Database::create(&path, Options::default())?;

    let start = Instant::now();
    for start in (0..rows).step_by(batch_size) {
        let mut tx = db.begin()?;
        for i in start..(start + batch_size).min(rows) {
            let key = format!("key/{i:08}").into_bytes();
            let value = (i as u64).to_le_bytes().to_vec();
            tx.set(key, value)?;
        }
        tx.commit()?;
    }
    let load_ms = start.elapsed().as_secs_f64() * 1000.0;

    let start = Instant::now();
    let mut checksum = 0u64;
    for round in 0..rounds {
        for i in 0..rows {
            let key = format!("key/{:08}", (i * 7919 + round) % rows).into_bytes();
            if let Some(value) = db.get(&key)? {
                checksum ^= u64::from_le_bytes(value.try_into().unwrap());
            }
        }
    }
    let read_ms = start.elapsed().as_secs_f64() * 1000.0;
    let before = db.stats()?.physical_bytes;
    db.compact()?;
    db.collect()?;
    let after = db.stats()?.physical_bytes;
    println!("{{\"rows\":{rows},\"rounds\":{rounds},\"batch_size\":{batch_size},\"load_ms\":{load_ms:.3},\"read_ms\":{read_ms:.3},\"checksum\":{checksum},\"physical_before\":{before},\"physical_after_compact\":{after}}}");
    Ok(())
}
