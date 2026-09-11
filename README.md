# UDB kernel laboratory

Measured tests of the mechanisms in
`Research Universal Database Architecture` and the accompanying theory
package (`database_theory_research_package.zip`).

The goal is both halves, designed together:

1. Eliminate work that does not need to exist.
2. Make the remaining work fundamentally faster.

Neither a caching trick on an ordinary engine, nor a fast engine that still
reads everything, is the target.

This repo is a kernel lab, not a database. Each experiment isolates one
mechanism, compares it against the strongest simple alternative on the same
machine, and includes a case that should lose. Correctness is exact.
Ranking uses integer millunits and smaller-index ties. Times below 1 µs
are flagged and are not latency ratios.

Public repo: https://github.com/Alx8g/udb-lab

## Hardware for the numbers below

- 12th Gen Intel Core i9-12900H, 14 cores / 20 threads
- 32 GB DDR4-3200
- NVMe SSD
- rustc 1.96.0, `--release`, thin LTO
- 217 records, 445.4 s full run

## Docs and data

| What | Where |
|---|---|
| Findings (what leaped, what died) | [docs/findings.md](docs/findings.md) |
| Thoughts after measuring | [docs/thoughts.md](docs/thoughts.md) |
| Issues encountered along the way | [docs/issues.md](docs/issues.md) |
| Measurement protocol | [docs/protocol.md](docs/protocol.md) |
| Full run JSON | [results/full/records.json](results/full/records.json) |
| Full run CSV | [results/full/records.csv](results/full/records.csv) |
| Quick run JSON | [results/quick/records.json](results/quick/records.json) |

## Run

```
cargo test --release
cargo run --release -- --quick
cargo run --release
cargo run --release -- --only factorized
python scripts/summarize.py results/full/records.json
```

`--only NAME` writes `results/full/records.NAME.json` and does not replace
the full suite file.

## What leaped (full run)

| Mechanism | Before | After | Speedup | Kill case |
|---|---|---|---|---|
| Factorized `sum(A)*sum(B)`, 8k x 8k pairs | 25.3 ms | 4.5 µs | 5,624x | XOR-coupled aggregate still expands |
| Answer cells, 20k products x 12k weights | 295-626 ms | 126-228 µs | 2.3k-5.0kx | near-ties / large drift: 23.8 s, worse than brute |
| Compact bulk `+= K` on 20M prices | 31.2 ms | <1 µs | below timer | point lookup slightly slower; unindexed filter still O(n) |
| Block min/max, 20M clustered `> T` | 10.7-12.0 ms | 26.1 µs | 411-460x | shuffled correlation column: no win |
| Empty-join from two extrema, 2M+2M | 2.54 ms merge | <1 µs | below timer | dense overlap ~1.1x |
| Gallop intersection, sparse | 1.90 ms | 8.3 µs | 229x | dense overlap ~1.1x |
| Joint catalog (executable prices + ranking cells) | 2.445 s | 357 µs | 6,846x | irregular per-row jitter: 2.23 s rebuild |
| Joint mixed, brute without envelope | 2.492 s | 299 ms | 8.3x | exceptions still rebuild |
| Correlation block bounds, 8M tight | 3.80 ms | 11.4 µs | 333x | shuffled column: no win |
| 4k dense PK lookup, 20M rows, same XOR payload | 1.60 ms bsearch | 12.1 µs direct | 132x | merge-walk of id column 20.0 ms |
| Residue histogram q=100, 1M values | 453 ms scalar | 55 µs stream | 8,230x | build 6.87 ms; `x>T` still scans |
| Interleaved pointer-chases, 4.19M follows | 575 ms width 1 | 47.6 ms width 32 | 12.1x | width 64 does not help further |
| Fenwick prefix vs column scan, clustered | 56.8 ms | 36.2 µs | 1,568x | shuffled blocks lose; Fenwick still wins the stream |

## What did not leap

- Per-row 16-bit progressive bounds: extra array + branch lost to a tight u32 scan.
- AVX2 column scan: 87 ms vs scalar SoA 69 ms vs row 98 ms. Layout beat SIMD.
- PGM vs HashMap on gapped/clustered keys: hash still wins unless the CDF is almost linear.
- Local reservation quotas vs one CAS: about 1.5x, not a revolution on this box.
- Queryable `C=A+B`: 2.2x on `sum(A+B)`, zero help on predicates over A.
- WCOJ vs a filtered binary plan on bipartite graphs: binary was faster because it never emitted the two-hop product.
- Ranking certificates at dim 32: simplex-tight certified 3/50, actual winner unchanged 31/50, zero false positives.
- Block summaries on shuffled prefix keys: extra work, no prune.

## 4,000 people / 20 ms

In RAM, dense id, 20M rows, 4,000 known keys, identical payload XOR:
**12.1 µs**.

Payload, independent of the engine:

- 4,000 x 256 B -> 8.19 ms ideal at 1 Gbit/s
- 4,000 x 4,096 B -> 131 ms at 1 Gbit/s, 13.1 ms at 10 Gbit/s

Fat results over a slow link miss 20 ms no matter how good the lookup is.

## Research bet these numbers support

Keep compact exact constructions, certificates whose size tracks the decision
(not the table), shared maintained state, and identities that make a
certificate invariant under a class of writes. Compile residue histograms
and Fenwick-shaped state when the query family matches. Interleave remaining
dependent reads.

Do not treat per-row extra metadata, automatic SIMD, or local quotas as the
lever. Answer cells are only cheap while the ranking gap survives. Block
summaries are not a substitute for a prefix index.
