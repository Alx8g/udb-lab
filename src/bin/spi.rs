use std::env;
use std::path::PathBuf;

use udb_lab::spi::{Database, Options};

fn usage() -> ! {
    eprintln!(
        "usage:\n  spi init PATH\n  spi get PATH KEY_HEX\n  spi set PATH KEY_HEX VALUE_HEX\n  spi delete PATH KEY_HEX\n  spi scan-prefix PATH PREFIX_HEX\n  spi batch PATH OPS_JSON\n  spi verify PATH\n  spi stats PATH\n  spi compact PATH\n  spi collect PATH"
    );
    std::process::exit(2);
}

fn hex_decode(input: &str) -> Result<Vec<u8>, String> {
    if input.len() % 2 != 0 {
        return Err("hex input must have an even number of characters".into());
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for i in (0..bytes.len()).step_by(2) {
        let high = digit(bytes[i])?;
        let low = digit(bytes[i + 1])?;
        out.push((high << 4) | low);
    }
    Ok(out)
}

fn digit(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(format!("invalid hex digit: {byte}")),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

fn open(path: &str) -> Result<Database, Box<dyn std::error::Error>> {
    Ok(Database::open(path, Options::default())?)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let command = args.next().unwrap_or_else(|| usage());
    match command.as_str() {
        "init" => {
            let path = PathBuf::from(args.next().unwrap_or_else(|| usage()));
            Database::create(path, Options::default())?;
            println!("initialized");
        }
        "get" => {
            let path = args.next().unwrap_or_else(|| usage());
            let key = hex_decode(&args.next().unwrap_or_else(|| usage()))?;
            match open(&path)?.get(&key)? {
                Some(value) => println!("{}", hex_encode(&value)),
                None => println!("not-found"),
            }
        }
        "set" => {
            let path = args.next().unwrap_or_else(|| usage());
            let key = hex_decode(&args.next().unwrap_or_else(|| usage()))?;
            let value = hex_decode(&args.next().unwrap_or_else(|| usage()))?;
            let db = open(&path)?;
            let mut tx = db.begin()?;
            tx.set(key, value)?;
            println!("generation={}", tx.commit()?);
        }
        "delete" => {
            let path = args.next().unwrap_or_else(|| usage());
            let key = hex_decode(&args.next().unwrap_or_else(|| usage()))?;
            let db = open(&path)?;
            let mut tx = db.begin()?;
            tx.delete(key)?;
            println!("generation={}", tx.commit()?);
        }
        "scan-prefix" => {
            let path = args.next().unwrap_or_else(|| usage());
            let prefix = hex_decode(&args.next().unwrap_or_else(|| usage()))?;
            for entry in open(&path)?.scan_prefix(&prefix)? {
                println!("{}={}", hex_encode(&entry.key), hex_encode(&entry.value));
            }
        }
        "batch" => {
            let path = args.next().unwrap_or_else(|| usage());
            let file = args.next().unwrap_or_else(|| usage());
            let ops: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(file)?)?;
            let db = open(&path)?;
            let mut tx = db.begin()?;
            for op in ops {
                let key = hex_decode(
                    op.get("key")
                        .and_then(|v| v.as_str())
                        .ok_or("missing hex key")?,
                )?;
                match op.get("op").and_then(|v| v.as_str()) {
                    Some("set") => tx.set(
                        key,
                        hex_decode(
                            op.get("value")
                                .and_then(|v| v.as_str())
                                .ok_or("missing hex value")?,
                        )?,
                    )?,
                    Some("delete") => tx.delete(key)?,
                    _ => return Err("batch operation must be set or delete".into()),
                }
            }
            println!("generation={}", tx.commit()?);
        }
        "verify" => {
            let path = args.next().unwrap_or_else(|| usage());
            println!("verified_keys={}", open(&path)?.verify()?);
        }
        "stats" => {
            let path = args.next().unwrap_or_else(|| usage());
            println!("{}", serde_json::to_string_pretty(&open(&path)?.stats()?)?);
        }
        "compact" => {
            let path = args.next().unwrap_or_else(|| usage());
            let db = open(&path)?;
            db.compact()?;
            println!("compacted");
        }
        "collect" => {
            let path = args.next().unwrap_or_else(|| usage());
            let db = open(&path)?;
            println!("removed={}", db.collect()?);
        }
        _ => usage(),
    }
    Ok(())
}
