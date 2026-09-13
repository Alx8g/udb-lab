"""Run a source-identified PostgreSQL BYTEA campaign on a new owned local cluster.

Never connects to an existing application cluster. Uses a random password, binds
only loopback, preserves all trial databases and stops its owned server in finally.
PostgreSQL timings include TCP/prepared SQL; they are not embedded-call timings.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import os
import secrets
import socket
import subprocess
import sys
import time
from pathlib import Path

from run_spi_campaign import sha256, write_json
from spi_benchmark_identity import inspect_binary
from validate_engine_result import validate_engine_name


def binary_path(directory: Path, name: str) -> Path:
    return (directory / (name + ('.exe' if os.name == 'nt' else ''))).resolve(strict=True)


def config_string(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def free_loopback_port() -> int:
    # Release before startup; PostgreSQL's bind is the authoritative port guard.
    # A race produces a startup error, never reuse of another cluster.
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as stream:
        stream.bind(('127.0.0.1', 0))
        return stream.getsockname()[1]


def directory_bytes(root: Path) -> int:
    return sum(p.stat().st_size for p in root.rglob('*') if p.is_file())


def validate_server_version(version: str) -> None:
    if version != 'postgres (PostgreSQL) 18.4':
        raise ValueError('PostgreSQL 18.4 official runtime required')


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--pg-bin', type=Path, required=True)
    parser.add_argument('--adapter-test-binary', type=Path, help='Optional compiled spi-compare test executable for ignored PostgreSQL boundary test')
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--rows', type=int, default=2000)
    parser.add_argument('--value-bytes', type=int, default=64)
    parser.add_argument('--seeds', type=int, nargs='+', default=[17, 29, 43])
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    executable = args.binary.resolve(strict=True)
    identity = inspect_binary(executable, root, ['postgres'])
    if args.rows < 100 or not 1 <= args.value_bytes <= 4096:
        parser.error('rows>=100 and 1<=value-bytes<=4096 required')
    if len(set(args.seeds)) != len(args.seeds):
        parser.error('duplicate seeds')
    if subprocess.check_output(['git', 'status', '--porcelain'], cwd=root).strip():
        parser.error('commit benchmark source before campaign')
    pg_bin = args.pg_bin.resolve(strict=True)
    initdb, pgctl, postgres, psql = [binary_path(pg_bin, name) for name in ('initdb', 'pg_ctl', 'postgres', 'psql')]
    version_run = subprocess.run([str(postgres), '--version'], cwd=root, capture_output=True,
                                 text=True, encoding='utf-8', check=True, timeout=15)
    validate_server_version(version_run.stdout.strip())
    test_binary = args.adapter_test_binary.resolve(strict=True) if args.adapter_test_binary else None
    source_hashes = {**identity['source_sha256']}
    for name in ('scripts/run_spi_postgres_campaign.py', 'scripts/run_spi_campaign.py',
                 'scripts/spi_benchmark_identity.py', 'scripts/validate_engine_result.py',
                 'scripts/engine_capabilities.json'):
        source_hashes[name] = sha256(root / name)
    executable_hash = sha256(executable)
    output = args.out.resolve()
    # Credentials and cluster state belong in ignored scratch beside this repo.
    if not output.is_relative_to(root / '.working/tmp'):
        parser.error('cluster output must be a new directory under this project .working/tmp')
    output.mkdir(parents=True, exist_ok=False)
    data = output / 'cluster'
    port = free_loopback_port()
    marker = 'spi_benchmark_' + secrets.token_hex(16)
    password = secrets.token_urlsafe(32)
    password_file = output / 'private-password.txt'
    # 0600 on POSIX; on Windows the file inherits the private user-workspace ACL.
    fd = os.open(password_file, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    with os.fdopen(fd, 'w', encoding='utf-8', newline='\n') as stream:
        stream.write(password + '\n')
    server_env = os.environ.copy()
    for name in list(server_env):
        if name.startswith('PG') or name.startswith('SPI_PG_'):
            del server_env[name]
    server_env['PATH'] = str(pg_bin) + os.pathsep + server_env.get('PATH', '')
    commands = []

    def command(argv: list[str], *, env=None, timeout=120):
        result = subprocess.run(argv, cwd=root, env=env or server_env, capture_output=True,
                                text=True, encoding='utf-8', errors='replace', timeout=timeout)
        # Generated credentials are never persisted into command evidence.
        evidence = {'command': argv, 'exit_code': result.returncode,
                    'stdout': result.stdout.replace(password, '[REDACTED]'),
                    'stderr': result.stderr.replace(password, '[REDACTED]')}
        commands.append(evidence)
        write_json(output / f'command-{len(commands):02d}.json', evidence)
        if result.returncode:
            raise RuntimeError(f'{Path(argv[0]).name} failed; see {output}/command-{len(commands):02d}.json')
        return result.stdout.strip()

    postgres_version = command([str(postgres), '--version'])
    server_files = {p.name: sha256(p) for p in pg_bin.iterdir()
                    if p.is_file() and (p.suffix.lower() in ('.exe', '.dll') or p in (initdb, pgctl, postgres, psql))}
    write_json(output / 'environment.json', {
        'git_head': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip(),
        'binary_sha256': executable_hash, 'binary_identity': identity,
        'git_status': '', 'engines': ['postgres'],
        'source_sha256': source_hashes,
        'adapter_test_sha256': sha256(test_binary) if test_binary else None,
        'runner_sha256': sha256(Path(__file__)), 'postgres_version': postgres_version,
        'server_bin_sha256': server_files, 'port': port, 'cluster_name': marker,
        'rows': args.rows, 'value_bytes': args.value_bytes, 'cache_bytes': 8 * 1024 * 1024,
        'seeds': args.seeds, 'engine_capabilities': {'postgres': validate_engine_name('postgres', shared_kv=True)},
        'contract': 'Prepared serializable BYTEA KV transactions over loopback TCP. No remote service or application database.',
        'limits': ['Server CPU/memory is not client CPU/memory',
                   'Relation size excludes shared catalogs, cluster WAL and preallocated files',
                   'Reconnect does not measure server restart or crash recovery',
                   'Shared buffers and OS cache remain warm when adapter clear is unavailable',
                   'No SQL feature/analytics comparison with the current binary-KV SPI API'],
    })
    command([str(initdb), '-D', str(data), '-U', 'spi_bench', '--encoding=UTF8', '--locale=C',
             '--auth-local=scram-sha-256', '--auth-host=scram-sha-256', '--pwfile=' + str(password_file)])
    config = output / 'benchmark-postgresql.conf'
    with config.open('x', encoding='utf-8', newline='\n') as stream:
        stream.write('\n'.join([
            'data_directory = ' + config_string(data.as_posix()),
            'hba_file = ' + config_string((data / 'pg_hba.conf').as_posix()),
            'ident_file = ' + config_string((data / 'pg_ident.conf').as_posix()),
            "listen_addresses = '127.0.0.1'", f'port = {port}',
            'cluster_name = ' + config_string(marker),
            "shared_buffers = '64MB'", "work_mem = '4MB'", 'max_connections = 10',
            'fsync = on', 'full_page_writes = on', 'synchronous_commit = on',
            "password_encryption = 'scram-sha-256'", 'ssl = off',
            'logging_collector = off', 'log_statement = none',
        ]) + '\n')
    cluster_manifest = output / 'cluster-identity.json'
    write_json(cluster_manifest, {'schema': 1, 'host': '127.0.0.1', 'port': port,
               'cluster_name': marker, 'data_directory': str(data.resolve()),
               'postgres_version': postgres_version, 'config_sha256': sha256(config)})
    auth_env = server_env.copy()
    auth_env['PGPASSWORD'] = password
    auth_env['SPI_PG_PASSWORD'] = password
    auth_env['SPI_PG_PORT'] = str(port)
    auth_env['SPI_PG_CLUSTER_FILE'] = str(cluster_manifest)
    started = False
    startup_attempted = False
    run_started = time.perf_counter_ns()
    try:
        startup_attempted = True
        command([str(pgctl), '-D', str(data), '-l', str(output / 'postgres-server.log'), '-w', '-t', '60',
                 '-o', '-c config_file="' + config.as_posix() + '"', 'start'], timeout=90)
        started = True
        values = command([str(psql), '-h', '127.0.0.1', '-p', str(port), '-U', 'spi_bench', '-d', 'postgres',
                          '-X', '-A', '-t', '-v', 'ON_ERROR_STOP=1', '-c',
                          "SELECT current_setting('cluster_name'), current_setting('data_directory'), current_setting('fsync'), current_setting('full_page_writes')"], env=auth_env)
        fields = values.split('|')
        if len(fields) != 4 or fields[0] != marker or Path(fields[1]).resolve() != data.resolve() or fields[2:] != ['on', 'on']:
            raise RuntimeError('isolated PostgreSQL cluster identity/settings mismatch')
        if test_binary is not None:
            test_output = command([str(test_binary), '--exact',
                                   'adapter_tests::postgres_complete_outputs_and_reopen', '--ignored',
                                   '--test-threads=1'], env=auth_env, timeout=120)
            if '1 passed; 0 failed' not in test_output:
                raise RuntimeError('PostgreSQL adapter test did not execute exactly one passing test')
        command([sys.executable, str(root / 'scripts/run_spi_campaign.py'), '--binary', str(executable),
                 '--out', str(output / 'trials'), '--rows', str(args.rows), '--value-bytes', str(args.value_bytes),
                 '--seeds', *map(str, args.seeds), '--engines', 'postgres'], env=auth_env, timeout=600)
    finally:
        # Never stop a process by image name or connect to an unrelated server.
        # Only this fresh data directory can be targeted. Preserve cluster files.
        try:
            if started or (startup_attempted and (data / 'postmaster.pid').exists()):
                command([str(pgctl), '-D', str(data), '-m', 'fast', '-w', '-t', '60', 'stop'], timeout=90)
        finally:
            write_json(output / 'lifecycle.json', {
                'startup_succeeded': started, 'elapsed_ns_including_startup_shutdown': time.perf_counter_ns() - run_started,
                'server_stopped': not (data / 'postmaster.pid').exists(),
                'retained_cluster_logical_bytes': directory_bytes(data),
                'private_files': ['private-password.txt'],
                'retention_rule': 'Do not publish credentials, cluster data, raw server logs, or raw config. Retain sanitized command evidence and trial JSON only.',
            })
    if sha256(executable) != executable_hash or any(sha256(root / n) != h for n, h in source_hashes.items()):
        raise RuntimeError('PostgreSQL campaign source or executable changed')
    if subprocess.check_output(['git', 'status', '--porcelain'], cwd=root).strip():
        raise RuntimeError('PostgreSQL campaign source became dirty')
    if any(sha256(pg_bin / n) != h for n, h in server_files.items()):
        raise RuntimeError('PostgreSQL runtime changed during campaign')
    print('Isolated PostgreSQL campaign passed and owned server stopped. Trial databases retained locally.')


if __name__ == '__main__':
    main()
