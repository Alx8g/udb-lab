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
machine, and includes a case that should lose.

## Hardware for the numbers in `.working/results`

- 12th Gen Intel Core i9-12900H, 14 cores / 20 threads
- 32 GB DDR4-3200
- NVMe SSD
- rustc 1.96.0, `--release`, thin LTO

## Run

```
cargo test --release
cargo run --release -- --quick
cargo run --release
cargo run --release -- --only factorized
python scripts/summarize.py .working/results/full/records.json
```

Results land in `.working/results/{quick,full}/`.

## Kernels

| Module | Mechanism | What would kill it |
|---|---|---|
| `factorized` | `sum(A)*sum(B)` and O(1) incremental inserts | non-separable aggregates |
| `answer_cells` | lower envelope of parameterized rankings + drift certificates | near-ties, large drift |
| `executable` | bulk `+K` as one group adjustment | point lookups, unindexed filters, dense exceptions |
| `progressive` | 16-bit bounds, residual only on the decision boundary | all values in one bucket |
| `certificate` | empty join from two extrema; galloping for sparse overlap | interleaved empty; dense overlap |
| `wcoj` | neighbor-list triangle intersection vs two-hop expansion | graphs where AGM ≈ input |
| `coordination` | local reservation quotas vs one CAS counter | leftover quota / false sold-out |
| `redundancy` | `C=A+B` as recovery and as `A+B` projection | predicates on A alone |
| `pgm` | piecewise-linear CDF + bounded correction | hash-map specialist; wild clustering |
| `engine` | row vs column vs AVX2; 4k batched lookup; payload bound | network payload, cold random I/O |
| `shared` | one sorted index for N parameterized range counts | private per-query match lists |
| `joint` | executable prices + ranking cells: bulk adj cannot change the winner | irregular per-row jitter |
| `correlation` | same linear model for storage and prune | shuffled / uncorrelated columns |

Correctness is exact. A faster approximate path is treated as a failure.
