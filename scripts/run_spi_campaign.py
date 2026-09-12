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

from spi_benchmark_identity import IdentityError, inspect_binary


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


def validate_record(record: dict, engine: str, rows: int, seed: int, value_bytes: int, cache_bytes: int) -> None:
    """Check the native result's operation and resource contract before acceptance."""
    expected = {"benchmark_schema": 2, "value_cache_workload_extension": 1,
                "engine": engine, "rows": rows, "seed": seed,
                "value_bytes": value_bytes, "cache_bytes": cache_bytes,
                "full_output_validation": "PASS"}
    for field, value in expected.items():
        if record.get(field) != value:
            raise RuntimeError(f"native result contract mismatch: {field}")
    for field in ("warm_hits", "reused_16_key_hits", "unique_value_reads", "one_off_scan"):
        metric = record.get(field, {})
        samples = metric.get("samples_ns")
        if not isinstance(samples, list) or not samples or any(type(n) is not int or n < 0 for n in samples):
            raise RuntimeError(f"invalid timing samples: {field}")
        if metric.get("total_ns") != sum(samples) or metric.get("sample_count") != len(samples):
            raise RuntimeError(f"inconsistent timing summary: {field}")
    if not engine.startswith("spi"):
        return
    snapshots = ("cache_after_reused_hits", "cache_after_unique_reads", "cache_after_one_off_scan",
                 "before_maintenance", "after_maintenance")
    for field in snapshots:
        stats = record[field]
        if not 0 <= stats["cache_bytes"] <= cache_bytes:
            raise RuntimeError(f"shared cache exceeds budget: {field}")
        if stats["value_cache_enabled"] != (engine == "spi-value-cache"):
            raise RuntimeError(f"incorrect cache control: {field}")
        if stats["retired_cache_value_capacity"] != 0:
            raise RuntimeError(f"retired value cache retained capacity: {field}")
        if engine != "spi-value-cache" and stats["cache_value_entries"] != 0:
            raise RuntimeError(f"node-only control admitted values: {field}")
        if value_bytes + 192 > cache_bytes // 8 and stats["cache_value_entries"] != 0:
            raise RuntimeError(f"undersized cache admitted values: {field}")
    if record["cache_after_one_off_scan"]["cache_value_entries"] != 0:
        raise RuntimeError("one-off scan populated the value cache")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--rows", type=int, default=5000)
    parser.add_argument("--value-bytes", type=int, default=64)
    parser.add_argument("--cache-bytes", type=int, default=8 * 1024 * 1024)
    parser.add_argument("--seeds", type=int, nargs="+", default=[17, 29, 43])
    parser.add_argument("--engines", nargs="+", choices=["spi", "spi-value-cache", "spi-unbuffered", "sqlite"], default=["spi", "sqlite"])
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    root = Path(__file__).resolve().parents[1]
    try:
        identity = inspect_binary(binary, root, args.engines)
    except IdentityError as exc:
        parser.error(str(exc))
    out = args.out.resolve()
    if args.rows < 100 or not 1 <= args.value_bytes <= 4096 or args.cache_bytes < 1024:
        parser.error("rows >= 100, 1 <= value-bytes <= 4096, cache-bytes >= 1024 required")
    if len(set(args.seeds)) != len(args.seeds) or len(set(args.engines)) != len(args.engines):
        parser.error("duplicate seeds or engines")
    out.mkdir(exist_ok=False)
    source_files = [root / "Cargo.toml", root / "Cargo.lock"]
    source_files += sorted((root / "src/spi").glob("*.rs"))
    source_files += [root / "src/lib.rs", root / "src/bin/spi-compare.rs", root / "scripts/spi_benchmark_identity.py", Path(__file__).resolve()]
    binary_hash = sha256(binary)
    source_hashes = {p.relative_to(root).as_posix(): sha256(p) for p in source_files}
    environment = {
        "created_utc": datetime.now(timezone.utc).isoformat(),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "processor": platform.processor(),
        "logical_cpus": os.cpu_count(),
        "rustc": command_output(["rustc", "-Vv"], root),
        "git_head": command_output(["git", "rev-parse", "HEAD"], root),
        "git_status": command_output(["git", "status", "--short"], root),
        "binary_sha256": binary_hash,
        "binary_identity": identity,
        "source_sha256": source_hashes,
        "rows": args.rows,
        "value_bytes": args.value_bytes,
        "cache_bytes": args.cache_bytes,
        "seeds": args.seeds,
        "method": "Sequential engines with rotating order by seed. Complete output checks inside native adapter. No OS-cache purge or CPU affinity setting.",
    }
    write_json(out / "environment.json", environment)
    records = []

    def check_unchanged_inputs() -> None:
        if sha256(binary) != binary_hash:
            raise RuntimeError("benchmark binary changed during campaign")
        for p in source_files:
            if sha256(p) != source_hashes[p.relative_to(root).as_posix()]:
                raise RuntimeError(f"source changed during campaign: {p.name}")

    for ordinal, seed in enumerate(args.seeds):
        shift = ordinal % len(args.engines)
        engines = args.engines[shift:] + args.engines[:shift]
        for engine in engines:
            check_unchanged_inputs()
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
            validate_record(record, engine, args.rows, seed, args.value_bytes, args.cache_bytes)
            records.append(record)
    check_unchanged_inputs()
    summary = {"trials": len(records), "all_full_outputs_match": True,
               "value_cache_workload_extension": 1, "cache_contract_checks": "PASS", "engines": {}}
    for engine in args.engines:
        trials = [r for r in records if r["engine"] == engine]
        metrics = {
            "load_ms": [r["load_batches_256"]["total_ns"] / 1e6 for r in trials],
            "reused_16_key_p50_us": [r["reused_16_key_hits"]["p50_ns"] / 1e3 for r in trials],
            "unique_reads_ms": [r["unique_value_reads"]["total_ns"] / 1e6 for r in trials],
            "one_off_scan_ms": [r["one_off_scan"]["total_ns"] / 1e6 for r in trials],
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
        if all("arena_write_calls" in r["before_maintenance"] for r in trials):
            metrics["arena_write_calls"] = [r["before_maintenance"]["arena_write_calls"] for r in trials]
            metrics["arena_bytes_written"] = [r["before_maintenance"]["bytes_written"] for r in trials]
        if engine.startswith("spi"):
            metrics["cache_bytes_after_reuse"] = [r["cache_after_reused_hits"]["cache_bytes"] for r in trials]
            metrics["cache_values_after_reuse"] = [r["cache_after_reused_hits"]["cache_value_entries"] for r in trials]
            metrics["cache_values_after_unique_reads"] = [r["cache_after_unique_reads"]["cache_value_entries"] for r in trials]
            metrics["cache_values_after_scan"] = [r["cache_after_one_off_scan"]["cache_value_entries"] for r in trials]
        summary["engines"][engine] = {k: {"median": statistics.median(v), "samples": v} for k, v in metrics.items()}
        if "arena_write_calls" not in metrics:
            summary["engines"][engine]["arena_write_calls"] = {"median": None, "samples": [], "status": "not measured"}
            summary["engines"][engine]["arena_bytes_written"] = {"median": None, "samples": [], "status": "not measured"}
    summary["limits"] = ["Local KV operation contract only, not SQL or production durability equivalence", "Logical file lengths are not allocated filesystem/device/NAND bytes", "Node/page cache budgets are not equal total memory or OS-cache budgets", "Closed-loop samples are not service latency under offered load", "Maintenance implementations and physical formats differ"]
    write_json(out / "summary.json", summary)
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
