# Kernel protocol

Machine: 12th Gen Intel Core i9-12900H, 14 cores / 20 threads, 32 GB DDR4-3200, NVMe.
Compiler: rustc 1.96.0, `--release`, LTO thin, codegen-units 1.

Each kernel:

1. Correctness against an independent reconstruction.
2. A specialist or naive baseline on the same data.
3. The proposed mechanism.
4. A negative control that should not win.
5. Accounting that includes maintenance, not only the happy-path read.

Not a database ranking. Isolated mechanisms, same process, same compiler.

## Mechanisms

| Family | Claim under test | Negative control |
|---|---|---|
| factorized | join-aggregate pairs need not exist | XOR-coupled aggregate |
| answer cells | winner reused across weights and bounded drift | near-ties / large drift |
| executable regions | bulk update is one adjustment | point lookup / full filter scan |
| progressive | bounds settle many decisions without residuals | values packed in one bucket |
| certificate | empty join from two extrema | interleaved empty |
| wcoj | neighbor intersection vs two-hop expansion | dense overlap where AGM is about input size |
| coordination | local quotas vs global CAS | leftover quota / false sold-out |
| redundancy | C=A+B is recovery and A+B projection | predicate on A only |
| pgm | model predicts, array decides | hash map specialist; clustered keys |
| engine | layout + SIMD + batched lookup | 4k independent bsearch; payload bound |
| shared state | one index, N parameterized counts | private match lists |
| joint | bulk adj invisible to ranking | irregular per-row jitter |
| correlation | same model for storage and prune | shuffled delivery |
| residue | rounded affine family from n, sum, 2q histogram | unmaintained x>T filter |
| chase | same logical pointer-chases, more overlap | width-1 serial chains |
| prefix | layout x block summaries vs Fenwick | shuffled keys; Fenwick specialist |
| ranking_cert | ordinary vs simplex-tight winner certificates | high-d, sufficient bound rejects true stables |

## Acceptance for a mechanism

Survive if, after including construction and invalidation:

- at least one workload family shows 10× or more less work or time versus the strongest simple alternative, and
- the negative control does not silently return wrong answers.

A mechanism that only wins by being approximate is a fail.
