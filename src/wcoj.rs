//! Worst-case optimal triangle listing vs binary-join expansion.
//!
//! Query: R(a,b), S(b,c), T(a,c)
//! Binary plan R⋈S then ⋈T can produce Θ(n^2) intermediates on a dense
//! bipartite-ish instance. Leapfrog-style intersection of neighbor lists
//! is O(n^{1.5}) on the same graph (AGM bound).

use crate::stats::{record, time_ns, Record};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde_json::json;
use std::collections::HashSet;

type Adj = Vec<Vec<u32>>;

fn undirected_adj(n: usize, edges: &[(u32, u32)]) -> Adj {
    let mut adj = vec![Vec::new(); n];
    for &(u, v) in edges {
        if u == v {
            continue;
        }
        adj[u as usize].push(v);
        adj[v as usize].push(u);
    }
    for nbrs in &mut adj {
        nbrs.sort_unstable();
        nbrs.dedup();
    }
    adj
}

fn erdos_renyi(n: usize, p: f64, seed: u64) -> Vec<(u32, u32)> {
    let mut rng = SmallRng::seed_from_u64(seed);
    let mut e = Vec::new();
    for i in 0..n {
        for j in (i + 1)..n {
            if rng.gen::<f64>() < p {
                e.push((i as u32, j as u32));
            }
        }
    }
    e
}

/// Two cliques of size s sharing no vertices: triangles = 2 * C(s,3).
fn two_cliques(s: usize) -> Vec<(u32, u32)> {
    let mut e = Vec::new();
    for part in 0..2 {
        let base = (part * s) as u32;
        for i in 0..s {
            for j in (i + 1)..s {
                e.push((base + i as u32, base + j as u32));
            }
        }
    }
    e
}

/// Complete bipartite K_{d,d} plus a few closing edges: binary join blows up.
fn dense_bipartite_plus(d: usize, extra: usize, seed: u64) -> Vec<(u32, u32)> {
    let mut e = Vec::new();
    for i in 0..d {
        for j in 0..d {
            e.push((i as u32, (d + j) as u32));
        }
    }
    let mut rng = SmallRng::seed_from_u64(seed);
    for _ in 0..extra {
        let a = rng.gen_range(0..d) as u32;
        let b = rng.gen_range(0..d) as u32;
        if a != b {
            e.push((a, b));
        }
    }
    e
}

/// Node-iterator: each triangle counted once with u < v < w.
fn triangle_node_iter(adj: &Adj) -> u64 {
    let mut t = 0u64;
    for u in 0..adj.len() {
        let nu = &adj[u];
        for &v in nu {
            if (v as usize) <= u {
                continue;
            }
            let nv = &adj[v as usize];
            let mut i = 0usize;
            let mut j = 0usize;
            while i < nu.len() && j < nv.len() {
                match nu[i].cmp(&nv[j]) {
                    std::cmp::Ordering::Less => i += 1,
                    std::cmp::Ordering::Greater => j += 1,
                    std::cmp::Ordering::Equal => {
                        if nu[i] > v {
                            t += 1;
                        }
                        i += 1;
                        j += 1;
                    }
                }
            }
        }
    }
    t
}

/// Hash-based binary join with early filter still counts intermediates.
fn triangle_hash_pairs(adj: &Adj) -> (u64, u64) {
    // Build pair hash of existing edges, then expand two-hop paths.
    let mut edges = HashSet::new();
    for u in 0..adj.len() {
        for &v in &adj[u] {
            if (v as usize) > u {
                edges.insert((u as u32, v));
            }
        }
    }
    let mut t = 0u64;
    let mut inter = 0u64;
    for u in 0..adj.len() {
        for &v in &adj[u] {
            if (v as usize) <= u {
                continue;
            }
            for &w in &adj[v as usize] {
                if (w as usize) <= v as usize {
                    continue;
                }
                inter += 1;
                let (x, y) = if u as u32 <= w {
                    (u as u32, w)
                } else {
                    (w, u as u32)
                };
                if edges.contains(&(x, y)) {
                    t += 1;
                }
            }
        }
    }
    (t, inter)
}

pub fn correctness() -> Result<(), String> {
    let edges = two_cliques(8);
    let adj = undirected_adj(16, &edges);
    let w = triangle_node_iter(&adj);
    let expected = 2 * (8 * 7 * 6 / 6);
    if w != expected {
        return Err(format!("two cliques: got {w} expected {expected}"));
    }
    let (b, _) = triangle_hash_pairs(&adj);
    if b != w {
        return Err(format!("binary {b} != wcoj {w}"));
    }
    let er = erdos_renyi(40, 0.2, 1);
    let adj = undirected_adj(40, &er);
    let w = triangle_node_iter(&adj);
    let (b, _) = triangle_hash_pairs(&adj);
    if w != b {
        return Err(format!("ER mismatch wcoj={w} hash={b}"));
    }
    eprintln!("wcoj correctness: clique and ER triangle counts match");
    Ok(())
}

pub fn run(quick: bool) -> Vec<Record> {
    let mut out = Vec::new();
    let graphs: Vec<(&str, usize, Vec<(u32, u32)>)> = if quick {
        vec![
            ("two_cliques_20", 40, two_cliques(20)),
            ("er_80_p0.05", 80, erdos_renyi(80, 0.05, 2)),
            ("bipartite_80", 160, dense_bipartite_plus(80, 40, 3)),
        ]
    } else {
        vec![
            ("two_cliques_40", 80, two_cliques(40)),
            ("two_cliques_70", 140, two_cliques(70)),
            ("er_200_p0.03", 200, erdos_renyi(200, 0.03, 2)),
            ("er_300_p0.02", 300, erdos_renyi(300, 0.02, 4)),
            ("bipartite_150", 300, dense_bipartite_plus(150, 60, 3)),
            ("bipartite_250", 500, dense_bipartite_plus(250, 80, 5)),
        ]
    };

    for (name, n, edges) in graphs {
        let adj = undirected_adj(n, &edges);
        let m: u64 = adj.iter().map(|v| v.len() as u64).sum::<u64>() / 2;
        let (val, times) = time_ns(2, 5, || triangle_node_iter(&adj));
        out.push(record(
            "wcoj",
            &format!("leapfrog_{name}"),
            n as u64,
            json!({"edges": m, "triangles": val}),
            times,
            m,
            m * 16,
            json!({"triangles": val}),
            "intersect neighbor lists; AGM-style",
        ));

        // Binary expansion can explode; skip the largest if intermediates would be huge.
        let two_hop: u64 = adj
            .iter()
            .map(|nbrs| {
                nbrs.iter()
                    .map(|&v| adj[v as usize].len() as u64)
                    .sum::<u64>()
            })
            .sum();
        if two_hop < 80_000_000 {
            let (val, times) = time_ns(1, 3, || triangle_hash_pairs(&adj));
            out.push(record(
                "wcoj",
                &format!("binary_expand_{name}"),
                n as u64,
                json!({"edges": m, "two_hop": two_hop, "triangles": val.0}),
                times,
                two_hop.max(1),
                two_hop * 12,
                json!({"triangles": val.0, "intermediates": val.1}),
                "R⋈S materializes two-hop pairs then probes T",
            ));
        } else {
            out.push(record(
                "wcoj",
                &format!("binary_expand_{name}_skipped"),
                n as u64,
                json!({"edges": m, "two_hop": two_hop}),
                vec![0],
                two_hop,
                two_hop * 12,
                json!({"skipped": true}),
                "binary plan intermediates exceed 80M; skipped to keep the run bounded",
            ));
        }
    }
    out
}
