"""Run sequential, reproducible native SPI/SQLite trials into a new directory.

No compilation, package installation, cache dropping, or directory deletion.
Use uv run --no-project scripts/run_spi_campaign.py --help.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import statistics
import subprocess
from datetime import datetime, timezone
from pathlib import Path


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def command_output(command: list[str], cwd: Path) -> str:
    result = subprocess.run(command, cwd=cwd, capture_output=True, text=True, encoding="utf-8")
    if result.returncode:
        raise RuntimeError(f"{command[0]} failed: {result.stderr}")
    return result.stdout.strip()


def write_json(path: Path, content: object) -> None:
    with path.open("x", encoding="utf-8") as stream:
        json.dump(content, stream, indent=2)
        stream.write("\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--rows", type=int, default=5000)
    parser.add_argument("--value-bytes", type=int, default=64)
    parser.add_argument("--cache-bytes", type=int, default=8 * 1024 * 1024)
    parser.add_argument("--seeds", type=int, nargs="+", default=[17, 29, 43])
    parser.add_argument("--engines", nargs="+", choices=["spi", "sqlite"], default=["spi", "sqlite"])
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    root = Path(__file__).resolve().parents[1]
    out = args.out.resolve()
    if args.rows < 100 or not 1 <= args.value_bytes <= 4096 or args.cache_bytes < 1024:
        parser.error("rows >= 100, 1 <= value-bytes <= 4096, cache-bytes >= 1024 required")
    if len(set(args.seeds)) != len(args.seeds) or len(set(args.engines)) != len(args.engines):
        parser.error("duplicate seeds or engines")
    out.mkdir(exist_ok=False)
    source_files = [root / "Cargo.toml", root / "Cargo.lock"]
    source_files += sorted((root / "src/spi").glob("*.rs"))
    source_files += [root / "src/lib.rs", root / "src/bin/spi-compare.rs", Path(__file__).resolve()]
    environment = {
        "created_utc": datetime.now(timezone.utc).isoformat(),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "processor": platform.processor(),
        "logical_cpus": os.cpu_count(),
        "rustc": command_output(["rustc", "-Vv"], root),
        "git_head": command_output(["git", "rev-parse", "HEAD"], root),
        "git_status": command_output(["git", "status", "--short"], root),
        "binary_sha256": sha256(binary),
        "source_sha256": {p.relative_to(root).as_posix(): sha256(p) for p in source_files},
        "rows": args.rows,
        "value_bytes": args.value_bytes,
        "cache_bytes": args.cache_bytes,
        "seeds": args.seeds,
        "method": "Sequential engines in alternating order by seed. Complete output checks inside native adapter. No OS-cache purge or CPU affinity setting.",
    }
    write_json(out / "environment.json", environment)
    records = []
    for ordinal, seed in enumerate(args.seeds):
        engines = args.engines if ordinal % 2 == 0 else list(reversed(args.engines))
        for engine in engines:
            destination = out / f"{engine}-seed-{seed}"
            command = [str(binary), str(destination), engine, str(args.rows), str(seed), str(args.value_bytes), str(args.cache_bytes)]
            start = datetime.now(timezone.utc).isoformat()
            result = subprocess.run(command, cwd=root, capture_output=True, text=True, encoding="utf-8")
            evidence = {"command": command, "started_utc": start, "finished_utc": datetime.now(timezone.utc).isoformat(), "exit_code": result.returncode, "stdout": result.stdout, "stderr": result.stderr}
            write_json(out / f"command-{engine}-{seed}.json", evidence)
            print(result.stdout, end="", flush=True)
            if result.returncode:
                raise RuntimeError(f"{engine} seed {seed} failed: {result.stderr}")
            record = json.loads((destination / "result.json").read_text(encoding="utf-8"))
            if record.get("benchmark_schema") != 2 or record.get("full_output_validation") != "PASS":
                raise RuntimeError("old benchmark schema or failed full-output validation")
            records.append(record)
    summary = {"trials": len(records), "all_full_outputs_match": True, "engines": {}}
    for engine in args.engines:
        trials = [r for r in records if r["engine"] == engine]
        metrics = {
            "load_ms": [r["load_batches_256"]["total_ns"] / 1e6 for r in trials],
            "warm_hit_p50_us": [r["warm_hits"]["p50_ns"] / 1e3 for r in trials],
            "warm_hit_p99_us": [r["warm_hits"]["p99_ns"] / 1e3 for r in trials],
            "cleared_app_cache_p50_us": [r["application_cache_cleared_hits_not_storage_cold"]["p50_ns"] / 1e3 for r in trials],
            "range_p50_us": [r["ranges_up_to_100_keys"]["p50_ns"] / 1e3 for r in trials],
            "updates_ms": [r["updates_batches_64"]["total_ns"] / 1e6 for r in trials],
            "durable_mutation_p50_us": [r["single_row_commits"]["p50_ns"] / 1e3 for r in trials],
            "reopen_ms": [r["reopen_ns"] / 1e6 for r in trials],
            "maintenance_ms": [r["maintenance_with_integrity_check_ns"] / 1e6 for r in trials],
            "final_owned_file_bytes": [r["after_maintenance"]["physical_bytes"] for r in trials],
        }
        summary["engines"][engine] = {k: {"median": statistics.median(v), "samples": v} for k, v in metrics.items()}
    summary["limits"] = ["Local KV operation contract only, not SQL or production durability equivalence", "Logical file lengths are not allocated filesystem/device/NAND bytes", "Node/page cache budgets are not equal total memory or OS-cache budgets", "Closed-loop samples are not service latency under offered load", "Maintenance implementations and physical formats differ"]
    write_json(out / "summary.json", summary)
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
