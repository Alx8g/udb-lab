# Issues encountered

Things that broke, lied, or wasted time while standing up the lab.
Each item is what happened, why it mattered, and what changed.

## Correctness bugs that would have produced fake speedups

**Ranking envelope used the upper hull.** The first `envelope` sorted
slopes increasing, which is the pointwise *max*. At `w=0` it picked the
wrong product. Correctness caught it: `winner mismatch mode=pareto w=0
cell=165 brute=7`. Fixed by sorting high slope first (min at small w)
and clipping hull segments that never win on `[0,1]`. Without that
check, answer cells would have looked brilliant and been wrong.

**Coded sum used an arithmetic shift for carry.** `i64 as i128 + i64`
then `(s >> 64) as i8` does not recover negative sums. Failed as
`coded sum mismatch`. Replaced with an exact `i128` parity block.
Queryable redundancy is only interesting if recovery and the query see
the same bytes.

**Triangle count divided by 3 after already requiring `u < v`.**
Overcount / undercount depending on the graph. Rewrote as a single
pass with `u < v < w`. Clique check: `2 * C(8,3) = 56`.

**PGM never split.** Fitting a line from segment start to the newest
endpoint always has zero error at that endpoint, so the greedy builder
emitted one segment for 20k keys. Lookups still worked (the correction
window became the whole array) and looked like a "learned index" that
was just binary search with extra indirection. Fixed by freezing the
slope from the first two keys. After the fix: 1 segment on linear
data, 106 on gapped, 93 on clustered (20k keys).

**AVX2 u16 compare treated values as signed.** High buckets compared
wrong. Biased with `i16::MIN` so `cmpgt` is unsigned. Same class of
bug later on the u32 column scan.

**AVX2 price sum accumulated in i32.** At 20M rows the running sum
overflows. Correctness on 5k rows did not catch it. Widened to i64
lanes via `cvtepu32_epi64`. That widening is also why the SIMD scan
lost to scalar: the "faster remaining work" paid for the conversion.

## Measurement bugs that would have produced fake speedups

**Timer reported 0 ns** for O(1) bodies (`compact_bulk_add`,
`incremental_insert_a`, `bound_certificate_disjoint`). Windows
`Instant` granularity hid the cost. `time_ns` now repeats until a
sample is at least 1 µs, then divides. Some results still print 0
because the divided sample rounds down. Treat those as "below timer
resolution," not zero work.

**`summarize.py` paired the first `full_scan_clustered` with the first
`block_bounds_clustered` regardless of threshold.** Four thresholds
collapsed into one bogus speedup line. The findings tables were built
from the JSON, not from that pairing. Do not trust the script for
multi-threshold variants without grouping on `params`.

**Quick run mixed updates inside the timed closure without reset.**
Answer-cell mixed workloads accumulated drift across iterations, so
later samples were not the same work. Reset certified state each
iteration.

**`batch_merge` sorted keys inside the timed path.** Compared a lookup
plus a sort against binary search. Sort moved out of the timer. After
the fix, merge-walk of a 20M id column is *slower* than 4k independent
bsearches (17.4 ms vs 1.27 ms), which is the honest result: you still
walk the large array.

**Shared-state insert timed `Vec` rebuild from scratch** in an early
draft. That measured allocation, not the incremental insert. Current
numbers time `Vec::insert` (O(n), 1.0 ms at 500k), which is still a
bad shared arrangement, and now honestly so.

**Executable-region `sum` scanned `birth_adj` every time** in the first
draft, so the compact form was not O(1). Maintained `birth_sum`.

**WCOJ "binary expand" did not emit the two-hop product.** It counted
intermediates after a `u < v < w` filter, so the textbook explosion
never appeared on bipartite graphs and WCOJ looked worse. That is a
real finding about the comparison plan, and also a hole: we still owe
a baseline that materializes R⋈S fully.

## Implementation issues that were not bugs, just friction

**First compile failed on `//!` in the middle of `progressive.rs`.**
Inner doc comments are only legal at the start of a module. One line.
Rustc was right.

**`.working/` is gitignored.** Full JSON and findings lived only on
disk after the first push. The public repo looked like source with a
summary table and no evidence. That is why this docs/results push
exists.

**Cargo test and `cargo run --release` compiled twice** because tests
use a different crate hash (`--cfg test` / harness). Not wrong, just
slow on a cold cache.

**`cargo` first build pulled rayon, then it was removed.** Dead
dependency from an earlier parallel plan. Coordination uses
`thread::scope` instead.

**escli fetch of PDFs returned HTML abstracts** more than once.
Downloaded arXiv PDFs with curl into `.working/papers/` (gitignored,
8.5 MB). Paper claims used as context were not reproduced.

**Windows console encoding** (`cp1252`) crashed a print of `≈` in a
summarize script. Findings were written in ASCII.

**Quota kernel used a Mutex per shard.** That serialized the "local"
path and lost to a single CAS. Thread-local `Shard` without the mutex
is the version in the full run (1.48×, not 10×). The mutex version
would have been a fake loss.

**Answer-cell mixed large-drift rebuilds dominate.** 600 envelope
rebuilds on 20k products took ~2 s, worse than brute. Easy to report
only the static lookup. The protocol requires charging invalidation.
We did, and the mechanism failed that test on near-ties.

## What was out of scope and would mislead if ignored

- No durability, WAL, or crash recovery.
- No network, so coordination does not pay the hop it exists to avoid.
- All timed data in RAM. The 20 ms / 4k-id example is not an I/O result.
- No SQL parser, optimizer, or concurrency control beyond the quota CAS.
- Negative controls that *should* lose did lose. If a future run makes
  them win, the kernel is wrong or the control is no longer negative.

## Reproduction

```
cargo test --release
cargo run --release
python scripts/summarize.py results/full/records.json
```

Hardware for checked-in JSON: Intel Core i9-12900H, 32 GB, Windows 11,
rustc 1.96.0. 190 records, 120.2 s wall.
