//! Interleaved dependent reads. Same 4,194,304 logical follows, more overlap.
//! Matches database_theory_round1/src/engine_lab.cpp chain_lab.

use crate::stats::{record, time_ns, Record};
use rand::rngs::SmallRng;
use rand::{seq::SliceRandom, Rng, SeedableRng};
use serde_json::json;

fn follow(links: &[u32], starts: &[u32], depth: usize, width: usize, out: &mut [u32]) {
    let mut i = 0usize;
    let mut pos = vec![0u32; width];
    while i < starts.len() {
        let w = width.min(starts.len() - i);
        for k in 0..w {
            pos[k] = starts[i + k];
        }
        for _ in 0..depth {
            for k in 0..w {
                pos[k] = links[pos[k] as usize];
            }
        }
        for k in 0..w {
            out[i + k] = pos[k];
        }
        i += w;
    }
}

fn xor_out(out: &[u32]) -> u64 {
    out.iter().fold(0u64, |a, &x| a.wrapping_add(x as u64))
}

pub fn correctness() -> Result<(), String> {
    let n = 1 << 16;
    let mut rng = SmallRng::seed_from_u64(3);
    let mut links: Vec<u32> = (0..n as u32).collect();
    links.shuffle(&mut rng);
    let starts: Vec<u32> = (0..256).map(|_| rng.gen_range(0..n as u32)).collect();
    let mut serial = vec![0u32; starts.len()];
    follow(&links, &starts, 16, 1, &mut serial);
    for w in [1usize, 4, 16, 32] {
        let mut got = vec![0u32; starts.len()];
        follow(&links, &starts, 16, w, &mut got);
        if got != serial {
            return Err(format!("chase width {w} mismatch"));
        }
    }
    eprintln!("chase correctness: interleaved widths match serial endpoints");
    Ok(())
}

pub fn run(quick: bool) -> Vec<Record> {
    let mut out = Vec::new();
    let n = if quick { 1 << 20 } else { 1 << 25 };
    let nstart = if quick { 8_192usize } else { 65_536usize };
    let depth = 64usize;
    let mut rng = SmallRng::seed_from_u64(1667);
    let mut links: Vec<u32> = (0..n as u32).collect();
    links.shuffle(&mut rng);
    let starts: Vec<u32> = (0..nstart).map(|_| rng.gen_range(0..n as u32)).collect();
    let ops = (nstart * depth) as u64;
    let mut ref_out = vec![0u32; nstart];
    follow(&links, &starts, depth, 1, &mut ref_out);
    let serial = xor_out(&ref_out);

    let mut buf = vec![0u32; nstart];
    for w in [1usize, 4, 16, 32, 64] {
        let (val, times, reps) = time_ns(1, if quick { 3 } else { 5 }, || {
            follow(&links, &starts, depth, w, &mut buf);
            xor_out(&buf)
        });
        if val != serial {
            panic!("chase output mismatch width={w}");
        }
        out.push(record(
            "chase",
            &format!("interleave_{w}"),
            n as u64,
            json!({"starts": nstart, "depth": depth, "width": w, "logical_reads": ops}),
            times,
            reps,
            ops,
            ops * 4,
            json!({"sum": val, "bytes_links": n * 4}),
            "same logical follows; width is in-flight independent chains",
        ));
    }
    out
}
