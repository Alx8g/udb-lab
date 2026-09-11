# Full kernel findings

Machine: Intel Core i9-12900H, 14c/20t, 32 GB DDR4-3200, NVMe.
Compiler: rustc 1.96.0, `--release`, thin LTO, codegen-units 1.
Correctness: all 13 kernels exact against independent reconstruction.
Run: `cargo run --release`, 190 records, 120.2 s wall.

This is not a database ranking. Isolated mechanisms, same process, same data.

The target is both halves, designed together: eliminate work that need not exist, and make remaining work fast. A mechanism that only wins by being approximate is a fail.

Raw data: [results/full/records.json](../results/full/records.json), [results/full/records.csv](../results/full/records.csv).
Protocol: [protocol.md](protocol.md). Thoughts: [thoughts.md](thoughts.md). Issues: [issues.md](issues.md).

## What actually leaps

| Mechanism | Workload | Before | After | Speedup | Negative control |
|---|---|---|---|---|---|
| Factorized join-aggregate | 8,000 × 8,000 pairs | 26.9 ms naive pairs | 4.7 µs `sum(A)*sum(B)` | 5,724× | XOR-coupled aggregate still 2.7 ms |
| Answer cells (static) | 20k products, 12k weights | 140–260 ms scan | 24–60 µs cell lookup | 2,300–11,000× | — |
| Answer cells (small drift, pareto) | same + 5% tiny updates | 140 ms | 162 µs, hit_rate=1.0 | 861× | near-tie / large drift: 2.1 s, worse than brute |
| Executable bulk update | 20M prices `+= 100` | 31.3 ms N writes | ~0 ns one adj | ~10^7× | point lookup 17 vs 19 µs (slightly slower); filter still 19.8 ms |
| Block bounds (clustered) | 20M `value > T` | 8.3–10.5 ms full scan | 23–35 µs | 230–296× | spread, no prune: 11.2 vs 11.9 ms, no win |
| Certificate empty join | 2M vs 2M disjoint ranges | 2.12 ms merge | two extrema, ~0 ns | ~10^6× | interleaved empty: merge 24.5 ms, bounds cannot help |
| Galloping intersection | 2M vs 200 sparse | 1.70 ms merge | 8.1 µs gallop | 210× | dense overlap: 17.5 vs 15.1 ms, ~1.2× |
| Joint catalog | 60k products, 15k rankings | 1.165 s brute | 83.7 µs cells over bases | 13,917× | irregular per-row jitter: 2.85 s rebuild |
| Correlation block bounds | 8M sorted, tight noise | 7.38 ms column scan | 10.8 µs block min/max | 683× | shuffled: 4.45 vs 4.35 ms, no win |
| Shared index vs rescan | 500k keys, 1024 ranges | 424 ms rescan | 130 µs shared | 3,267× | private lists 350 ns but duplicate RAM; Vec insert 1.0 ms |
| 4k dense PK lookup | 20M ids | 1.27 ms 4k bsearch | 12.6 µs direct | 101× | merge-walk of id column 17.4 ms, worse |

## What does not leap, and why

**Per-row 16-bit progressive bounds lost.** Full u32 scan of 20M: 8–12 ms. Scalar bucket pass: 18–65 ms. AVX2 bucket pass: 14–38 ms. One extra array plus a branch per row is more work than a tight column scan. The research claim only holds when the bound describes a group, not a record. Block min/max (1024-row) is the version that wins, and only when physical layout clusters values.

**AVX2 column scan lost to scalar.** Row 82.8 ms, scalar SoA 58.5 ms, AVX2 78.2 ms. Layout (AoS vs SoA) is the 1.4× engine win. The SIMD path paid for `cvtepu32_epi64` plus horizontal add and lost. Remaining-work redesign is real, but this particular kernel is not yet a specialist-beating scan.

**Learned index is not free, and hash still wins on messy keys.** 5M keys, 200k lookups: linear PGM 13.0 ms vs binary 51.5 ms vs hash 18.1 ms (PGM wins). Gaps: PGM 50.7, binary 56.8, hash 18.5. Clusters: PGM 35.3, binary 60.4, hash 18.5–19.6. Prediction may not decide existence (correctness held). It also does not beat a HashMap unless the CDF is almost linear.

**Local quotas are not a 10× transaction story on this box.** 16 threads, 400k stock, 1.28M attempts: global CAS 71.7 ms, quota 48.4 ms (1.48×). Restock size 1 vs 4096 did not matter. Oversell was zero. The expensive part of a real distributed reserve is the network hop this kernel never pays.

