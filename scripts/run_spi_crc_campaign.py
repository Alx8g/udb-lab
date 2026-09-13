"""Paired release CRC experiment with source identity and byte-equivalence checks.

Runs bitwise/sliced binaries from identical committed sources, alternates binary
order by seed, and uses the normal complete-output campaign runner. No builds,
installs, deletion or replacement. Diagnostic binaries are rejected.
"""
from __future__ import annotations
import argparse
import json
import statistics
import subprocess
import sys
from pathlib import Path

from run_spi_campaign import command_output, sha256, write_json
from spi_benchmark_identity import inspect_binary

CASES = {
    'normal': (2000, 64, 8388608),
    'small': (2000, 64, 1024),
    'large': (1000, 4096, 8388608),
}
SEEDS = [17, 29, 43]
ENGINES = ['spi', 'spi-packed', 'sqlite']


def inspect_pair(bitwise: Path, sliced: Path, root: Path) -> dict:
    identities = {name: inspect_binary(binary, root, ENGINES)
                  for name, binary in [('ieee-bitwise', bitwise), ('ieee-slicing8', sliced)]}
    for name, identity in identities.items():
        if identity['crc32_implementation'] != name:
            raise ValueError(f'incorrect control binary for {name}')
    if identities['ieee-bitwise']['source_sha256'] != identities['ieee-slicing8']['source_sha256']:
        raise ValueError('CRC controls are not compiled from identical source bytes')
    if identities['ieee-bitwise']['packed_scan_implementation'] != identities['ieee-slicing8']['packed_scan_implementation']:
        raise ValueError('CRC controls have different scan implementations')
    return identities


def check_physical_pair(left: Path, right: Path) -> dict:
    """Compare every SPI-owned persistent byte after verified maintenance/reopen."""
    def files(root):
        return {p.relative_to(root).as_posix(): sha256(p) for p in root.rglob('*')
                if p.is_file()}
    a, b = files(left), files(right)
    if not a or a != b:
        raise ValueError('CRC implementations produced different persistent bytes')
    return a


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bitwise', type=Path, required=True)
    parser.add_argument('--sliced', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    binaries = {'ieee-bitwise': args.bitwise.resolve(strict=True),
                'ieee-slicing8': args.sliced.resolve(strict=True)}
    identities = inspect_pair(binaries['ieee-bitwise'], binaries['ieee-slicing8'], root)
    if command_output(['git', 'status', '--porcelain'], root):
        parser.error('commit all source before the paired campaign')
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    binary_hashes = {name: sha256(path) for name, path in binaries.items()}
    source_hashes = dict(identities['ieee-bitwise']['source_sha256'])
    for name in ['scripts/run_spi_crc_campaign.py', 'scripts/run_spi_campaign.py', 'scripts/spi_benchmark_identity.py']:
        source_hashes[name] = sha256(root / name)
    write_json(out / 'environment.json', {
        'git_head': command_output(['git', 'rev-parse', 'HEAD'], root),
        'source_sha256': source_hashes, 'binary_sha256': binary_hashes,
        'identities': identities, 'cases': CASES, 'seeds': SEEDS,
        'method': 'Sequential normal release controls. Binary order alternates per seed and case. Engine order rotates per seed. Complete outputs validated in each child campaign.',
    })
    def check_inputs():
        if any(sha256(root / n) != h for n, h in source_hashes.items()):
            raise RuntimeError('paired-campaign source changed')
        if any(sha256(binaries[n]) != h for n, h in binary_hashes.items()):
            raise RuntimeError('paired-campaign binary changed')
    combined, physical = {}, []
    for case_index, (case, (rows, size, cache)) in enumerate(CASES.items()):
        combined[case] = {name: {engine: [] for engine in ENGINES} for name in binaries}
        for ordinal, seed in enumerate(SEEDS):
            order = list(binaries)
            if (case_index + ordinal) % 2:
                order.reverse()
            engines = ENGINES[ordinal:] + ENGINES[:ordinal]
            for name in order:
                check_inputs()
                destination = out / f'{case}-{seed}-{name}'
                cmd = [sys.executable, str(root / 'scripts/run_spi_campaign.py'),
                       '--binary', str(binaries[name]), '--expected-crc', name,
                       '--out', str(destination), '--rows', str(rows), '--value-bytes', str(size),
                       '--cache-bytes', str(cache), '--seeds', str(seed), '--engines', *engines]
                run = subprocess.run(cmd, cwd=root, capture_output=True, text=True, encoding='utf-8')
                write_json(out / f'command-{case}-{seed}-{name}.json',
                           {'command': cmd, 'exit_code': run.returncode, 'stdout': run.stdout, 'stderr': run.stderr})
                if run.returncode:
                    raise RuntimeError(f'{case}/{name} failed: {run.stderr}')
                summary = json.loads((destination / 'summary.json').read_text())
                if summary['trials'] != len(ENGINES) or not summary['all_full_outputs_match']:
                    raise RuntimeError('incomplete child campaign')
                for engine in ENGINES:
                    combined[case][name][engine].append(summary['engines'][engine])
                print(f'{case} seed={seed} {name}: all engine outputs PASS', flush=True)
            for engine in ENGINES[:2]:
                paths = [out / f'{case}-{seed}-{name}' / f'{engine}-seed-{seed}' / 'db' for name in binaries]
                hashes = check_physical_pair(*paths)
                physical.append({'case': case, 'seed': seed, 'engine': engine, 'sha256': hashes})
    check_inputs()
    for case, implementations in combined.items():
        for name, engines in implementations.items():
            for engine, trials in engines.items():
                engines[engine] = {metric: {
                    'samples': [r[metric]['median'] for r in trials],
                    'median': statistics.median(r[metric]['median'] for r in trials)
                              if all(r[metric]['median'] is not None for r in trials) else None,
                } for metric in trials[0]}
    write_json(out / 'summary.json', {'trials': len(CASES)*len(SEEDS)*len(ENGINES)*len(binaries),
        'all_full_outputs_match': True, 'physical_equivalence_checks': physical, 'cases': combined,
        'limits': ['Small local closed-loop trials, OS cache and CPU placement uncontrolled',
                   'CRC table consumes 8192 static read-only bytes per process, not per database',
                   'Physical byte equality applies to paired CRC implementations, not different engines',
                   'Same low-level hash algorithm/format, not weakened durability',
                   'Source identity does not attest compiler machine code or performance causality']})
    print('Paired campaign PASS: complete outputs and identical SPI persistent bytes.')


if __name__ == '__main__':
    main()
