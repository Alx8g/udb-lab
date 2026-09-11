//! Exact rounded affine aggregates from a residue histogram.
//! Matches database_theory_round1/src/residue_lab.py and residue_bench.cpp.
//!
//! Q(p,b,q) = sum_i round_even((p * x_i + b) / q)
//! Maintain n, S, H[r] = #{x : x mod 2q = r}. Ties-to-even needs 2q.

use crate::stats::{record, time_ns, time_ns_setup, Record};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde_json::json;

fn euclid_div(num: i64, q: i64) -> (i64, i64) {
    let mut v = num / q;
    let mut r = num % q;
    if r < 0 {
        r += q;
        v -= 1;
    }
    (v, r)
}

fn round_even(num: i64, q: i64) -> i64 {
    let (v, r) = euclid_div(num, q);
    v + i64::from(2 * r > q || (2 * r == q && v % 2 != 0))
}

fn naive_sum(xs: &[i64], p: i64, b: i64, q: i64) -> i64 {
    xs.iter()
        .map(|&x| round_even(p.wrapping_mul(x).wrapping_add(b), q))
        .sum()
}

#[derive(Clone)]
struct Residue {
    q: i64,
    n: i64,
    sum: i64,
    hist: Vec<i64>,
}

impl Residue {
    fn new(q: i64) -> Self {
        Self {
            q,
            n: 0,
            sum: 0,
            hist: vec![0; (2 * q) as usize],
        }
    }

    fn from_values(q: i64, xs: &[i64]) -> Self {
        let mut s = Self::new(q);
        for &x in xs {
            s.add(x, 1);
        }
        s
    }

    fn add(&mut self, x: i64, m: i64) {
        self.n += m;
        self.sum += m * x;
        let two_q = 2 * self.q;
        let mut r = x % two_q;
        if r < 0 {
            r += two_q;
        }
        self.hist[r as usize] += m;
    }

    fn query(&self, p: i64, b: i64) -> i64 {
        let q = self.q;
        let mut residual = 0i128;
        let mut inc = 0i128;
        for r in 0..2 * q {
            let c = self.hist[r as usize] as i128;
            if c == 0 {
                continue;
            }
            let (z, t) = euclid_div(p.wrapping_mul(r).wrapping_add(b), q);
            residual += c * t as i128;
            inc += c * i128::from(2 * t > q || (2 * t == q && z % 2 != 0));
        }
        let numerator = p as i128 * self.sum as i128 + self.n as i128 * b as i128 - residual;
        (numerator / q as i128 + inc) as i64
    }
}

struct PolyState {
    q: i64,
    degree: usize,
    hist: Vec<i64>,
    moments: Vec<i128>,
}

impl PolyState {
    fn new(q: i64, degree: usize) -> Self {
        Self {
            q,
            degree,
            hist: vec![0; (2 * q) as usize],
            moments: vec![0; degree + 1],
        }
    }

    fn add(&mut self, x: i64, m: i64) {
        let two_q = 2 * self.q;
        let mut r = x % two_q;
        if r < 0 {
            r += two_q;
        }
        self.hist[r as usize] += m;
        let mut power = 1i128;
        for k in 0..=self.degree {
            self.moments[k] += m as i128 * power;
            power *= x as i128;
        }
    }

    fn eval(c: &[i64], x: i64) -> i64 {
        let mut y = 0i64;
        for &a in c.iter().rev() {
            y = y.wrapping_mul(x).wrapping_add(a);
        }
        y
    }

    fn query(&self, c: &[i64]) -> i64 {
        let q = self.q;
        let mut numerator = 0i128;
        for (ck, mk) in c.iter().zip(self.moments.iter()) {
            numerator += *ck as i128 * *mk;
        }
        let mut residue = 0i128;
        let mut up = 0i128;
        for r in 0..2 * q {
            let n = self.hist[r as usize] as i128;
            if n == 0 {
                continue;
            }
            let y = Self::eval(c, r);
            let (a, b) = euclid_div(y, q);
            residue += n * b as i128;
            up += n * i128::from(2 * b > q || (2 * b == q && a % 2 != 0));
        }
        ((numerator - residue) / q as i128 + up) as i64
    }
}

fn naive_poly(xs: &[i64], c: &[i64], q: i64) -> i64 {
    xs.iter()
        .map(|&x| round_even(PolyState::eval(c, x), q))
        .sum()
}