**Queryable redundancy is operator-specific.** Sum of A+B from C: 4.18 ms vs 10.3 ms from A and B (2.5×, one array vs two). Filter on A alone: C does not help (7.67 ms). Repair of A from B and C: 33.6 ms, so the extra block is not free.

**WCOJ vs this binary plan is mixed.** Two cliques: leapfrog 3–4× faster. Dense bipartite: the binary plan with `u < v < w` only produced ~8k–20k intermediates, not the 6–31M two-hop estimate, and was faster (1.75 vs 4.14 ms at 150; 15.7 vs 18.3 ms at 250). A hash-join that actually emitted every two-hop pair would look different. This kernel did not reproduce the textbook explosion because the comparison plan already avoided it.

**Answer-cell maintenance can exceed the original query.** Pareto + large drift: 1.84 s vs 140 ms brute (600 envelope rebuilds). Near-ties: even 0.002 drift rebuilds on 5% of queries, 2.08 s. The certificate is only cheap while the gap survives. Near-tied rankings are the kill case named in the research note, and they killed it.

**Shared state trades query time for update time.** Private match-list counts are O(1) and faster than a shared binary search. The shared array wins against rescan, and uses one copy of the keys. Inserting into a `Vec` is O(n) (1.0 ms at 500k). A real shared arrangement needs an insert-friendly structure.

## Design-together result

The joint catalog is the one experiment that composes two mechanisms on the same data:

- Prices are `base + group_adj + exception` (executable region).
- Rankings are the lower envelope of bases (answer cells).
- A uniform bulk adjustment shifts every score by the same amount and cannot change the winner.

Static joint: 1.165 s to 83.7 µs (13,917×), 2 cells.

Mixed (bulk every 20 queries, exception every 400): 1.551 s to 398 ms (3.9×). Exceptions force envelope rebuild. The identity still saves the bulk half.

Irregular per-row price jitter: 2.85 s. Sharing the representation does not help when the identity is false. Rebuild cost shows up in the open.

That is the architectural claim, measured: representation and certificate have to be the same object. Caching fully materialized prices would invalidate on every bulk adj. Caching the envelope of bases does not.

## 4,000 people / 20 ms

In-process, dense primary key, 20M rows, 4,000 known ids: 12.6 µs median. Independent binary search: 1.27 ms. The engine half of the example is easy once the id is a direct load.

Network payload, independent of the engine:

- 4,000 × 256 B = 1.024 MB → 8.19 ms ideal at 1 Gbit/s, 0.82 ms at 10 Gbit/s
- 4,000 × 4,096 B = 16.4 MB → 131 ms at 1 Gbit/s, 13.1 ms at 10 Gbit/s

20 ms for fat rows over 1 Gbit/s is a link bound, not an engine bound. Returning only requested fields is mandatory. Cold 4K random I/O was not measured here (all in RAM).

## Paper numbers we did not reproduce, used as context

- Jasper (TiDB, VLDB 2026 program): 20.43–40.59% lower workload completion time. Adaptation of existing dual-format storage, not a new unit of work.
- Blitzcrank: 85% less TPC-C memory, 19% lower throughput. Compression is a RUM trade, not a free lunch.
- Free Join: 2.94× geomean on JOB, single-thread in-memory. Our triangle kernel is smaller and mixed.
- F-IVM: up to two orders of magnitude vs other IVM in the authors' setting. Our factorized kernel is the identity they exploit, at 10^3–10^4× on the expanded product.

## What the evidence supports as the research bet

Keep:

1. Compact exact constructions (factorized aggregates, executable bulk ops).
2. Certificates whose size tracks the decision, not the table (envelope cells, range bounds, block min/max).
3. Shared maintained state across parameterized queries.
4. Joint identities: a representation that makes a certificate invariant under a class of writes.

Drop or bound:

1. Per-row extra metadata as a scan accelerator. It lost.
2. Treating SIMD as automatically faster. Layout beat SIMD here.
3. Assuming WCOJ always beats a filtered binary plan.
4. Assuming local quotas revolutionize uncontended CAS.
5. Answer cells on near-tied, high-churn rankings.

## Next measurements that would change the bet

1. Cold 4k lookup from NVMe: page grouping vs independent I/O. The 20 ms example is an I/O problem once RAM is gone.
2. A binary join that does emit the two-hop product, so WCOJ has a fair explosion baseline.
3. Answer-cell compiler for a larger operator set, with rebuild budget vs brute as an explicit planner choice.
4. Insert-friendly shared arrangements (not `Vec::insert`).
5. A scan kernel that beats scalar SoA on this CPU, or a documented reason it cannot.
6. The synthesis loop: given a restricted algebra, search for (representation, certificate, operator) triples and keep only those that beat the specialist including construction.
