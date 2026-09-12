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

The target is one logical database with a compact shared core and selectively
activated physical and execution experts, not one universal AVL tree. Ordinary
state should be implicit where possible, deviations sparse, and derived state
retained only while its measured savings repay construction and maintenance.

The native benchmark does not yet exercise that integrated architecture. The
specialist kernels remain in the separate laboratory executable. Native cache
and grouped-write switches are limited experiments, not an automatic expert
controller. SQLite's lead is a real gap in the current implementation, neither
proof against the research nor something expert routing alone will fix.

The next storage integration combines scan-friendly pages, implicit extent
mapping and bounded sparse updates under the existing logical transaction and
recovery contract. Preserve the AVL control while measuring the replacement.
Expert eligibility, snapshot coverage, admission and retirement must then share
that foundation. Compare total latency, CPU, memory, storage and lifecycle costs,
including structureless workloads, rather than multiply isolated kernel wins.
[The support ledger](docs/track-a-status.json) separates this target from what runs.

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
Compaction copies a pinned root outside the foreground state mutex, then validates
that root before publication. A concurrent commit invalidates the candidate and
returns `Conflict`; retry is explicit. Publication still holds the state mutex
while syncing the manifest. Whole-epoch retention is a conservative baseline,
not Round 9's fine version-interval page reclamation.
Cache retirement drops container allocations, not just their entries. Pinned
historical snapshots remain readable without repopulating retired caches. Cache,
staging and scan budgets are accounting estimates, not a whole-machine RAM limit. Windows uses file flushes and write-through manifest replacement.
Neither that path nor the Unix sync/rename path has been device-power-loss tested.

Small arena records now share a writer-private 64 KiB append buffer. The buffer
flushes before the unchanged data and manifest sync barriers. Set
`Options::append_buffer` to `false` for the direct-write control. Buffering changes
write-call count, not arena bytes or the persistent format.

`Options::value_cache` is an opt-in experiment and defaults to `false`.
When enabled, point reads can admit immutable values up to 4 KiB into the same
charged budget as index nodes. A value's allocation capacity plus its metadata
charge must fit within one-eighth of that budget. Scans and compaction can reuse
admitted values but do not populate the value cache. `verify()` bypasses both
caches. Retired epochs drop both maps and the eviction queue, and returned values
are independent buffers. This does not bound total RSS or the OS file cache.

The comparison runner names this experimental path `spi-value-cache`. The `spi`
path keeps value admission disabled, and `spi-unbuffered` additionally disables
append buffering. Use these controls rather than comparing new results against
an older executable. Repeated-key, unique-key, and one-off-scan phases distinguish
reuse savings from admission overhead.

The committed-source campaign used three seeds and twelve engine trials per
cache setting. At 8 MiB, repeated 16-key reads were 0.4 microseconds with value
admission versus 4.0 microseconds without it. The 0.4-microsecond sample is below
this lab's 1-microsecond reporting threshold; do not quote a precise speedup
ratio from that number. Unique reads were 16.84 ms versus
17.16 ms, one-off scans were 16.01 ms versus 16.52 ms, and durable single-row
updates were 9.31 ms versus 9.16 ms. At a 1 KiB cache, admission correctly stayed
off and the opt-in path was slightly slower. The result supports keeping value
admission opt-in, not enabling it globally. [Normal results](results/spi/value-cache-normal-committed-v1/summary.json)
and [small-cache results](results/spi/value-cache-small-committed-v1/summary.json) are
source-pinned and fully output-checked.

[The measured buffer comparison](results/spi/append-buffer-v2/summary.json),
[buffer small-cache control](results/spi/append-buffer-small-cache-v2/summary.json),
and the value-cache results retain complete samples, source identities, and output checks. SQLite still wins most
operations. These are local KV experiments, not production latency or power-loss
validation. [Track A remains unfinished](docs/track-a-status.json).