pub fn correctness() -> Result<(), String> {
    let a = vec![0i64, 2];
    let b = vec![1i64, 1];
    let qa = naive_sum(&a, 1, 0, 2);
    let qb = naive_sum(&b, 1, 0, 2);
    if qa == qb {
        return Err(format!("count+sum would collide but rounded {qa} vs {qb}"));
    }
    if Residue::from_values(2, &a).query(1, 0) != qa
        || Residue::from_values(2, &b).query(1, 0) != qb
    {
        return Err("histogram missed [0,2] vs [1,1]".into());
    }

    let mut rng = SmallRng::seed_from_u64(7);
    for q in [2i64, 3, 4, 5, 10, 100] {
        let mut xs: Vec<i64> = (0..400).map(|_| rng.gen_range(-5000..5000)).collect();
        let mut st = Residue::from_values(q, &xs);
        for step in 0..80 {
            let p = rng.gen_range(-20..21);
            let b = rng.gen_range(-50..51);
            let got = st.query(p, b);
            let exp = naive_sum(&xs, p, b, q);
            if got != exp {
                return Err(format!(
                    "affine mismatch q={q} step={step} p={p} b={b} got={got} exp={exp}"
                ));
            }
            match step % 3 {
                0 => {
                    let x = rng.gen_range(-5000..5000);
                    xs.push(x);
                    st.add(x, 1);
                }
                1 if xs.len() > 10 => {
                    let i = rng.gen_range(0..xs.len());
                    let old = xs[i];
                    let new = rng.gen_range(-5000..5000);
                    xs[i] = new;
                    st.add(old, -1);
                    st.add(new, 1);
                }
                _ if xs.len() > 10 => {
                    let x = xs.pop().unwrap();
                    st.add(x, -1);
                }
                _ => {}
            }
        }
    }

    for q in [4i64, 10] {
        let mut xs: Vec<i64> = (0..120).map(|_| rng.gen_range(-40..41)).collect();
        let mut st = PolyState::new(q, 2);
        for &x in &xs {
            st.add(x, 1);
        }
        for step in 0..40 {
            let c = [
                rng.gen_range(-3..4),
                rng.gen_range(-3..4),
                rng.gen_range(-2..3),
            ];
            let got = st.query(&c);
            let exp = naive_poly(&xs, &c, q);
            if got != exp {
                return Err(format!(
                    "poly mismatch q={q} step={step} got={got} exp={exp}"
                ));
            }
            let i = rng.gen_range(0..xs.len());
            let old = xs[i];
            let new = rng.gen_range(-40..41);
            xs[i] = new;
            st.add(old, -1);
            st.add(new, 1);
        }
    }
    eprintln!("residue correctness: affine + poly histograms match scalar round-even");
    Ok(())
}

struct Op {
    query: bool,
    a: i64,
    b: i64,
}

pub fn run(quick: bool) -> Vec<Record> {
    let mut out = Vec::new();
    let n = if quick { 50_000usize } else { 1 << 20 };
    let nq = if quick { 16usize } else { 48usize };
    let nu_per_q = 4usize;
    let mut rng = SmallRng::seed_from_u64(17113);
    let xs: Vec<i64> = (0..n)
        .map(|_| rng.gen_range(-1_000_000..=1_000_000))
        .collect();

    let mut ops = Vec::new();
    for _ in 0..nq {
        for _ in 0..nu_per_q {
            ops.push(Op {
                query: false,
                a: rng.gen_range(0..n as i64),
                b: rng.gen_range(-10..=10),
            });
        }
        ops.push(Op {
            query: true,
            a: rng.gen_range(-200..=200),
            b: rng.gen_range(-10_000..=10_000),
        });
    }

    for q in [100i64, 10_000] {
        let (val, times, reps) = time_ns_setup(
            1,
            if quick { 3 } else { 5 },
            || xs.clone(),
            |data| {
                let mut acc = 0i64;
                for o in &ops {
                    if o.query {
                        acc ^= naive_sum(data, o.a, o.b, q);
                    } else {
                        data[o.a as usize] += o.b;
                    }
                }
                acc
            },
        );
        out.push(record(
            "residue",
            &format!("scalar_recompute_q{q}"),
            n as u64,
            json!({"q": q, "queries": nq, "updates": nq * nu_per_q}),
            times,
            reps,
            (n * nq) as u64,
            (n * nq * 8) as u64,
            json!({"xor": val, "hist_bytes": 2 * q * 8}),
            "rescan and round every value for every (p,b)",
        ));

        let st0 = Residue::from_values(q, &xs);
        let (val, times, reps) = time_ns_setup(
            2,
            if quick { 4 } else { 8 },
            || (xs.clone(), st0.clone()),
            |(data, st)| {
                let mut acc = 0i64;
                for o in &ops {
                    if o.query {
                        acc ^= st.query(o.a, o.b);
                    } else {
                        let i = o.a as usize;
                        st.add(data[i], -1);
                        data[i] += o.b;
                        st.add(data[i], 1);
                    }
                }
                acc
            },
        );
        out.push(record(
            "residue",
            &format!("histogram_stream_q{q}"),
            n as u64,
            json!({"q": q, "queries": nq, "updates": nq * nu_per_q, "hist_bytes": 2 * q * 8}),
            times,
            reps,
            (nq as u64) * (2 * q as u64),
            (2 * q as u64) * 8,
            json!({"xor": val}),
            "O(q) query; original values retained and updated",
        ));

        let (val, times, reps) = time_ns(2, 6, || Residue::from_values(q, &xs).n);
        out.push(record(
            "residue",
            &format!("histogram_build_q{q}"),
            n as u64,
            json!({"q": q}),
            times,
            reps,
            n as u64,
            (n * 8) as u64,
            json!({"n": val}),
            "one pass to build n, sum, and 2q bins; charged separately",
        ));
    }

    let t = 0i64;
    let (val, times, reps) = time_ns(2, 6, || xs.iter().filter(|&&x| x > t).count());
    out.push(record(
        "residue",
        "unmaintained_filter_negative",
        n as u64,
        json!({"t": t}),
        times,
        reps,
        n as u64,
        (n * 8) as u64,
        json!({"count": val}),
        "histogram of x mod 2q does not answer x > T",
    ));
    out
}
