"""Summarize verified diagnostic resources, never performance measurements.

Rust allocation traffic is not native-library allocation traffic or resident
memory. OS samples include the harness and engine. Phase CPU can be zero below
OS resolution and is not CPU per operation. Nested storage timers overlap.
"""
import argparse
import hashlib
import json
from pathlib import Path
import sys

ROOT = next(p for p in Path(__file__).resolve().parents if (p / 'Cargo.toml').is_file())
sys.path.insert(0, str(ROOT / 'scripts'))
from run_spi_profile import validate_profile


def load(path):
    return json.loads(path.read_bytes())


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def summarize(directory):
    manifest = load(directory / 'manifest.json')
    if manifest.get('diagnostic_only') is not True:
        raise ValueError('Only diagnostic resource evidence is accepted')
    for name, h in manifest['sha256'].items():
        path = directory / name
        if not path.resolve().is_relative_to(directory.resolve()) or digest(path) != h:
            raise ValueError('Manifest mismatch: ' + name)
    env = load(directory / 'environment.json')
    records = []
    for engine in env['engine_capabilities']:
        for seed in env['seeds']:
            record = load(directory / f'{engine}-seed-{seed}/result.json')
            validate_profile(record, engine, env['rows'], seed, env['value_bytes'], env['cache_bytes'])
            phases = record['diagnostic_phases']
            cpu = [v['process_cpu_ns_including_harness'] for v in phases.values()]
            working = [(v.get('os_memory_after_phase') or {}).get('working_set_bytes') for v in phases.values()]
            private = [(v.get('os_memory_after_phase') or {}).get('private_commit_bytes') for v in phases.values()]
            selected = lambda values: max((n for n in values if n is not None), default=None)
            rows = {'engine': engine, 'seed': seed,
                    'sum_measured_phase_cpu_ns_including_harness': sum(cpu) if all(n is not None for n in cpu) else None,
                    'sum_phase_rust_allocation_requests_bytes': sum(v['rust_requested_allocation_bytes'] for v in phases.values()),
                    'max_sampled_working_set_bytes_including_harness': selected(working),
                    'max_sampled_private_commit_bytes_including_harness': selected(private),
                    'measured_phases': phases}
            commits = phases['single_commits']
            if engine.startswith('spi'):
                storage = commits['storage']
                rows['spi_publication_attribution'] = {
                    'single_commit_phase_wall_ns_diagnostic': commits['phase_wall_ns_including_harness'],
                    'data_sync_ns': storage['data_sync_ns'],
                    'manifest_write_ns': storage['manifest_write_ns'],
                    'manifest_sync_ns': storage['manifest_sync_ns'],
                    'manifest_replace_ns': storage['manifest_replace_ns'],
                    'directory_sync_ns': storage['directory_sync_ns'],
                    'build_ns_nested': storage['build_ns'],
                    'note': 'Diagnostic scope only. Do not sum overlapping timers or remove publication barriers.'}
            records.append(rows)
    return {'source_commit': env['git_head'], 'rows': env['rows'], 'value_bytes': env['value_bytes'],
            'requested_cache_bytes': env['cache_bytes'], 'source_manifest_sha256': digest(directory / 'manifest.json'),
            'trials': len(records), 'records': records, 'limits': env['limits']}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, nargs='+', required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    cases = {str(p): summarize(p) for p in args.source}
    args.out.parent.mkdir(parents=True, exist_ok=True)
    with args.out.open('x', encoding='utf-8', newline='\n') as f:
        json.dump({'diagnostic_only': True, 'generator_sha256': digest(Path(__file__)), 'cases': cases,
                   'postgres_server_resources': 'not measured; client CPU/memory cannot substitute for server resources',
                   'limits': ['Do not treat diagnostic timings as accepted performance results',
                              'Do not compare Rust-only allocation totals as total engine allocation costs',
                              'Process samples include the harness and omit system-wide OS cache',
                              'The sum of measured phase CPU excludes startup, setup and between-phase work',
                              'One seed per regime provides attribution, not statistical resource dominance']}, f, indent=2)
        f.write('\n')
    for case, value in cases.items():
        print(case, value['trials'], 'diagnostic trials')
        for r in value['records']:
            print(r['engine'], 'CPU ns', r['sum_measured_phase_cpu_ns_including_harness'],
                  'max working bytes', r['max_sampled_working_set_bytes_including_harness'])


if __name__ == '__main__':
    main()
