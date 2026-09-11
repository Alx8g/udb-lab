# UDB kernel laboratory

Measured tests of the mechanisms in
`Research Universal Database Architecture`.

The goal is both halves, designed together:

1. Eliminate work that does not need to exist.
2. Make the remaining work fundamentally faster.

Neither a caching trick on an ordinary engine, nor a fast engine that still
reads everything, is the target.

This repo is a kernel lab, not a database. Each experiment isolates one
mechanism, compares it against the strongest simple alternative on the same
machine, and includes a case that should lose. Correctness is exact.

## Hardware for the numbers below

- 12th Gen Intel Core i9-12900H, 14 cores / 20 threads
- 32 GB DDR4-3200
- NVMe SSD
- rustc 1.96.0, `--release`, thin LTO
- 190 records, 120.2 s full run

Raw JSON: `.working/results/full/records.json`
Write-up: `.working/results/full/findings.md`

## Run

```
cargo test --release
cargo run --release -- --quick
cargo run --release
cargo run --release -- --only factorized
python scripts/summarize.py .working/results/full/records.json
```

## What leaped (full run)

| Mechanism | Before | After | Speedup | Kill case |
|---|---|---|---|---|
| Factorized `sum(A)*sum(B)`, 8k×8k pairs | 26.9 ms | 4.7 µs | 5,724× | XOR-coupled aggregate still expands |
| Answer cells, 20k products × 12k weights | 140–260 ms | 24–60 µs | 2.3k–11k× | near-ties / large drift: 2.1 s, worse than brute |
| Compact bulk `+= K` on 20M prices | 31.3 ms | ~0 ns | ~10^7× | point lookup slightly slower; unindexed filter still O(n) |
| Block min/max, 20M clustered `> T` | 8–10 ms | 23–35 µs | 230–296× | unclustered: no prune, no win |
| Empty-join from two extrema, 2M+2M | 2.12 ms merge | ~0 ns | ~10^6× | interleaved empty: bounds overlap, merge 24.5 ms |
| Gallop intersection, sparse | 1.70 ms | 8.1 µs | 210× | dense overlap ~1.2× |
| Joint catalog (executable prices + ranking cells) | 1.165 s | 83.7 µs | 13,917× | irregular per-row jitter: 2.85 s rebuild |
| Correlation block bounds, 8M tight | 7.38 ms | 10.8 µs | 683× | shuffled column: no win |
| Shared index vs rescan, 1024 ranges | 424 ms | 130 µs | 3,267× | `Vec::insert` 1.0 ms; private lists faster but duplicate RAM |
| 4k dense PK lookup, 20M rows | 1.27 ms bsearch | 12.6 µs direct | 101× | merge-walk of id column 17.4 ms |

## What did not leap

- Per-row 16-bit progressive bounds: extra array + branch lost to a tight u32 scan (8–12 ms vs 14–65 ms).
- AVX2 column scan: 78 ms vs scalar SoA 59 ms vs row 83 ms. Layout beat SIMD.
- PGM vs HashMap on gapped/clustered keys: hash 18–20 ms, PGM 35–51 ms, binary 52–60 ms.
- Local reservation quotas vs one CAS: 48 ms vs 72 ms (1.48×), not a revolution on this box.
- Queryable `C=A+B`: 2.5× on `sum(A+B)`, zero help on predicates over A, 33.6 ms to repair A.
- WCOJ vs a filtered binary plan on bipartite graphs: binary was faster because it never emitted the two-hop product.

## 4,000 people / 20 ms

In RAM, dense id, 20M rows, 4,000 known keys: **12.6 µs**.

Payload, independent of the engine:

- 4,000 × 256 B → 8.19 ms ideal at 1 Gbit/s
- 4,000 × 4,096 B → 131 ms at 1 Gbit/s, 13.1 ms at 10 Gbit/s

Fat results over a slow link miss 20 ms no matter how good the lookup is.

## Research bet these numbers support

Keep compact exact constructions, certificates whose size tracks the decision
(not the table), shared maintained state, and identities that make a
certificate invariant under a class of writes.

Do not treat per-row extra metadata, automatic SIMD, or local quotas as the
lever. Answer cells are only cheap while the ranking gap survives.
