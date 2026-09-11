# Thoughts after measuring the kernels

The research note asked for a leap: stop doing work that the architecture
does not need, then make leftover work fast, and design those two levels
together. The lab is not a database. It is a set of isolated, exact
kernels with a specialist or naive baseline and a case that should lose.

The numbers change what I would bet on.

## The unit of work is the decision, not the row

Every leap had the same shape. The expensive object never needed to exist.

- Pairwise products collapse to two sums.
- A ranking over 20k products collapses to a handful of envelope cells.
- A bulk `+= K` on 20M prices is one integer.
- An empty join of two sorted ranges is two extrema.
- A clustered `value > T` is a handful of block min/max values.
- A family of rounded affine totals is n, S, and 2q bins, not a rescan.

The failures had the opposite shape. We still touched every record, then
did extra work on top: a second array, a branch, a model, a lock.

Per-row 16-bit bounds lost to a tight u32 scan. That is the research
note's own warning, measured: "reading one tiny bound for each of three
billion records is still three billion accesses." Group bounds win.
Record bounds do not.

## Representation and certificate have to be the same object

The joint catalog is the only experiment that composes two mechanisms on
the same changing data.

Prices are `base + group_adj + exception`. Rankings are the lower envelope
of bases. A uniform bulk adjustment shifts every score by the same amount
and cannot change the winner. Caching materialized prices would
invalidate on every bulk write. Caching the envelope of bases does not.

Static: 2.445 s to 357 µs. Mixed bulk plus rare exceptions, after the
brute path stopped paying for an envelope it never used: 2.492 s to 299 ms
(8.3x). Irregular per-row jitter, where the identity is false: 2.23 s.

The mixed number is the one I would keep investigating. 8.3x including
rebuilds is a real combined-object win, not 13,917x of a static lookup.

## Exactness is non-negotiable and cheap when the identity is real

Ranking now uses integer millunits and smaller-index ties. Residue uses
Euclidean remainder and ties-to-even against a scalar oracle. 4k lookup
paths XOR the same payload, including duplicate keys. When the identity
was false, the numbers said so: near-tied answer cells rebuilt 12,000
times and lost; shuffled prefix blocks did not prune; dim-32 certificates
rejected 47 of 50 still-correct winners.

I would not relax exactness to chase a headline. The leap is that the
exact answer did not require the expanded object.

## The engine half is real, and smaller than the representation half

4,000 known ids against 20M dense keys, identical payload XOR: 12.1 µs
direct vs 1.60 ms binary search vs 20.0 ms merge-walk. Column layout beat
row layout (69 vs 98 ms). AVX2 lost to scalar on this scan (87 vs 69 ms).
Interleaving 32 independent pointer chains was 12.1x the serial batch
with the same 4.19 million follows.

So yes, remaining work still matters. Layout, covering keys, and not
returning 16 MB over 1 Gbit/s matter. They are 1.1x to 132x. The
representation identities are 10^2x to 10^4x on timed streams, and
"below timer resolution" on a bulk adj. Do not confuse them.

The 20 ms / 4,000 people example still splits. In RAM the lookup is
easy. The payload is 8 ms at 1 Gbit/s for 256-byte rows and 131 ms for
4 KiB rows. Fat results miss 20 ms no matter how good the engine is.
Cold NVMe random I/O was not measured.

## A specialist can beat a combined trick

Prefix SUM on dense unique keys is the cleanest example from the theory
package. Column layout already beat padded rows by 8x. Block summaries
then skipped clustered keys. Fenwick still won the recurring stream
(36 µs vs 723 µs column blocks clustered; 40 µs vs 295 ms shuffled
blocks). Construction is 7-33 ms and must be charged. That is the
compiler's job: admit the Fenwick-shaped state when the query family is
prefix sums on dense keys, and refuse to sell block summaries as a
general engine.

Residue histograms have the same shape. Versus scalar recomputation they
look like 8,230x. Versus a hand-maintained equivalent histogram they
would look like 1x plus compiler overhead. The identity is still worth
compiling. The control was honest and weak.

## Combining existing engines is not the leap

Jasper's 20-41% on TiDB is a real HTAP result and a different question.
It chooses partitions and column replicas inside an existing dual-format
system. That is adaptation of today's work. The kernels that leaped
removed the work.

I would still steal their controller idea: charge construction,
invalidation, and interference, and allow the answer "do not rebuild."
Answer cells already need that. Large drift plus near-ties made
maintenance more expensive than scanning.

## What I would build next, in order

1. A restricted algebra with three objects: a construction (how values
   exist), a certificate (what can be skipped), and an operator (what
   still runs). Admit a triple only if it beats the specialist including
   build and invalidation. Residue vs Fenwick vs envelope is the template.

2. Physical grouping. Block bounds only won when values were clustered.
   The layout is part of the certificate.

3. Cold I/O for the 4,000-id example. Until that is measured, the 20 ms
   claim is an in-RAM result plus a network bound.

4. An insert-friendly shared arrangement. `Vec::insert` is not one.

5. A binary join that actually emits the two-hop product, so WCOJ has a
   fair explosion baseline.

6. A residue control that is itself a maintained histogram, not scalar
   rounding. Until then, 8,230x is "vs the naive family," not vs the
   best equivalent state.

## What I would not spend the next month on

Per-row extra metadata as a scan accelerator. Automatic SIMD as a
strategy. Local quotas as a substitute for a network hop we did not
pay. Learned indexes as a default primary key. Answer cells on
near-tied, high-churn rankings. Selling block summaries as a replacement
for a prefix index.

Those are not empty ideas. They lost on this machine, against honest
baselines, with exact answers. That is enough to demote them.
