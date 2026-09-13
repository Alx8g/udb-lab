"""Run a representative, version-identified embedded KV-operation campaign.

Delegates output/provenance validation to run_spi_campaign.py. DuckDB is measured
as a prepared BLOB KV workload, not analytics. PostgreSQL uses the separate owned
server runner. Cache, reopen and maintenance differences remain qualified.
"""
from __future__ import annotations
import argparse
import json
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

from run_spi_campaign import sha256, write_json
from spi_benchmark_identity import inspect_binary
from validate_engine_result import load_capabilities, validate_engine_name, external_inputs

ENGINES = ['spi', 'spi-packed', 'sqlite', 'redb', 'lmdb', 'rocksdb', 'duckdb']
CASES = {'normal': (2000, 64, 8388608), 'small': (2000, 64, 1024),
         'large': (1000, 4096, 8388608), 'scale': (20000, 64, 8388608)}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--engines', nargs='+', choices=ENGINES, default=ENGINES)
    parser.add_argument('--cases', nargs='+', choices=list(CASES), default=list(CASES))
    parser.add_argument('--seeds', nargs='+', type=int, default=[17, 29, 43])
    args = parser.parse_args()
    for values in (args.engines, args.cases, args.seeds):
        if len(set(values)) != len(values):
            parser.error('duplicate engine, case or seed')
    root = Path(__file__).resolve().parents[1]
    binary = args.binary.resolve(strict=True)
    identity = inspect_binary(binary, root, args.engines)
    capabilities = load_capabilities()
    for engine in args.engines:
        validate_engine_name(engine, shared_kv=True)
    dependencies = external_inputs(args.engines)
    if subprocess.check_output(['git', 'status', '--porcelain'], cwd=root).strip():
        parser.error('commit all source before campaign')
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    hashes = dict(identity['source_sha256'])
    for name in ['scripts/run_embedded_kv_campaign.py', 'scripts/run_spi_campaign.py',
                 'scripts/spi_benchmark_identity.py', 'scripts/validate_engine_result.py',
                 'scripts/engine_capabilities.json']:
        hashes[name] = sha256(root / name)
    binary_hash = sha256(binary)
    write_json(out / 'environment.json', {
        'created_utc': datetime.now(timezone.utc).isoformat(),
        'git_head': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip(),
        'git_status': '', 'source_sha256': hashes, 'binary_sha256': binary_hash,
        'binary_identity': identity, 'external_dependencies': dependencies,
        'engines': args.engines, 'cases': {name: CASES[name] for name in args.cases}, 'seeds': args.seeds,
        'coverage_gaps': capabilities['coverage_gaps'],
        'limits': ['Common ordered binary-KV operation stream, not universal database capability',
                   'Requested cache budgets are not equal total memory. LMDB cache is OS-managed.',
                   'DuckDB uses prepared BLOB operations and a declared memory floor, not columnar analytics.',
                   'No concurrency/offered-load, power-loss or larger-than-RAM result is implied.'],
    })
    def unchanged():
        if sha256(binary) != binary_hash or any(sha256(root / n) != h for n, h in hashes.items()):
            raise RuntimeError('campaign inputs changed')
        if external_inputs(args.engines) != dependencies:
            raise RuntimeError('external dependencies changed')
    results = {}
    for ordinal, case in enumerate(args.cases):
        unchanged()
        rows, size, cache = CASES[case]
        shift = ordinal % len(args.engines)
        engines = args.engines[shift:] + args.engines[:shift]
        command = [sys.executable, str(root / 'scripts/run_spi_campaign.py'), '--binary', str(binary),
                   '--out', str(out / case), '--rows', str(rows), '--value-bytes', str(size),
                   '--cache-bytes', str(cache), '--seeds', *map(str, args.seeds), '--engines', *engines]
        run = subprocess.run(command, cwd=root, capture_output=True, text=True, encoding='utf-8', timeout=1800)
        write_json(out / f'command-{case}.json', {'command': command, 'exit_code': run.returncode,
                   'stdout': run.stdout, 'stderr': run.stderr})
        if run.returncode:
            raise RuntimeError(f'{case} failed; inspect preserved command evidence: {run.stderr}')
        result = json.loads((out / case / 'summary.json').read_text())
        if result['trials'] != len(args.engines)*len(args.seeds) or not result['all_full_outputs_match']:
            raise RuntimeError('incomplete child campaign')
        results[case] = result
        print(case, result['trials'], 'trials PASS', flush=True)
    unchanged()
    write_json(out / 'summary.json', {'schema': 1,
        'trials': sum(r['trials'] for r in results.values()), 'all_full_outputs_match': True,
        'cases': results, 'coverage_gaps': capabilities['coverage_gaps'],
        'scope': 'Representative native KV-operation comparison. Engine lifecycle and cache metrics are qualified, not one overall score.'})
    print('Embedded campaign completed with all outputs validated.')


if __name__ == '__main__':
    main()
