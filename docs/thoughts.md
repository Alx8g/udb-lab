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

Static: 1.165 s to 83.7 µs. Mixed bulk plus rare exceptions: 1.551 s to
398 ms. Irregular per-row jitter, where the identity is false: 2.85 s,
worse than brute.

That is the architectural claim I would keep. Not "one engine with many
indexes." A restricted algebra of constructions whose certificates are
invariant under named classes of writes.

## Exactness is non-negotiable and cheap when the identity is real

Every kernel was checked against an independent reconstruction. The
fast paths did not cheat. When the identity was false, the numbers
said so in the open: near-tied answer cells rebuilt so often they lost
to brute; shuffled correlation columns did not prune; interleaved empty
joins could not use range bounds.

I would not relax exactness to chase a headline. The leap is that the
exact answer did not require the expanded object.

## The engine half is real, and smaller than the representation half

4,000 known ids against 20M dense keys: 12.6 µs direct vs 1.27 ms binary
search. Column layout beat row layout (59 vs 83 ms). AVX2 lost to scalar
on this scan (78 vs 59 ms). HashMap beat a piecewise-linear index unless
the keys were almost linear.

So yes, remaining work still matters. Layout, covering keys, and not
returning 16 MB over 1 Gbit/s matter. They are 1.4× to 100×. The
representation identities are 10^3× to 10^7×. Do not confuse them.

The 20 ms / 4,000 people example splits cleanly. In RAM the lookup is
easy. The payload is 8 ms at 1 Gbit/s for 256-byte rows and 131 ms for
4 KiB rows. Fat results miss 20 ms no matter how good the engine is.
Cold NVMe random I/O was not measured.

## Combining existing engines is not the leap

Jasper's 20–41% on TiDB is a real HTAP result and a different question.
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
   build and invalidation.

2. Physical grouping. Block bounds only won when values were clustered.
   The layout is part of the certificate.

3. Cold I/O for the 4,000-id example. Until that is measured, the 20 ms
   claim is an in-RAM result plus a network bound.

4. An insert-friendly shared arrangement. `Vec::insert` is not one.

5. A binary join that actually emits the two-hop product, so WCOJ has a
   fair explosion baseline. The current comparison plan already avoided
   the explosion, which is itself a finding.

## What I would not spend the next month on

Per-row extra metadata as a scan accelerator. Automatic SIMD as a
strategy. Local quotas as a substitute for a network hop we did not
pay. Learned indexes as a default primary key. Answer cells on
near-tied, high-churn rankings.

Those are not empty ideas. They lost on this machine, against honest
baselines, with exact answers. That is enough to demote them.