Grouped copy-on-write is a separate opt-in experiment, disabled by default.
With `Options::grouped_updates`, a multi-key insertion batch into an empty tree
builds a balanced tree directly. Replacement-only batches preserve topology and
copy each affected ancestor once. Unchanged keys retain their logical revisions.
Single-key writes, deletes, mixed structural batches, and batches over the scratch
limit use the original sequential path. Eligibility checks add reads before
replacement and fallback work, so fewer bytes need not mean faster commits.

The temporary borrowed-entry array is limited to 64 KiB and one eighth of the
transaction budget. This is additional to staging charge, excludes recursion and
allocator overhead, and is not a process-memory guarantee. Payloads are borrowed,
not cloned into the array. Allocation failure falls back to sequential updates.
The record format and publication barriers are unchanged, but bulk construction
changes topology and serialized bytes. `spi-grouped` selects this experiment in
the comparison runner with value admission disabled. Per-phase arena counters
measure application-written bytes, not filesystem or device write amplification.
The [normal-cache campaign](results/spi/grouped-normal-committed-v1/summary.json)
and [1 KiB campaign](results/spi/grouped-small-committed-v1/summary.json) each
passed nine trials from the same clean committed source, using 2,000 rows,
64-byte values, three seeds and rotating engine order. Median update-phase arena
bytes fell from 451,584 to 221,424, a 51% reduction. Total pre-maintenance arena
bytes fell 14%. These counters exclude manifest writes and device effects.

At 8 MiB, median time for eight update batches fell from 101.97 ms to 87.29 ms.
At 1 KiB, it fell from 174.38 ms to 164.42 ms. Normal-cache load time was nearly
unchanged at 186.79 ms versus 185.23 ms, since only the first insertion batch
qualifies for bulk construction. Normal-cache single-row commit medians were
9.76 ms versus 9.77 ms, and ranges were 712.2 versus 724.0 microseconds.
The single-key control still uses sequential updates, but prior bulk construction
changes topology, so later path lengths can differ.

SQLite remained faster: normal-cache update batches took 41.66 ms, single-row
commits 2.77 ms, and ranges 27.7 microseconds. Three local trials per engine do
not establish service latency, statistical significance or larger-than-RAM
behavior. These measurements support a narrow byte-reduction experiment, not a
general speedup or changing the default. Fine page reuse, durable overlays,
resource governance and device-power-loss validation remain open.

Run an isolated comparison into a new directory:

```
cargo build --release --locked --features rusqlite --bin spi-compare
uv run --no-project scripts/run_spi_campaign.py --binary target/release/spi-compare.exe --out .working/tmp/my-new-campaign --rows 2000 --engines spi spi-value-cache spi-unbuffered sqlite
```

On Unix omit `.exe`. The runner rejects stale source or missing engine adapters
before creating output. Build and test first, then benchmark without competing
local jobs. Never delete an existing evidence directory merely to reuse its name.

## Experimental packed storage

`Options::packed_pages = true` selects a new authoritative format at creation,
not a secondary copy of the AVL data. It stores sorted keys, per-key revisions
and small values in records of at most 16 KiB. Values over 1 KiB use immutable
overflow records. A fence-key AVL indexes pages instead of every individual key.
Small updates use exact deltas capped at eight records and 4 KiB total serialized
payload before consolidation. Splits use a streaming merge over staged writes.

Point reads search offsets in validated serialized pages. Scans merge a bounded
base/delta lineage. Both use the same transaction, snapshot, publication and
recovery code. Page caches share the existing charged cache budget with nodes
and values. Scans do not admit page records. Retired epochs drop all containers.
Temporary decoded pages, merge/output buffers and allocator overhead remain
separate from the cache charge. This is not a total-process RAM guarantee.

The manifest identifies packed stores as `SPIMETA2`. New binaries open either
format based on that persisted identity, regardless of creation options. Older
binaries reject packed manifests. There is no implicit in-place format conversion.
The CLI opens packed stores, but creating one currently requires the library or
the `spi-packed` comparison adapter. Legacy AVL remains the default/control.

