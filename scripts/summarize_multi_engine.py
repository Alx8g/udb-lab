"""Build a per-operation gap matrix from verified raw campaign records.

Does not rank non-equivalent lifecycle/cache metrics. Recomputes trial metrics
rather than copying medians, verifies summary consistency, retains regressions,
and suppresses ratios when either observation is below one microsecond.
"""
import argparse
import csv
import hashlib
import json
import statistics
import sys
from pathlib import Path

ROOT = next(p for p in Path(__file__).resolve().parents if (p / 'Cargo.toml').is_file())
sys.path.insert(0, str(ROOT / 'scripts'))
from run_spi_campaign import validate_record

METRICS = {
    'load_ms': ('load_batches_256', 'total_ns', 1e6),
    'warm_hit_p50_us': ('warm_hits', 'p50_ns', 1e3),
    'warm_hit_p99_us': ('warm_hits', 'p99_ns', 1e3),
    'missing_p50_us': ('misses', 'p50_ns', 1e3),
    'range_p50_us': ('ranges_up_to_100_keys', 'p50_ns', 1e3),
    'reused_16_key_p50_us': ('reused_16_key_hits', 'p50_ns', 1e3),
    'unique_reads_ms': ('unique_value_reads', 'total_ns', 1e6),
    'one_off_scan_ms': ('one_off_scan', 'total_ns', 1e6),
    'updates_ms': ('updates_batches_64', 'total_ns', 1e6),
    'durable_mutation_p50_us': ('single_row_commits', 'p50_ns', 1e3),
    'post_mutation_scan_ms': ('post_mutation_scan', 'total_ns', 1e6),
    'post_mutation_range_p50_us': ('post_mutation_ranges', 'p50_ns', 1e3),
    'post_compaction_scan_ms': ('post_compaction_scan', 'total_ns', 1e6),
    'cleared_app_cache_p50_us': ('application_cache_cleared_hits_not_storage_cold', 'p50_ns', 1e3),
    'reopen_ms': ('reopen_ns', None, 1e6),
    'maintenance_ms': ('maintenance_with_integrity_check_ns', None, 1e6),
    'final_owned_file_bytes': ('after_maintenance', 'physical_bytes', 1),
}


def load(path):
    return json.loads(path.read_bytes())


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def verify_manifest(directory):
    manifest = load(directory / 'manifest.json')
    for name, expected in manifest['sha256'].items():
        path = directory / name
        assert path.resolve().is_relative_to(directory.resolve())
        assert digest(path) == expected, name
    return manifest


