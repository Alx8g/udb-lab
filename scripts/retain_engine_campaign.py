"""Verify completed engine campaign provenance and retain only allowlisted evidence.

Never overwrites a destination. PostgreSQL credentials, cluster data, configs and
server logs are deliberately excluded. Raw JSON bytes stay identical. The native
result checker must be run from the same benchmark implementation as the campaign.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import subprocess
from pathlib import Path

from run_spi_campaign import validate_record, write_json


def digest(path: Path) -> str:
    h = hashlib.sha256()
    with path.open('rb') as f:
        for data in iter(lambda: f.read(1024 * 1024), b''):
            h.update(data)
    return h.hexdigest()


def load(path: Path):
    return json.loads(path.read_text(encoding='utf-8'))


def check_sources(root: Path, env: dict):
    head = env['git_head']
    if env.get('git_status') != '':
        raise ValueError('dirty or missing source status')
    identity = env['binary_identity']
    hashes = dict(identity['source_sha256'])
    for name, expected in env.get('source_sha256', {}).items():
        if name in hashes and hashes[name] != expected:
            raise ValueError('source hash differs from binary identity: ' + name)
        hashes[name] = expected
    if 'runner_sha256' in env:
        hashes['scripts/run_spi_postgres_campaign.py'] = env['runner_sha256']
    for name, expected in hashes.items():
        if Path(name).is_absolute() or '..' in Path(name).parts:
            raise ValueError('unsafe source path in evidence')
        data = subprocess.check_output(['git', 'show', head + ':' + name], cwd=root)
        if hashlib.sha256(data).hexdigest() != expected:
            raise ValueError('committed source mismatch: ' + name)
    if identity['debug_assertions'] is not False or identity['profile_enabled'] is not False:
        raise ValueError('non-release campaign evidence rejected')


def child_files(root: Path, child: Path, parent_env: dict) -> list[Path]:
    env = load(child / 'environment.json')
    check_sources(root, env)
    if env['git_head'] != parent_env['git_head'] or env['binary_sha256'] != parent_env['binary_sha256']:
        raise ValueError('child source/binary differs from campaign')
    if env['git_status'] != '':
        raise ValueError('dirty-source benchmark evidence')
    summary = load(child / 'summary.json')
    engines = list(summary['engines'])
    declared_engines = parent_env.get('engines', ['postgres'])
    if set(engines) != set(declared_engines) or set(env.get('engine_capabilities', {})) != set(engines):
        raise ValueError('engine coverage mismatch')
    seeds = env['seeds']
    if seeds != parent_env['seeds'] or not seeds or len(set(seeds)) != len(seeds):
        raise ValueError('seed coverage mismatch')
    if 'cases' in parent_env:
        expected_case = parent_env['cases'][child.name]
        if isinstance(expected_case, dict):
            expected_case = [expected_case['rows'], expected_case['value_bytes'], expected_case['cache_bytes']]
        if [env['rows'], env['value_bytes'], env['cache_bytes']] != list(expected_case):
            raise ValueError('workload coverage mismatch')
    elif (env['rows'], env['value_bytes']) != (parent_env['rows'], parent_env['value_bytes']):
        raise ValueError('PostgreSQL workload mismatch')
    if summary['trials'] != len(engines) * len(seeds) or summary['all_full_outputs_match'] is not True:
        raise ValueError('incomplete campaign')
    selected = [child / 'environment.json', child / 'summary.json']
    for engine in engines:
        if engine not in env['binary_identity']['compiled_engines']:
            raise ValueError('uncompiled engine in evidence')
        for seed in seeds:
            command = child / f'command-{engine}-{seed}.json'
            result = child / f'{engine}-seed-{seed}' / 'result.json'
            cmd = load(command)
            if cmd['exit_code'] != 0:
                raise ValueError('failed trial')
            record = load(result)
            validate_record(record, engine, env['rows'], seed, env['value_bytes'], env['cache_bytes'])
            for field in ('crc32_implementation', 'packed_scan_implementation'):
                if record[field] != env['binary_identity'][field]:
                    raise ValueError('trial implementation mismatch')
            selected += [command, result]
    return selected


def evidence_files(root: Path, source: Path, postgres: bool) -> list[Path]:
    env = load(source / 'environment.json')
    check_sources(root, env)
    selected = [source / 'environment.json']
    if postgres:
        lifecycle = load(source / 'lifecycle.json')
        if lifecycle.get('startup_succeeded') is not True or lifecycle.get('server_stopped') is not True:
            raise ValueError('PostgreSQL campaign did not finish on a stopped owned server')
        selected += [source / 'lifecycle.json']
        selected += child_files(root, source / 'trials', env)
        # Retain only wrapper command JSON explicitly produced by the owned
        # lifecycle. Never recurse into the data/config/password/log files.
        commands = sorted(source.glob('command-[0-9][0-9].json'))
        if not commands or any(load(p)['exit_code'] != 0 for p in commands):
            raise ValueError('missing or failed PostgreSQL lifecycle command')
        selected += commands
    else:
        summary = load(source / 'summary.json')
        if summary.get('all_full_outputs_match') is not True:
            raise ValueError('incomplete top-level campaign')
        if set(summary['cases']) != set(env['cases']) or not env['cases']:
            raise ValueError('missing declared workload cases')
        selected += [source / 'summary.json']
        total = 0
        for case in env['cases']:
            if not case or '/' in case or '\\' in case or case in ('.', '..'):
                raise ValueError('unsafe case name')
            selected += child_files(root, source / case, env)
            selected += [source / f'command-{case}.json']
            if load(source / f'command-{case}.json')['exit_code'] != 0:
                raise ValueError('failed child campaign command')
            total += load(source / case / 'summary.json')['trials']
        if total != summary['trials']:
            raise ValueError('trial total mismatch')
    for p in selected:
        if not p.resolve().is_relative_to(source.resolve()):
            raise ValueError('evidence escapes campaign directory')
    return selected


def retain(root: Path, source: Path, target: Path, postgres: bool):
    files = evidence_files(root, source, postgres)
    # Extra protection for generated secrets: match the known local secret
    # before copying any allowed JSON. Never print it or store it in a manifest.
    secret_file = source / 'private-password.txt'
    secret = secret_file.read_bytes().strip() if postgres else b''
    if postgres and not secret:
        raise ValueError('PostgreSQL secret scan requires the preserved local password file')
    if secret and any(secret in p.read_bytes() for p in files):
        raise ValueError('secret found in allowlisted evidence; retain nothing')
    if target.exists():
        raise FileExistsError('write-once evidence destination already exists')
    target.mkdir(parents=True, exist_ok=False)
    hashes = {}
    for p in files:
        relative = p.relative_to(source)
        q = target / relative
        q.parent.mkdir(parents=True, exist_ok=True)
        with q.open('xb') as output:
            output.write(p.read_bytes())
        hashes[relative.as_posix()] = digest(q)
        if digest(p) != hashes[relative.as_posix()]:
            raise ValueError('evidence changed during retention')
    write_json(target / 'manifest.json', {
        'schema': 1, 'source_commit': load(source / 'environment.json')['git_head'],
        'diagnostic_only': False, 'postgres_loopback': postgres, 'sha256': hashes,
        'scope': 'Complete output-checked KV evidence, raw bytes preserved. PostgreSQL cluster/credentials/config/logs excluded. Lifecycle/memory contracts differ.'})
    for relative, expected in hashes.items():
        if digest(target / relative) != expected:
            raise ValueError('retained manifest verification failed')
    print(f'{len(hashes)} source-identified evidence files retained and verified.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--postgres', action='store_true')
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    retain(root, args.source.resolve(strict=True), args.out.resolve(), args.postgres)


if __name__ == '__main__':
    main()
