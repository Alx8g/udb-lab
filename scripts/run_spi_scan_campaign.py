"""Two- or three-mode packed scan experiment on identical committed sources.

All normal workload phases are run, not just scans. Engines and implementation
order rotate. Diagnostic timing is rejected. SPI persistent bytes must match.
"""
from __future__ import annotations
import argparse
import json
import statistics
import subprocess
import sys
from pathlib import Path

from run_spi_campaign import command_output, sha256, write_json
from run_spi_crc_campaign import check_physical_pair
from spi_benchmark_identity import inspect_binary

CASES = {'normal': (2000, 64, 8388608), 'small': (2000, 64, 1024),
         'large': (1000, 4096, 8388608)}
SEEDS = [17, 29, 43]
ENGINES = ['spi', 'spi-packed', 'sqlite']


def inspect_pair(materialized: Path, direct: Path, root: Path, *, control_mode='materialized', candidate_mode='direct-base') -> dict:
    if control_mode == candidate_mode:
        raise ValueError('scan controls must select different execution modes')
    ids = {mode: inspect_binary(binary, root, ENGINES)
           for mode, binary in [(control_mode, materialized), (candidate_mode, direct)]}
    for mode, info in ids.items():
        if info['packed_scan_implementation'] != mode:
            raise ValueError(f'incorrect scan control: {mode}')
    if ids[control_mode]['source_sha256'] != ids[candidate_mode]['source_sha256']:
        raise ValueError('scan controls have different source bytes')
    if ids[control_mode]['crc32_implementation'] != ids[candidate_mode]['crc32_implementation']:
        raise ValueError('scan controls have different checksum implementations')
    return ids


def inspect_extra(delta: Path, root: Path, identities: dict) -> dict:
    if set(identities) != {'materialized', 'direct-base'}:
        raise ValueError('--delta requires materialized and direct-base controls')
    info = inspect_binary(delta, root, ENGINES)
    reference = identities['direct-base']
    if info['packed_scan_implementation'] != 'direct-delta':
        raise ValueError('incorrect direct-delta binary identity')
    if info['source_sha256'] != reference['source_sha256']:
        raise ValueError('direct-delta source mismatch')
    if info['crc32_implementation'] != reference['crc32_implementation']:
        raise ValueError('direct-delta CRC mismatch')
    return info


def physical_checks(out: Path, case: str, seed: int, modes: list[str]) -> list[dict]:
    checks = []
    for engine in ENGINES[:2]:
        reference = out / f'{case}-{seed}-{modes[0]}' / f'{engine}-seed-{seed}' / 'db'
        for mode in modes[1:]:
            candidate = out / f'{case}-{seed}-{mode}' / f'{engine}-seed-{seed}' / 'db'
            checks.append({'case': case, 'seed': seed, 'engine': engine,
                           'control_mode': modes[0], 'candidate_mode': mode,
                           'sha256': check_physical_pair(reference, candidate)})
    return checks


