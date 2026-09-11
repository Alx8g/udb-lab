mod answer_cells;
mod certificate;
mod coordination;
mod correlation;
mod engine;
mod executable;
mod factorized;
mod joint;
mod pgm;
mod progressive;
mod redundancy;
mod shared;
mod stats;
mod wcoj;

use stats::{print_record, Record};
use std::env;
use std::fs;
use std::time::Instant;

fn run_correctness() -> Result<(), String> {
    factorized::correctness()?;
    answer_cells::correctness()?;
    executable::correctness()?;
    progressive::correctness()?;
    certificate::correctness()?;
    wcoj::correctness()?;
    coordination::correctness()?;
    redundancy::correctness()?;
    pgm::correctness()?;
    engine::correctness()?;
    shared::correctness()?;
    joint::correctness()?;
    correlation::correctness()?;
    Ok(())
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let quick = args.iter().any(|a| a == "--quick");
    let only: Option<&str> = args.windows(2).find_map(|w| {
        if w[0] == "--only" {
            Some(w[1].as_str())
        } else {
            None
        }
    });

    eprintln!("udb-lab  hardware=i9-12900H  rustc=release  quick={quick}");
    let t0 = Instant::now();
    if let Err(e) = run_correctness() {
        eprintln!("CORRECTNESS FAILED: {e}");
        std::process::exit(1);
    }
    eprintln!("correctness passed in {:.2}s", t0.elapsed().as_secs_f64());

    let mut all: Vec<Record> = Vec::new();
    let mut run = |name: &str, f: fn(bool) -> Vec<Record>| {
        if let Some(only) = only {
            if only != name {
                return;
            }
        }
        eprintln!("\n=== {name} ===");
        let t = Instant::now();
        let recs = f(quick);
        for r in &recs {
            print_record(r);
        }
        eprintln!("{name} done in {:.2}s ({} records)", t.elapsed().as_secs_f64(), recs.len());
        all.extend(recs);
    };

    run("factorized", factorized::run);
    run("answer_cells", answer_cells::run);
    run("executable_regions", executable::run);
    run("progressive", progressive::run);
    run("certificate", certificate::run);
    run("wcoj", wcoj::run);
    run("coordination", coordination::run);
    run("redundancy", redundancy::run);
    run("pgm", pgm::run);
    run("engine", engine::run);
    run("shared_state", shared::run);
    run("joint", joint::run);
    run("correlation", correlation::run);

    let out_dir = if quick {
        "results/quick"
    } else {
        "results/full"
    };
    fs::create_dir_all(out_dir).expect("results dir");
    let path = format!("{out_dir}/records.json");
    fs::write(&path, serde_json::to_string_pretty(&all).unwrap()).unwrap();
    eprintln!(
        "\n{} records -> {path}  total {:.1}s",
        all.len(),
        t0.elapsed().as_secs_f64()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_correctness() {
        run_correctness().expect("correctness");
    }
}
