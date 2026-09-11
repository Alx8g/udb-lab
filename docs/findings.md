# Full kernel findings

Machine: Intel Core i9-12900H, 14c/20t, 32 GB DDR4-3200, NVMe.
Compiler: rustc 1.96.0, `--release`, thin LTO, codegen-units 1.
Correctness: all 17 kernels exact against independent reconstruction.
Run: `cargo run --release`, 217 records, 445.4 s wall. Answer-cell mixed
workloads were remeasured after an interior-gap fix and merged into the
same JSON.

This is not a database ranking. Isolated mechanisms, same process, same data.

The target is both halves, designed together: eliminate work that need not exist, and make remaining work fast. A mechanism that only wins by being approximate is a fail.

Raw data: [results/full/records.json](../results/full/records.json), [results/full/records.csv](../results/full/records.csv).
Protocol: [protocol.md](protocol.md). Thoughts: [thoughts.md](thoughts.md). Issues: [issues.md](issues.md).

Integer ranking uses millunits and weight `k/10000`, lower score then smaller index. Times below 1 µs are flagged `below_timer_resolution` and are not latency ratios.

## What actually leaps

| Mechanism | Workload | Before | After | Speedup | Negative control |
|---|---|---|---|---|---|
| Factorized join-aggregate | 8,000 x 8,000 pairs | 25.3 ms naive pairs | 4.5 µs `sum(A)*sum(B)` | 5,624x | XOR-coupled aggregate still 2.66 ms |
| Answer cells (static) | 20k products, 12k weights | 295-626 ms scan | 126-228 µs cell lookup | 2,334-4,953x | - |
| Answer cells (small drift, pareto) | same + 5% tiny updates | 295 ms | 240 µs, hit_rate=1.0, 0 rebuilds | 1,231x | near-tie: 23.8 s, 12,000 rebuilds |
| Executable bulk update | 20M prices `+= 100` | 31.2 ms N writes | <1 µs one adj | below timer | point lookup 14 vs 17 µs; filter 20.5 ms |
| Block bounds (clustered) | 20M `value > T` | 10.7-12.0 ms full scan | 26.1 µs | 411-460x | spread still prunes some blocks here (100-122x), not a kill |
| Certificate empty join | 2M vs 2M disjoint ranges | 2.54 ms merge | two extrema, <1 µs | below timer | dense overlap: gallop 15.7 vs merge 17.4 ms |
| Galloping intersection | 2M vs sparse | 1.90 ms merge | 8.3 µs gallop | 229x | dense overlap ~1.1x |
| Joint catalog | 60k products, 15k rankings | 2.445 s brute | 357 µs cells over bases | 6,846x | irregular per-row jitter: 2.23 s rebuild |
| Joint mixed (corrected baseline) | bulk every 20, exception every 400 | 2.492 s brute, no envelope | 299 ms joint | 8.3x | exceptions rebuild; identity still saves the bulk half |
| Correlation block bounds | 8M sorted, tight noise | 3.80 ms column scan | 11.4 µs block min/max | 333x | shuffled: 5.06 vs 4.76 ms, no win |
| Shared index vs rescan | 500k keys, 1024 ranges | 498 ms rescan | 525 ns shared | below timer on shared | private lists faster but duplicate RAM |
| 4k dense PK lookup | 20M ids, same XOR payload | 1.60 ms 4k bsearch | 12.1 µs direct | 132x | merge-walk of id column 20.0 ms, worse |
| Residue histogram q=100 | 1,048,576 values, 48 queries, 192 updates | 453 ms scalar round-even | 55 µs stream | 8,230x | unmaintained `x>T` still scans (0.50 ms) |
| Residue histogram q=10,000 | same stream | 445 ms | 4.43 ms | 101x | O(q) query cost shows up |
| Dependent-read interleave | 4,194,304 logical follows | 575 ms width 1 | 47.6 ms width 32 | 12.1x | width 64 is 48.0 ms, no further gain |
| Fenwick prefix (clustered) | 2,097,152 dense keys, 48 prefix sums | 56.8 ms column scan | 36.2 µs Fenwick | 1,568x | shuffled blocks lose; Fenwick still 39.7 µs |

## What does not leap, and why

**Per-row 16-bit progressive bounds lost.** Full u32 scan of 20M is still cheaper than an extra array plus a branch. Group min/max is the version that wins.

**AVX2 column scan lost to scalar.** Row 97.7 ms, scalar SoA 69.1 ms, AVX2 87.0 ms. Layout is the engine win. SIMD paid for widening and lost.

**Learned index is not free, and hash still wins on messy keys.** Same qualitative result as the previous run.

**Local quotas are not a 10x transaction story on this box.**

**Queryable redundancy is operator-specific.** Sum of A+B from C: 5.03 vs 11.0 ms (2.2x). Filter on A alone: C does not help.

**WCOJ vs this binary plan is mixed.** The comparison plan already avoided the textbook two-hop explosion.

