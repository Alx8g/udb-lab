"""Print before/after speedups from records.json."""

from __future__ import annotations

import json
import sys
from pathlib import Path

PAIRS = [
    ("factorized", "naive_pairs", "factorized_sums"),
    ("factorized", "hashjoin_then_pairs", "incremental_insert_a"),
    ("answer_cells", "brute_scan_pareto", "cell_lookup_pareto"),
    ("answer_cells", "brute_scan_random", "cell_lookup_random"),
    ("answer_cells", "brute_scan_near_tie", "cell_lookup_near_tie"),
    ("executable_regions", "materialized_bulk_add", "compact_bulk_add"),
    ("executable_regions", "materialized_point_lookup_4k", "compact_point_lookup_4k"),
    ("progressive", "full_scan_clustered", "block_bounds_clustered"),
    ("progressive", "full_scan_spread", "block_bounds_spread"),
    ("shared_state", "rescan_each_query", "shared_index_counts"),
    ("certificate", "merge_disjoint_range", "bound_certificate_disjoint"),
    ("certificate", "merge_sparse", "gallop_sparse"),
    ("certificate", "merge_overlap", "gallop_overlap"),
    ("redundancy", "sum_from_a_and_b", "sum_from_coded_c"),
    ("engine", "row_scan_predicate", "col_scan_avx2"),
    ("engine", "lookup_4k_independent_bsearch", "lookup_4k_direct_id"),
    ("joint", "brute_materialized_scan", "joint_cells_over_bases"),
    ("joint", "brute_mixed_bulk_and_exceptions", "joint_mixed_bulk_and_exceptions"),
    ("correlation", "full_columns_tight", "block_bounds_tight"),
    ("correlation", "full_columns_none", "block_bounds_none"),
    ("correlation", "full_columns_tight", "model_prune_tight"),
    ("shared_state", "private_counts", "shared_index_counts"),
    ("residue", "scalar_recompute_q100", "histogram_stream_q100"),
    ("residue", "scalar_recompute_q10000", "histogram_stream_q10000"),
    ("chase", "interleave_1", "interleave_32"),
    ("prefix", "row_scan_clustered", "col_blocks_clustered"),
    ("prefix", "col_scan_clustered", "fenwick_clustered"),
    ("prefix", "row_scan_shuffled", "col_blocks_shuffled"),
    ("prefix", "col_blocks_shuffled", "fenwick_shuffled"),
    ("prefix", "fenwick_build_clustered", "fenwick_clustered"),
]


def load(path: Path) -> list[dict]:
    return json.loads(path.read_text(encoding="utf-8"))


def pick(recs: list[dict], exp: str, variant: str) -> list[dict]:
    return [r for r in recs if r["experiment"] == exp and r["variant"] == variant]


def fmt(ns: float) -> str:
    if ns >= 1e9:
        return f"{ns/1e9:.3f}s"
    if ns >= 1e6:
        return f"{ns/1e6:.3f}ms"
    if ns >= 1e3:
        return f"{ns/1e3:.3f}µs"
    return f"{ns:.0f}ns"


def main() -> None:
    path = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("results/full/records.json")
    recs = load(path)
    print(f"{'experiment':<22} {'before':<28} {'after':<28} {'n':>10} {'before_t':>10} {'after_t':>10} {'speedup':>10}")
    for exp, before, after in PAIRS:
        bs = pick(recs, exp, before)
        as_ = pick(recs, exp, after)
        if not bs or not as_:
            continue
        # pair by n when possible
        by_n = {}
        for r in as_:
            by_n.setdefault(r["n"], r)
        for b in bs:
            a = by_n.get(b["n"]) or as_[0]
            if a["median_ns"] <= 0:
                continue
            if b.get("below_timer_resolution") or a.get("below_timer_resolution"):
                print(
                    f"{exp:<22} {before:<28} {after:<28} {b['n']:>10} {fmt(b['median_ns']):>10} {fmt(a['median_ns']):>10}     n/a (below 1us)"
                )
                continue
            sp = b["median_ns"] / a["median_ns"]
            print(
                f"{exp:<22} {before:<28} {after:<28} {b['n']:>10} {fmt(b['median_ns']):>10} {fmt(a['median_ns']):>10} {sp:>9.1f}x"
            )


if __name__ == "__main__":
    main()
