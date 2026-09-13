"""Run source-identified diagnostic builds only. Output is NOT performance evidence.

Detailed instrumentation and allocator tracking distort execution time. Run the
same normal release binary separately for accepted latency comparisons. No installs,
compilation, deletion or directory replacement performed here.
"""
from __future__ import annotations
import argparse
import json
import subprocess
from pathlib import Path

from run_spi_campaign import command_output, sha256, validate_record, write_json
from spi_benchmark_identity import IdentityError, inspect_binary
from validate_engine_result import external_inputs, validate_engine_name

PHASES = ('load', 'warm_hits', 'misses', 'cleared_hits', 'ranges', 'reuse',
          'unique', 'scan', 'updates', 'single_commits', 'reopen', 'maintenance',
          'post_mutation_scan', 'post_mutation_ranges', 'post_compaction_scan')
COUNTERS = frozenset(('read_calls', 'read_bytes', 'read_ns', 'write_calls', 'write_bytes',
    'write_ns', 'checksum_calls', 'checksum_bytes', 'checksum_ns', 'node_cache_hits',
    'node_cache_misses', 'value_cache_hits', 'value_cache_misses', 'node_saves', 'value_records',
    'build_ns', 'data_sync_ns', 'manifest_write_ns', 'manifest_sync_ns', 'manifest_replace_ns',
    'directory_sync_ns', 'fault_checks', 'fault_check_ns'))