**Answer-cell maintenance can exceed the original query.** Pareto + large drift: 1.47 s vs 295 ms brute (600 envelope rebuilds). Near-ties: 23.8-23.9 s, hit_rate=0, 12,000 rebuilds. The certificate is only cheap while the gap survives.

**Shared state trades query time for update time.** Private match-list counts remain faster than a shared binary search. Inserting into a `Vec` is still O(n).

**Block summaries lose when keys are shuffled.** Clustered row scan 448 ms vs row blocks 281 µs. Shuffled row blocks 623 ms, worse than the 584 ms row scan. Column blocks shuffled 295 ms vs column scan 269 ms: extra metadata, no prune.

**Fenwick is the specialist, and it wins the recurring stream.** Clustered Fenwick 36.2 µs vs column blocks 723 µs. Construction is 7.29 ms clustered and 32.8 ms shuffled, charged separately. A one-shot query does not pay that tax for free.

**Ranking certificates get conservative in high dimension.** 4,096 candidates, residual 100, query L1 0.05 of 1e6. Ordinary vs simplex-tight certified / 50, actual unchanged / 50: dim 2 = 48/48, 49; dim 8 = 17/17, 38; dim 32 = 0/3, 31. Zero false certifications. Simplex helped only at dim 32 on this sample.

## Design-together result

The joint catalog is the experiment that composes two mechanisms on the same data:

- Prices are `base + group_adj + exception` (executable region).
- Rankings are the lower envelope of bases (answer cells), integer millunits, smaller-index ties.
- A uniform bulk adjustment shifts every score by `k*adj` and cannot change the winner.

Static joint: 2.445 s to 357 µs (6,846x), 2 cells.

Mixed, after the baseline stopped rebuilding the envelope it never used: 2.492 s to 299 ms (8.3x). Previously the brute path paid for `set_exception()` envelope rebuilds. The 8.3x is the number that survives that correction. Exceptions still force a rebuild. The identity still saves the bulk half.

Irregular per-row price jitter: 2.23 s. Sharing the representation does not help when the identity is false.

## Theory-package kernels (this round)

Residue, chase, prefix, and ranking certificates are the mechanisms from
`database_theory_research_package.zip`, reimplemented in this lab and
measured on the same machine as the rest of the suite.

**Rounded affine family.** Count and sum are not enough: `[0,2]` and `[1,1]`
share them, `round_even(x/2)` sums to 1 vs 0. A histogram modulo `2q` plus
n and S answers every integer `(p,b)` at fixed q. Stream at q=100: 453 ms
to 55 µs. Build is 6.87 ms, so construction-plus-stream is about 65x, not
8,230x. At q=10,000 the histogram is 160 KB and the stream is 4.43 ms
(101x vs scalar, 36x including build). Original values stay in RAM. An
unmaintained `x>T` still scans.

**Dependent reads.** 65,536 chains, depth 64, 128 MiB link array. Width 32
is 12.1x the serial batch. No logical follow is removed. Width 64 does not
help further (47.6 vs 48.0 ms).

**Prefix SUM on dense unique keys.** Layout is the first cut (column vs
row, 56.8 vs 448 ms clustered). Block summaries then skip clustered keys
(row blocks 281 µs). Fenwick is 36.2 µs clustered and 39.7 µs shuffled.
The theory paper's warning holds: an established prefix index beats the
layout-plus-summary trick on this workload, and shuffled data kills the
summary.

**Certificates.** The three-product integer example certifies the middle
winner across 15,625 residual states with zero false positives. Random
coverage at dim 32 certifies 3/50 even with the simplex bound, while the
winner actually stayed in 31/50. Failure to certify is not a wrong answer.

## 4,000 people / 20 ms

In-process, dense primary key, 20M rows, 4,000 known ids, same XOR of a
`u32` payload, missing=0, duplicate requests independent: 12.1 µs median.
Independent binary search: 1.60 ms. Sorted merge of the id column: 20.0 ms.

Network payload, independent of the engine:

- 4,000 x 256 B = 1.024 MB -> 8.19 ms ideal at 1 Gbit/s, 0.82 ms at 10 Gbit/s
- 4,000 x 4,096 B = 16.4 MB -> 131 ms at 1 Gbit/s, 13.1 ms at 10 Gbit/s

20 ms for fat rows over 1 Gbit/s is a link bound, not an engine bound.
Cold 4K random I/O was not measured here (all in RAM).

## Paper / package numbers, used as context

The accompanying theory package measured a virtualized Linux host, GCC
14.2, pinned CPU. This lab is Windows, rustc, i9-12900H. Direction
matched. Absolute times did not, and should not be multiplied together.

- Package residue q=100: 78.47x construction-plus-stream vs scalar. Here 65x.
- Package chase width 32: 10.68x. Here 12.1x.
- Package Fenwick clustered stream: 28 µs vs our 36 µs. Same specialist win.
- Package certificate coverage dim 2/8/32 at eps=100, L1=0.05: 50/20/0 ordinary, 50/30/5 simplex. Here 48/17/0 and 48/17/3 over 50 trials.

No production database, billion-row, distributed, or crash-recovery claim.