This is an integrated physical storage path, not automatic mixture-of-experts
routing. Fixed-extent implicit mapping, fine physical reuse, expert admission
policy and typed analytics remain open. Empty internal fence pages are retained,
and current compaction does not merge them.

The [normal-cache results](results/spi/packed-normal-v1/summary.json),
[1 KiB results](results/spi/packed-small-v1/summary.json) and
[4 KiB-value results](results/spi/packed-large-values-v1/summary.json) passed all
33 committed-source trials. With 64-byte values and an 8 MiB cache, full scans
fell from 15.54 ms to 1.33 ms, load from 163.12 ms to 78.06 ms, and unique reads
from 15.97 ms to 1.77 ms versus legacy SPI. Arena writes fell from 2,585,984 to
495,120 bytes. Final logical file lengths were 193,600 bytes versus SQLite's
200,704 bytes. The reported 0.4-microsecond point median is below the lab's
1-microsecond reporting threshold and does not support a precise speedup ratio.

These results do not justify enabling packed storage by default. With a 1 KiB
cache, point medians regressed from 46.6 to 86.7 microseconds and update time
from 150.66 to 186.97 ms. With 4 KiB values, ranges regressed from 2.38 to 2.66 ms.
SQLite remained faster on general scans, ranges, updates and durable commits.
These are the pre-acceleration results, retained as negative controls. The paired
checksum campaign below addresses the tiny-cache latency regression without
changing persistent bytes. Across-workload CPU and total-memory superiority
remain unproven.

## Checksum implementation controls

The native checksum uses exact IEEE CRC32 with slicing-by-eight processing.
It preserves the polynomial, initial/final complement and serialized byte coverage.
The old bitwise implementation remains selectable with `spi-crc-bitwise` for
matched release controls. Both identify themselves in the comparison binary and
result record. This is not CRC32C and does not weaken integrity checks.

The slicing tables add 8 KiB of static read-only data per process and require no
per-call allocation. Tests compare independent zlib-generated vectors, all short
split/alignment cases and irregular fragments through 16 MiB. The
[paired release campaign](results/spi/packed-crc-paired-v1/summary.json) passed
54 trials from identical source bytes and 18 post-maintenance persistent-file
comparisons. Engine order rotates and checksum-build order alternates by seed.

At 1 KiB, packed point medians fell from 92.3 to 22.5 microseconds, unique reads
from 163.45 to 29.90 ms, and update batches from 200.12 to 125.79 ms. The sliced
legacy control took 48.9 microseconds for points and 162.26 ms for updates. With
normal cache, packed scans fell from 1.456 to 0.615 ms, close to SQLite's 0.617 ms
in that arm, and ranges fell from 117.0 to 47.3 microseconds. With 4 KiB values,
packed scans fell from 25.51 to 8.00 ms and ranges from 2.60 to 0.80 ms.

Not every phase improved. Normal-cache packed updates measured 87.11 versus
85.33 ms, and large-value single-row commits measured 9.53 versus 9.36 ms.
The SQLite negative controls also varied, so small changes are not evidence of
causality. SQLite still leads on general updates, durable commits and most ranges.
Three seeds do not establish statistical equivalence or service latency.

CRC acceleration changes compute cost, not page-miss read bytes or allocation
volume. The earlier diagnostic byte-amplification findings remain open. Packed
storage, grouped writes and value admission remain disabled by default. The
0.4-microsecond cached-point result is below the lab's reporting threshold and
must not be used for a precise latency ratio.

## Packed scan execution controls

Packed base pages can be scanned directly from validated serialized records.
The cursor seeks the lower key bound before allocating rows and checks the upper
bound on borrowed key bytes. Snapshot scan results take ownership of inline
values rather than copying them twice. Delta chains still use the bounded
materialized merge, without rereading their head record. Generation checks,
checksums, cache admission rules, output budgets and publication are unchanged.