def validate_profile(record: dict, engine: str, rows: int, seed: int, size: int, cache: int) -> None:
    validate_record(record, engine, rows, seed, size, cache, diagnostic=True)
    if record.get('diagnostic_only') is not True:
        raise RuntimeError('profile evidence is not marked diagnostic-only')
    phases = record.get('diagnostic_phases', {})
    if set(phases) != set(PHASES):
        raise RuntimeError('diagnostic phase inventory mismatch')
    for phase, metrics in phases.items():
        for field in ('phase_wall_ns_including_harness', 'rust_allocation_calls',
                      'rust_requested_allocation_bytes', 'rust_live_requested_bytes',
                      'process_peak_rust_requested_bytes'):
            if type(metrics.get(field)) is not int or metrics[field] < 0:
                raise RuntimeError(f'invalid profile counter {phase}: {field}')
        cpu = metrics.get('process_cpu_ns_including_harness')
        if cpu is not None and (type(cpu) is not int or cpu < 0):
            raise RuntimeError(f'invalid process CPU counter: {phase}')
        storage = metrics.get('storage', {})
        if set(storage) != COUNTERS or any(type(n) is not int or n < 0 for n in storage.values()):
            raise RuntimeError(f'invalid storage profile: {phase}')
        if metrics['rust_live_requested_bytes'] > metrics['process_peak_rust_requested_bytes']:
            raise RuntimeError('live allocation exceeds lifetime peak')
    if not engine.startswith('spi') and any(n for p in phases.values() for n in p['storage'].values()):
        raise RuntimeError('external engine profile unexpectedly contains SPI counters')


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--rows', type=int, default=2000)
    parser.add_argument('--value-bytes', type=int, default=64)
    parser.add_argument('--cache-bytes', type=int, default=8388608)
    parser.add_argument('--seeds', nargs='+', type=int, default=[17, 29, 43])
    parser.add_argument('--engines', nargs='+', choices=['spi', 'spi-grouped', 'spi-packed', 'sqlite', 'lmdb', 'redb', 'rocksdb', 'duckdb', 'postgres'], default=['spi', 'spi-grouped', 'sqlite'])
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    binary = args.binary.resolve(strict=True)
    try:
        identity = inspect_binary(binary, root, args.engines, allow_profile=True)
    except IdentityError as exc:
        parser.error(str(exc))
    if not identity['profile_enabled']:
        parser.error('a diagnostic spi-profile build is required')
    if args.rows < 100 or not 1 <= args.value_bytes <= 4096 or args.cache_bytes < 1024:
        parser.error('rows>=100, 1<=value-bytes<=4096 and cache-bytes>=1024 required')
    if len(set(args.seeds)) != len(args.seeds) or len(set(args.engines)) != len(args.engines):
        parser.error('duplicate seeds/engines')
    if command_output(['git', 'status', '--porcelain'], root):
        parser.error('commit source first, diagnostics require a clean source identity')
    capabilities = {e: validate_engine_name(e, shared_kv=True) for e in args.engines}
    dependencies = external_inputs(args.engines)
    output = args.out.resolve()
    output.mkdir(parents=True, exist_ok=False)
    binary_hash = sha256(binary)
    inputs = dict(identity['source_sha256'])
    for name in ['scripts/run_spi_profile.py', 'scripts/run_spi_campaign.py', 'scripts/spi_benchmark_identity.py',
                 'scripts/validate_engine_result.py', 'scripts/engine_capabilities.json']:
        inputs[name] = sha256(root / name)
    write_json(output / 'environment.json', {
        'diagnostic_only': True, 'binary_identity': identity, 'binary_sha256': binary_hash,
        'engine_capabilities': capabilities, 'external_dependencies': dependencies,
        'git_head': command_output(['git', 'rev-parse', 'HEAD'], root),
        'source_sha256': inputs, 'rows': args.rows, 'seeds': args.seeds,
        'cache_bytes': args.cache_bytes, 'value_bytes': args.value_bytes,
        'rustc': command_output(['rustc', '-Vv'], root),
        'limits': [
            'Instrumented wall timings are not accepted performance results',
            'Process CPU includes harness/oracle work, not CPU per engine operation',
            'Rust allocator counts omit native engine allocations, allocator metadata and OS cache',
            'Server CPU and memory are not captured by client-process profiling',
            'Process memory samples include benchmark inputs and output buffers',
            'Storage counters persist across reopen/maintenance and include verify',
            'Storage timers overlap when nested. Do not add all categories.',
            'Process CPU has OS-specific resolution and may report zero in short phases',
        ],
    })
    results = []
    def check_inputs():
        if external_inputs(args.engines) != dependencies:
            raise RuntimeError('external engine dependency changed during diagnostic run')
        if sha256(binary) != binary_hash or any(sha256(root / n) != h for n, h in inputs.items()):
            raise RuntimeError('diagnostic inputs changed during run')
    for i, seed in enumerate(args.seeds):
        shift = i % len(args.engines)
        for engine in args.engines[shift:] + args.engines[:shift]:
            check_inputs()
            destination = output / f'{engine}-seed-{seed}'
            cmd = [str(binary), str(destination), engine, str(args.rows), str(seed),
                   str(args.value_bytes), str(args.cache_bytes)]
            result = subprocess.run(cmd, cwd=root, capture_output=True, text=True, encoding='utf-8', timeout=600)
            write_json(output / f'command-{engine}-{seed}.json', {'command': cmd, 'exit_code': result.returncode,
                       'stdout': result.stdout, 'stderr': result.stderr})
            if result.returncode:
                raise RuntimeError(f'{engine} diagnostic failed: {result.stderr}')
            record = json.loads((destination / 'result.json').read_text(encoding='utf-8'))
            validate_profile(record, engine, args.rows, seed, args.value_bytes, args.cache_bytes)
            for field in ('packed_scan_implementation', 'crc32_implementation'):
                if record.get(field) != identity[field]:
                    raise RuntimeError(f'profile binary/result mismatch: {field}')
            results.append({'engine': engine, 'seed': seed, 'phases': record['diagnostic_phases']})
            print(f'{engine} seed={seed} full-output and diagnostic-contract PASS', flush=True)
    check_inputs()
    write_json(output / 'summary.json', {'diagnostic_only': True, 'trials': len(results),
                'all_full_outputs_match': True, 'results': results})


if __name__ == '__main__':
    main()
