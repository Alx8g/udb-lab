//! Factorized join-aggregate vs expanded pairwise work.
//!
//! Identity: sum_i sum_j a_i * b_j = (sum a) * (sum b)
//! Incremental insert of x into A changes the product by x * sum(B).
//!
//! The naive expansion is the "necessary work" a row engine does when it
//! materializes the join. The factorized form never creates those pairs.

use crate::stats::{record, time_ns, Record};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde_json::json;

#[derive(Clone)]
struct Group {
    a: Vec<i64>,
    b: Vec<i64>,
}

fn gen_groups(groups: usize, per: usize, seed: u64) -> Vec<Group> {
    let mut rng = SmallRng::seed_from_u64(seed);
    (0..groups)
        .map(|_| Group {
            a: (0..per).map(|_| rng.gen_range(1..50)).collect(),
            b: (0..per).map(|_| rng.gen_range(1..50)).collect(),
        })
        .collect()
}

fn naive_pair_sum(g: &Group) -> i128 {
    let mut s: i128 = 0;
    for &a in &g.a {
        for &b in &g.b {
            s += a as i128 * b as i128;
        }
    }
    s
}

fn factorized_sum(g: &Group) -> i128 {
    let sa: i128 = g.a.iter().map(|&x| x as i128).sum();
    let sb: i128 = g.b.iter().map(|&x| x as i128).sum();
    sa * sb
}

/// Hash-join style: still expands pairs after matching on a single group key.
fn hashjoin_style_sum(g: &Group) -> i128 {
    // After the join key matches, a nested product remains.
    naive_pair_sum(g)
}

#[derive(Clone)]
struct Maintained {
    sum_a: i128,
    sum_b: i128,
}

impl Maintained {
    fn from_group(g: &Group) -> Self {
        Self {
            sum_a: g.a.iter().map(|&x| x as i128).sum(),
            sum_b: g.b.iter().map(|&x| x as i128).sum(),
        }
    }
    fn result(&self) -> i128 {
        self.sum_a * self.sum_b
    }
    fn insert_a(&mut self, x: i64) {
        self.sum_a += x as i128;
    }
    fn delete_a(&mut self, x: i64) {
        self.sum_a -= x as i128;
    }
    fn insert_b(&mut self, x: i64) {
        self.sum_b += x as i128;
    }
    fn update_a(&mut self, old: i64, new: i64) {
        self.sum_a += new as i128 - old as i128;
    }
}

/// Non-separable aggregate: sum_{i,j} a_i * b_j * (i as i64 xor j as i64).
/// Factorization does not apply. Used as a negative control.
fn inseparable_pair_sum(g: &Group) -> i128 {
    let mut s: i128 = 0;
    for (i, &a) in g.a.iter().enumerate() {
        for (j, &b) in g.b.iter().enumerate() {
            s += a as i128 * b as i128 * ((i as i64 ^ j as i64) as i128);
        }
    }
    s
}

pub fn correctness() -> Result<(), String> {
    let groups = gen_groups(32, 48, 7);
    let mut states = 0u64;
    let mut mutations = 0u64;
    for g in &groups {
        let mut cur = g.clone();
        let mut m = Maintained::from_group(&cur);
        for step in 0..80 {
            states += 1;
            let naive = naive_pair_sum(&cur);
            let fact = factorized_sum(&cur);
            let maint = m.result();
            if naive != fact || fact != maint {
                return Err(format!(
                    "mismatch state={states} step={step} naive={naive} fact={fact} maint={maint}"
                ));
            }
            let k = step % 5;
            match k {
                0 => {
                    let x = 3 + (step % 17) as i64;
                    cur.a.push(x);
                    m.insert_a(x);
                    mutations += 1;
                }
                1 => {
                    if let Some(x) = cur.a.pop() {
                        m.delete_a(x);
                        mutations += 1;
                    }
                }
                2 => {
                    let x = 5 + (step % 11) as i64;
                    cur.b.push(x);
                    m.insert_b(x);
                    mutations += 1;
                }
                3 => {
                    if !cur.a.is_empty() {
                        let i = (step as usize) % cur.a.len();
                        let old = cur.a[i];
                        let new = old + 2;
                        cur.a[i] = new;
                        m.update_a(old, new);
                        mutations += 1;
                    }
                }
                _ => {
                    let _ = inseparable_pair_sum(&Group {
                        a: cur.a.iter().take(6).copied().collect(),
                        b: cur.b.iter().take(6).copied().collect(),
                    });
                }
            }
        }
    }
    eprintln!("factorized correctness: {states} states, {mutations} mutations, 0 mismatches");
    Ok(())
}