`spi-scan-materialized` retains the materialized scan implementation as a build
control. Both modes report `packed_scan_implementation` in binary identity and
results. `scripts/run_spi_scan_campaign.py` compares identical committed source
and checksum implementations across normal cache, 1 KiB cache and 4 KiB values.
It measures all workload phases, adds fully checked post-mutation and
post-compaction scans, and requires identical persistent bytes between modes.
These new phases change the benchmark stream, so older campaigns remain
historical controls rather than interchangeable timing baselines.

This is not a zero-copy output API or automatic expert routing. Base pages still
require checked reads and output allocation. Delta merging and transactional
output assembly still allocate. The [paired scan campaign](results/spi/packed-direct-scan-paired-v1/summary.json)
passed 54 trials and 18 persistent-byte comparisons from identical committed source.
With 64-byte values and normal cache, packed ranges measured 22.6 versus 41.8
microseconds, full scans 0.398 versus 0.539 ms, and post-mutation scans 0.724
versus 1.005 ms for direct versus materialized execution. At 1 KiB, full scans
measured 0.432 versus 0.565 ms. Large-value scan improvements were small.

Not every phase improved. The 1 KiB packed update median increased from 116.98
to 122.56 ms and maintenance from 18.37 to 20.84 ms. Unchanged legacy and SQLite
controls also varied. Three seeds do not establish statistical equivalence or
an across-workload win.

All 18 [diagnostic trials](results/spi/packed-direct-profile-direct-base-normal-v1/summary.json)
passed their contracts. Against the [materialized control](results/spi/packed-direct-profile-materialized-normal-v1/summary.json),
normal packed full-scan allocation requests fell from 783,408 to 543,408 bytes
and allocation calls from 6,094 to 4,082. Range requests fell from 2,664,592 to
1,772,504 bytes. Read calls and bytes were identical for corresponding scan phases.
The 4 KiB-value full-scan request volume improved only from 4.295 to 4.239 MB.
These one-seed diagnostic measurements include harness allocations, are not
retained-memory or RSS measurements, and do not establish total CPU/RAM superiority.

## Bounded delta scan views

The `direct-delta` scan mode merges four-byte references into validated base and
delta records rather than building a map of copied row keys and values. Newer
updates replace older references and tombstones remove them. The full visible
page-size check remains in place even for a narrow query. Only returned rows
allocate their key/value payloads. The retained lineage is bounded by the existing
eight-delta and 4 KiB combined-delta limits plus one 16 KiB base record.

This is scan execution over the same authoritative bytes, not another stored
copy. Point reads, writes, consolidation and compaction keep their existing
algorithms. Scan-private references and retained records remain temporary memory,
not a claim about total RSS. Requested read calls and bytes must match the controls.

`spi-delta-materialized` selects the earlier `direct-base` mode. The original
`spi-scan-materialized` mode remains available and takes precedence if both flags
are set. The paired scan runner accepts an optional third `--delta` binary and
verifies source, checksum and execution-mode identity before output creation.
All three modes rotate through execution order and must produce identical
persistent bytes. Latency and allocation acceptance for delta views remain pending.

## Diagnostic attribution

The optional `spi-profile` build records phase-level storage work, nested wall
timers, process CPU time, Rust allocation requests and sampled process memory.
It does not change the persistent format or publication order. Normal builds
compile out detailed storage instrumentation and allocation tracking. The normal
campaign runner rejects profile binaries and diagnostic result records.

Build a diagnostic adapter separately before using `scripts/run_spi_profile.py`.
That runner requires clean committed source and labels every result diagnostic-only.
Its timings are not accepted speed measurements. CPU and allocation totals include
benchmark work. Rust allocator counts exclude SQLite's native allocations, and
process-memory samples include inputs and returned output buffers. Nested storage
timers overlap and must not be added as independent elapsed-time components.
The diagnostic runner is attribution machinery, not integrated expert routing.
The clean committed attribution campaigns measured approximately 8,000
positional reads, 352,000 bytes read and 4,000 checksums for one full SPI scan. The
1 KiB-cache load added approximately 77,000 positional reads. These are work
counters, not accepted performance timings. These results motivate packed live
pages for read amplification and a separate publication investigation for durable
single-row latency. No durability barriers have been removed.

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