def mode_order(modes: list[str], case_index: int, ordinal: int) -> list[str]:
    if len(modes) == 2:
        return list(reversed(modes)) if (case_index + ordinal) % 2 else list(modes)
    # Rotate all three positions over the three seeds; no mode always runs last.
    shift = (case_index + ordinal) % len(modes)
    return modes[shift:] + modes[:shift]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--materialized', type=Path, required=True)
    parser.add_argument('--direct', type=Path, required=True)
    parser.add_argument('--delta', type=Path, help='Optional third direct-delta control; requires default mode names')
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--control-mode', choices=['materialized', 'direct-base'], default='materialized')
    parser.add_argument('--candidate-mode', choices=['direct-base', 'direct-delta'], default='direct-base')
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    bins = {args.control_mode: args.materialized.resolve(strict=True),
            args.candidate_mode: args.direct.resolve(strict=True)}
    ids = inspect_pair(args.materialized.resolve(strict=True), args.direct.resolve(strict=True), root,
                       control_mode=args.control_mode, candidate_mode=args.candidate_mode)
    if args.delta is not None:
        delta = args.delta.resolve(strict=True)
        ids['direct-delta'] = inspect_extra(delta, root, ids)
        bins['direct-delta'] = delta
    if command_output(['git', 'status', '--porcelain'], root):
        parser.error('commit all source before running a paired campaign')
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    binary_hashes = {mode: sha256(binary) for mode, binary in bins.items()}
    hashes = dict(ids[args.control_mode]['source_sha256'])
    for name in ['scripts/run_spi_scan_campaign.py', 'scripts/run_spi_crc_campaign.py',
                 'scripts/run_spi_campaign.py', 'scripts/spi_benchmark_identity.py']:
        hashes[name] = sha256(root / name)
    write_json(out / 'environment.json', {
        'git_head': command_output(['git', 'rev-parse', 'HEAD'], root),
        'source_sha256': hashes, 'binary_sha256': binary_hashes,
        'identities': ids, 'cases': CASES, 'seeds': SEEDS,
        'method': 'Sequential normal release controls. Two modes alternate, three modes rotate by case/seed. Engine order rotates. Base and post-mutation scans included.',
    })
    def check_inputs():
        if any(sha256(root / name) != sha for name, sha in hashes.items()):
            raise RuntimeError('scan campaign source changed')
        if any(sha256(bins[mode]) != sha for mode, sha in binary_hashes.items()):
            raise RuntimeError('scan campaign binary changed')
    combined, physical = {}, []
    for case_index, (case, (rows, size, cache)) in enumerate(CASES.items()):
        combined[case] = {mode: {engine: [] for engine in ENGINES} for mode in bins}
        for ordinal, seed in enumerate(SEEDS):
            modes = mode_order(list(bins), case_index, ordinal)
            engines = ENGINES[ordinal:] + ENGINES[:ordinal]
            for mode in modes:
                check_inputs()
                dest = out / f'{case}-{seed}-{mode}'
                cmd = [sys.executable, str(root / 'scripts/run_spi_campaign.py'),
                       '--binary', str(bins[mode]), '--expected-scan', mode,
                       '--expected-crc', ids[mode]['crc32_implementation'],
                       '--out', str(dest), '--rows', str(rows), '--value-bytes', str(size),
                       '--cache-bytes', str(cache), '--seeds', str(seed), '--engines', *engines]
                run = subprocess.run(cmd, cwd=root, capture_output=True, text=True, encoding='utf-8')
                write_json(out / f'command-{case}-{seed}-{mode}.json',
                           {'command': cmd, 'exit_code': run.returncode, 'stdout': run.stdout, 'stderr': run.stderr})
                if run.returncode:
                    raise RuntimeError(f'{case}/{mode} failed: {run.stderr}')
                summary = json.loads((dest / 'summary.json').read_text())
                if summary['trials'] != len(ENGINES) or not summary['all_full_outputs_match']:
                    raise RuntimeError('incomplete scan control campaign')
                for engine in ENGINES:
                    combined[case][mode][engine].append(summary['engines'][engine])
                print(f'{case} seed={seed} {mode}: all engine outputs PASS', flush=True)
            physical.extend(physical_checks(out, case, seed, list(bins)))
    check_inputs()
    for case, modes in combined.items():
        for mode, engines in modes.items():
            for engine, trials in engines.items():
                engines[engine] = {metric: {
                    'samples': [r[metric]['median'] for r in trials],
                    'median': statistics.median(r[metric]['median'] for r in trials)
                        if all(r[metric]['median'] is not None for r in trials) else None,
                } for metric in trials[0]}
    write_json(out / 'summary.json', {
        'trials': len(CASES)*len(SEEDS)*len(ENGINES)*len(bins),
        'all_full_outputs_match': True, 'cases': combined,
        'physical_equivalence_checks': physical,
        'limits': ['Execution modes are explicit; direct-delta merges validated row references, direct-base materializes deltas.',
                   'Returned payload still requires copying, not zero-copy API.',
                   'Whole workload timings include negative controls; no service-latency or power-loss proof.',
                   'OS cache/CPU placement uncontrolled. Three seeds do not establish statistical equivalence.'],
    })
    print('Paired scan campaign passed all output and persistent-byte checks.')


if __name__ == '__main__':
    main()
