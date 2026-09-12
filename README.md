# UDB kernel laboratory

Measured tests of the mechanisms in
`Research Universal Database Architecture`.

The goal is both halves, designed together:

1. Eliminate work that does not need to exist.
2. Make the remaining work fundamentally faster.

Neither a caching trick on an ordinary engine, nor a fast engine that still
reads everything, is the target.

This repo contains two layers:

1. The original kernel laboratory. Each experiment isolates one mechanism,
   compares it against a simple or specialist control, and includes a case
   that should lose. Historical results are not a correctness certificate.
   Known ranking and numeric edge cases remain in the separate lab branches.
2. The native SPI embedded database. It is a small durable binary key/value
   store and Track A substrate, not yet a general SQL database.

The database API supports arbitrary binary keys and values, ordered scans,
prefix scans, immutable snapshots, optimistic serializable transactions,
checksummed copy-on-write index records, manifest publication exercised by
process-crash tests, streaming epoch compaction, and explicit integrity verification.
Keys are limited to 4 KiB and values to 16 MiB. This is an experimental format.
Do not store irreplaceable data in this implementation.

It does not yet claim SQL, distributed transactions, full power-loss testing,
version-interval page reuse, or 200 TB operation under a 50 GB total-memory
limit. Those remain Track A work.

Public repo: https://github.com/Alx8g/udb-lab

## Hardware for the numbers below

- 12th Gen Intel Core i9-12900H, 14 cores / 20 threads
- 32 GB DDR4-3200
- NVMe SSD
- rustc 1.96.0, `--release`, thin LTO
- 190 records, 120.2 s full run

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

## Native SPI database

Initialize and use the embedded database with hexadecimal keys and values:

```
cargo run --release --bin spi -- init target/example-db
cargo run --release --bin spi -- set target/example-db 6b6579 76616c7565
cargo run --release --bin spi -- get target/example-db 6b6579
cargo run --release --bin spi -- scan-prefix target/example-db 6b
cargo run --release --bin spi -- verify target/example-db
cargo run --release --bin spi -- compact target/example-db
cargo run --release --bin spi -- collect target/example-db
```

Use `batch` for one durable transaction containing several operations. Its
JSON file uses `{"op":"set","key":"...","value":"..."}` or
`{"op":"delete","key":"..."}`. The library API is in
[`src/spi`](src/spi), and storage regressions are in
[`tests/spi-storage.rs`](tests/spi-storage.rs) and
[`tests/spi-process.rs`](tests/spi-process.rs).

The storage contract is deliberately narrow. One process owns a database at
a time. A failed write or publication poisons the handle and requires reopen.
Transactions validate logical key versions and a bounded logical mutation journal,
including predicate changes and missing-key insert/delete cycles. An old transaction
aborts when its validation history expires. Physical node offsets are not conflict
identities. Snapshots pin arena files and do not survive process restart.

The authoritative commit is a copy-on-write root, not a separate WAL. Recovery
opens the committed root without replaying a full log or rebuilding a key dictionary.
Compaction publishes a new arena before collection. Whole-epoch retention is a
conservative baseline, not Round 9's fine version-interval page reclamation.
Cache, staging and scan budgets are accounting estimates, not a whole-machine
RAM limit. Windows uses file flushes and write-through manifest replacement.
Neither that path nor the Unix sync/rename path has been device-power-loss tested.

Small arena records now share a writer-private 64 KiB append buffer. The buffer
flushes before the unchanged data and manifest sync barriers. Set
`Options::append_buffer` to `false` for the direct-write control. Buffering changes
write-call count, not arena bytes or the persistent format.

[The measured buffer comparison](results/spi/append-buffer-v2/summary.json) and
[small-cache control](results/spi/append-buffer-small-cache-v2/summary.json) retain
complete samples, source identities, and output checks. SQLite still wins most
operations. These are local KV experiments, not production latency or power-loss
validation. [Track A remains unfinished](docs/track-a-status.json).

Run an isolated comparison into a new directory:

```
cargo build --release --locked --features rusqlite --bin spi-compare
uv run --no-project scripts/run_spi_campaign.py --binary target/release/spi-compare.exe --out .working/tmp/my-new-campaign --rows 2000 --engines spi spi-unbuffered sqlite
```

On Unix omit `.exe`. The runner rejects stale source or missing engine adapters
before creating output. Build and test first, then benchmark without competing
local jobs. Never delete an existing evidence directory merely to reuse its name.

## Run

```
cargo test --release
cargo run --release -- --quick
cargo run --release
cargo run --release -- --only factorized
python scripts/summarize.py results/full/records.json
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