pub fn run(quick: bool) -> Vec<Record> {
    let mut out = Vec::new();
    let sizes: &[(usize, usize)] = if quick {
        &[(1, 200), (1, 800), (1, 2_000)]
    } else {
        &[
            (1, 200),
            (1, 800),
            (1, 2_000),
            (1, 5_000),
            (1, 8_000),
            (8, 2_000),
            (1, 50_000),
            (1, 500_000),
            (1, 5_000_000),
        ]
    };

    for &(groups, per) in sizes {
        let data = gen_groups(groups, per, 11);
        let n = (groups * per) as u64;
        let pair_ops = groups as u64 * per as u64 * per as u64;
        let fact_ops = groups as u64 * per as u64 * 2;

        if pair_ops <= 80_000_000 {
            let (val, times, reps) =
                time_ns(2, 8, || data.iter().map(naive_pair_sum).sum::<i128>());
            let _ = val;
            out.push(record(
                "factorized",
                "naive_pairs",
                n,
                json!({"groups": groups, "per": per, "pairs": pair_ops}),
                times,
                reps,
                pair_ops,
                pair_ops * 16,
                json!({"result": val}),
                "materializes every matching pair",
            ));

            let (val2, times, reps) =
                time_ns(2, 8, || data.iter().map(hashjoin_style_sum).sum::<i128>());
            out.push(record(
                "factorized",
                "hashjoin_then_pairs",
                n,
                json!({"groups": groups, "per": per}),
                times,
                reps,
                pair_ops,
                pair_ops * 16,
                json!({"result": val2}),
                "join matching does not remove the product expansion",
            ));
        }

        let (val, times, reps) = time_ns(3, 12, || data.iter().map(factorized_sum).sum::<i128>());
        out.push(record(
            "factorized",
            "factorized_sums",
            n,
            json!({"groups": groups, "per": per, "pairs_avoided": pair_ops}),
            times,
            reps,
            fact_ops.max(1),
            n * 8 * 2,
            json!({"result": val}),
            "sum(A)*sum(B) per group; pairs never exist",
        ));

        let mut maint: Vec<Maintained> = data.iter().map(Maintained::from_group).collect();
        let (val, times, reps) = time_ns(4, 20, || {
            let mut acc = 0i128;
            for m in &mut maint {
                m.insert_a(7);
                acc += m.result();
            }
            acc
        });
        out.push(record(
            "factorized",
            "incremental_insert_a",
            n,
            json!({"groups": groups, "per": per}),
            times,
            reps,
            groups as u64,
            groups as u64 * 32,
            json!({"result": val}),
            "insert x into A is O(1) per group: sum_a += x",
        ));
    }

    // Negative control: inseparable aggregate, small n only.
    let g = gen_groups(1, if quick { 400 } else { 1_500 }, 3);
    let pair_ops = (g[0].a.len() * g[0].b.len()) as u64;
    let (val, times, reps) = time_ns(2, 6, || inseparable_pair_sum(&g[0]));
    out.push(record(
        "factorized",
        "inseparable_negative_control",
        g[0].a.len() as u64,
        json!({"pairs": pair_ops}),
        times,
        reps,
        pair_ops,
        pair_ops * 24,
        json!({"result": val}),
        "XOR coupling prevents factorization; expansion is required",
    ));
    out
}
