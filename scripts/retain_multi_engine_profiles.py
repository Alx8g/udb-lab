"""Retain complete, source-identified diagnostics without treating timing as performance."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys

ROOT = next(p for p in Path(__file__).resolve().parents if (p / 'Cargo.toml').is_file())
sys.path.insert(0, str(ROOT / 'scripts'))
from run_spi_profile import validate_profile


def load(path):
    return json.loads(path.read_bytes())


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def retain(source, target):
    env = load(source / 'environment.json')
    summary = load(source / 'summary.json')
    identity = env['binary_identity']
    assert env['diagnostic_only'] is True and identity['profile_enabled'] is True
    assert identity['debug_assertions'] is False
    assert summary['diagnostic_only'] is True and summary['all_full_outputs_match'] is True
    for name, h in env['source_sha256'].items():
        assert hashlib.sha256(subprocess.check_output(['git', 'show', env['git_head'] + ':' + name], cwd=ROOT)).hexdigest() == h, name
    engines = list(env['engine_capabilities'])
    assert 'postgres' not in engines, 'Use separate server-profile accounting and retention'
    assert summary['trials'] == len(engines) * len(env['seeds']) == len(summary['results'])
    expected = {(engine, seed) for engine in engines for seed in env['seeds']}
    assert {(r['engine'], r['seed']) for r in summary['results']} == expected
    selected = [source / 'environment.json', source / 'summary.json']
    for engine, seed in sorted(expected):
        path = source / f'{engine}-seed-{seed}/result.json'
        record = load(path)
        validate_profile(record, engine, env['rows'], seed, env['value_bytes'], env['cache_bytes'])
        for field in ('packed_scan_implementation', 'crc32_implementation'):
            assert record[field] == identity[field]
        entry = next(r for r in summary['results'] if r['engine'] == engine and r['seed'] == seed)
        assert record['diagnostic_phases'] == entry['phases']
        command = source / f'command-{engine}-{seed}.json'
        assert load(command)['exit_code'] == 0
        selected.extend([command, path])
    target.mkdir(parents=True, exist_ok=False)
    hashes = {}
    for path in selected:
        rel = path.relative_to(source)
        dest = target / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        with dest.open('xb') as f:
            f.write(path.read_bytes())
        hashes[rel.as_posix()] = digest(dest)
        assert digest(path) == hashes[rel.as_posix()]
    with (target / 'manifest.json').open('x', encoding='utf-8', newline='\n') as f:
        json.dump({'diagnostic_only': True, 'source_commit': env['git_head'], 'sha256': hashes,
                   'limits': env['limits']}, f, indent=2)
        f.write('\n')
    print(target, len(hashes), 'diagnostic evidence files validated and retained')


if __name__ == '__main__':
    if not __debug__:
        raise RuntimeError('Python -O must not disable evidence assertions')
    retain(Path(sys.argv[1]), Path(sys.argv[2]))
