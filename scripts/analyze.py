"""Turn kernel records into comparison tables and a findings markdown."""

from __future__ import annotations

import json
import math
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RESULTS = ROOT / "results"


def load_records(kind: str) -> list[dict]:
    path = RESULTS / kind / "records.json"
    if not path.exists():
        return []
    return json.loads(path.read_text(encoding="utf-8"))


def ns(x: int | float) -> str:
    x = float(x)
    if x >= 1e9:
        return f"{x / 1e9:.3f} s"
    if x >= 1e6:
        return f"{x / 1e6:.3f} ms"
    if x >= 1e3:
        return f"{x / 1e3:.3f} µs"
    return f"{x:.0f} ns"


def ratio(a: float, b: float) -> str:
    if b == 0:
        return "n/a"
    r = a / b
    if r >= 1:
        return f"{r:.2f}× slower than winner"
    return f"{1 / r:.2f}× faster"


def group(recs: list[dict]) -> dict[str, list[dict]]:
    g: dict[str, list[dict]] = defaultdict(list)
    for r in recs:
        g[r["experiment"]].append(r)
    return g


def best_of(recs: list[dict], key: str = "median_ns") -> dict | None:
    timed = [r for r in recs if r.get(key, 0) and r[key] > 0]
    if not timed:
        return None
    return min(timed, key=lambda r: r[key])


def write_findings(kind: str, recs: list[dict]) -> str:
    lines: list[str] = []
    lines.append(f"# Kernel findings ({kind})")
    lines.append("")
    lines.append("Machine: Intel Core i9-12900H, 32 GB, Windows 11. Release, LTO thin.")
    lines.append("")
    lines.append("Comparisons are same-process, same-data, same-compiler. They measure")
    lines.append("the isolated mechanism. They are not a database ranking.")
    lines.append("")
    by = group(recs)
    for exp in sorted(by):
        rows = by[exp]
        lines.append(f"## {exp}")
        lines.append("")
        lines.append("| variant | n | median | p99 | ns/op | bytes | note |")
        lines.append("|---|---:|---:|---:|---:|---:|---|")
        for r in rows:
            ops = r.get("ops") or 1
            per = r["median_ns"] / ops if ops else 0
            extra = r.get("extra") or {}
            note = r.get("notes", "")
            if isinstance(extra, dict) and "hit_rate" in extra:
                note += f" hit_rate={extra['hit_rate']:.3f} rebuilds={extra.get('rebuilds')}"
            if isinstance(extra, dict) and "cells" in extra:
                note += f" cells={extra['cells']}"
            if isinstance(extra, dict) and "segments" in extra:
                note += f" segs={extra['segments']}"
            if isinstance(extra, dict) and "intermediates" in extra:
                note += f" intermediates={extra['intermediates']}"
            lines.append(
                f"| `{r['variant']}` | {r['n']} | {ns(r['median_ns'])} | {ns(r['p99_ns'])} | "
                f"{per:.2f} | {r.get('bytes_touched', 0)} | {note} |"
            )
        lines.append("")
    path = RESULTS / kind / "findings.md"
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return str(path)


def main() -> None:
    RESULTS.mkdir(parents=True, exist_ok=True)
    for kind in ("quick", "full"):
        recs = load_records(kind)
        if recs:
            p = write_findings(kind, recs)
            print(f"{kind}: {len(recs)} records -> {p}")
        else:
            print(f"{kind}: no records yet")


if __name__ == "__main__":
    main()