def case_metrics(directory):
    env = load(directory / 'environment.json')
    summary = load(directory / 'summary.json')
    result = {}
    for engine, accepted in summary['engines'].items():
        records = [load(directory / f'{engine}-seed-{seed}' / 'result.json') for seed in env['seeds']]
        for seed, record in zip(env['seeds'], records):
            validate_record(record, engine, env['rows'], seed, env['value_bytes'], env['cache_bytes'])
        metrics = {}
        for metric, (phase, field, divisor) in METRICS.items():
            if any(phase not in record for record in records):
                continue
            if field == 'physical_bytes':
                samples = [r[phase][field] / divisor for r in records]
            else:
                samples = [(r[phase][field] if field else r[phase]) / divisor for r in records]
            median = statistics.median(samples)
            entry = {'samples': samples, 'median': median, 'min': min(samples), 'max': max(samples)}
            published = accepted.get(metric)
            if published:
                assert published['samples'] == samples, (engine, metric, 'samples')
                if published['median'] is not None:
                    assert median == published['median'], (engine, metric, 'median')
                else:
                    entry['median'] = None
                    entry['observed_warm_median_not_comparable'] = median
                if 'qualification' in published:
                    entry['qualification'] = published['qualification']
            metrics[metric] = entry
        result[engine] = metrics
    return env, result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--embedded', type=Path, required=True)
    parser.add_argument('--postgres', type=Path, nargs='+', required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    if not __debug__:
        raise RuntimeError('Run without Python -O: evidence assertions must remain enabled')
    manifests = {str(path): verify_manifest(path) for path in [args.embedded, *args.postgres]}
    embedded_env = load(args.embedded / 'environment.json')
    postgres_cases = {}
    for directory in args.postgres:
        postgres_env = load(directory / 'environment.json')
        # An evidence-only commit may advance HEAD between serialized campaigns.
        assert embedded_env['binary_sha256'] == postgres_env['binary_sha256']
        assert embedded_env['binary_identity'] == postgres_env['binary_identity']
        shared = embedded_env['source_sha256'].keys() & postgres_env['source_sha256'].keys()
        assert all(embedded_env['source_sha256'][n] == postgres_env['source_sha256'][n] for n in shared)
        env, metrics = case_metrics(directory / 'trials')
        postgres_cases[str(directory)] = {'evidence_head': manifests[str(directory)]['source_commit'],
            'rows': env['rows'], 'value_bytes': env['value_bytes'], 'seeds': env['seeds'],
            'trials': load(directory / 'trials/summary.json')['trials'], 'engines': metrics}
        assert set(metrics) == {'postgres'}
    cases = {}
    rows = []
    for case in embedded_env['cases']:
        env, engines = case_metrics(args.embedded / case)
        comparisons = []
        for variant in ('spi', 'spi-packed'):
            for other in ('sqlite', 'redb', 'lmdb', 'rocksdb'):
                for metric, own in engines[variant].items():
                    external = engines[other][metric]
                    a, b = own['median'], external['median']
                    qualified = metric in ('cleared_app_cache_p50_us', 'reopen_ms', 'maintenance_ms', 'post_compaction_scan_ms', 'final_owned_file_bytes')
                    below_floor = (metric.endswith('_us') and a is not None and b is not None and min(a, b) < 1)
                    ratio = a/b if a is not None and b is not None and b > 0 and not qualified and not below_floor else None
                    comparisons.append({'spi_variant': variant, 'baseline': other, 'metric': metric,
                                        'spi_median': a, 'baseline_median': b,
                                        'spi_over_baseline': ratio,
                                        'interpretation': 'greater than 1 means SPI slower' if ratio is not None else 'ratio suppressed: lifecycle/cache scope or sub-microsecond floor'})
        cases[case] = {'rows': env['rows'], 'value_bytes': env['value_bytes'], 'requested_cache_bytes': env['cache_bytes'],
                       'engines': engines, 'comparisons': comparisons}
        for engine, metrics in engines.items():
            for metric, value in metrics.items():
                rows.append([case, engine, metric, value['median'], value['min'], value['max'], value.get('qualification', '')])
    for case, record in postgres_cases.items():
        for metric, value in record['engines']['postgres'].items():
            rows.append([case, 'postgres', metric, value['median'], value['min'], value['max'], value.get('qualification', '')])
    output = {'schema': 1, 'source_commit': manifests[str(args.embedded)]['source_commit'],
              'report_generator_sha256': digest(Path(__file__)),
              'report_generator': str(Path(__file__).relative_to(ROOT)),
              'binary_sha256': embedded_env['binary_sha256'],
              'cross_campaign_identity': 'Byte-identical executable and compiled sources; evidence-only HEAD advancement allowed',
              'source_manifests': {str(p): digest(p / 'manifest.json') for p in [args.embedded, *args.postgres]},
              'embedded_trials': load(args.embedded / 'summary.json')['trials'],
              'postgres_trials': sum(c['trials'] for c in postgres_cases.values()),
              'cases': cases, 'postgres_loopback_bytea': postgres_cases,
              'coverage_gaps': embedded_env['coverage_gaps'],
              'limits': ['Medians of three seed-level observations, not significance or causal speedups.',
                         'Sequential closed-loop KV operations, no concurrent throughput or offered-load tail latency.',
                         'DuckDB BLOB and PostgreSQL BYTEA results are characterization only, not analytics/SQL parity.',
                         'PostgreSQL transport and server work are included in elapsed client timings.',
                         'Engine cache settings are not equal total memory or OS-cache budgets.',
                         'Native maintenance/reopen and file-size scopes differ.',
                         'Sub-microsecond per-operation ratios are suppressed.',
                         'No CPU/allocation/RSS inference from release elapsed time. Diagnostics are separate.',
                         'No universal winner, production readiness or power-loss parity is implied.']}
    args.out.mkdir(parents=True, exist_ok=False)
    with (args.out / 'gap-matrix.json').open('x', encoding='utf-8', newline='\n') as f:
        json.dump(output, f, indent=2)
        f.write('\n')
    with (args.out / 'operation-metrics.csv').open('x', encoding='utf-8', newline='') as f:
        writer = csv.writer(f)
        writer.writerow(['case', 'engine', 'metric', 'median', 'min', 'max', 'qualification'])
        writer.writerows(rows)
    print('Raw outputs, source manifests and aggregate metrics verified.')
    for case, value in cases.items():
        print(case)
        for engine, metrics in value['engines'].items():
            selected = ['load_ms', 'warm_hit_p50_us', 'one_off_scan_ms', 'updates_ms', 'durable_mutation_p50_us']
            print(engine, ' '.join(f'{m}={metrics[m]["median"]:.4g}' for m in selected))
    for case, record in postgres_cases.items():
        print('postgres separate:', case, {m: v['median'] for m, v in record['engines']['postgres'].items()})


if __name__ == '__main__':
    main()
