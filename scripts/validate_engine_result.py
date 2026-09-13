"""Capability boundary for current binary-KV comparisons, not feature parity."""
from __future__ import annotations
import json
import hashlib
import os
from pathlib import Path

CAPABILITIES = Path(__file__).with_name('engine_capabilities.json')
ROOT = CAPABILITIES.parents[1]


def load_capabilities() -> dict:
    value = json.loads(CAPABILITIES.read_text(encoding='utf-8'))
    if value.get('schema') != 2 or not isinstance(value.get('engines'), dict):
        raise ValueError('invalid engine capability schema')
    if not isinstance(value.get('coverage_gaps'), list) or not value['coverage_gaps']:
        raise ValueError('missing unsupported-domain declarations')
    required = {'class', 'adapter_status', 'common_kv_workload', 'durability',
                'cache_clear_supported', 'cache', 'maintenance', 'reopen', 'physical_scope'}
    for name, record in value['engines'].items():
        if not isinstance(record, dict) or not required <= record.keys():
            raise ValueError(f'incomplete capability record: {name}')
        if type(record['common_kv_workload']) is not bool or type(record['cache_clear_supported']) is not bool:
            raise ValueError(f'invalid capability boolean: {name}')
        for field in required - {'common_kv_workload', 'cache_clear_supported'}:
            if not isinstance(record[field], str) or not record[field]:
                raise ValueError(f'invalid capability {field}: {name}')
    return value


def validate_engine_name(name: str, *, shared_kv: bool = False) -> dict:
    value = load_capabilities()
    canonical = 'spi' if name in value['spi_aliases'] else name
    if canonical not in value['engines']:
        raise ValueError(f'engine has no declared capability record: {name}')
    record = value['engines'][canonical]
    if shared_kv and not record['common_kv_workload']:
        raise ValueError(f'{name} cannot execute the common KV operation stream')
    return {**record, 'engine': name, 'canonical_engine': canonical}


def _digest(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def external_inputs(engines: list[str]) -> dict:
    result = {}
    if 'duckdb' in engines:
        value = os.environ.get('SPI_DUCKDB_LIBRARY')
        if not value:
            raise ValueError('SPI_DUCKDB_LIBRARY must name the official DuckDB library')
        path = Path(value).resolve(strict=True)
        if not path.is_file():
            raise ValueError('DuckDB library path is not a file')
        result['duckdb'] = {'library_path': str(path), 'sha256': _digest(path),
                            'expected_version': 'v1.5.5'}
    if 'postgres' in engines:
        value = os.environ.get('SPI_PG_CLUSTER_FILE')
        if not value or not os.environ.get('SPI_PG_PASSWORD'):
            raise ValueError('owned PostgreSQL cluster identity and credentials required')
        path = Path(value).resolve(strict=True)
        if not path.is_relative_to(ROOT / '.working/tmp'):
            raise ValueError('PostgreSQL identity must be project-local scratch')
        marker = json.loads(path.read_text(encoding='utf-8'))
        if (marker.get('schema') != 1 or marker.get('host') != '127.0.0.1'
                or str(marker.get('port')) != os.environ.get('SPI_PG_PORT')):
            raise ValueError('invalid PostgreSQL cluster identity')
        data = Path(marker['data_directory']).resolve(strict=True)
        if data != (path.parent / 'cluster').resolve(strict=True):
            raise ValueError('PostgreSQL marker does not own the data directory')
        config = path.parent / 'benchmark-postgresql.conf'
        if _digest(config) != marker['config_sha256']:
            raise ValueError('owned PostgreSQL configuration changed')
        result['postgres'] = {'identity_sha256': _digest(path), 'cluster_name': marker['cluster_name'],
                              'version': marker['postgres_version'], 'port': marker['port'],
                              'config_sha256': marker['config_sha256']}
    return result


def validate_external_stats(record: dict, engine: str) -> None:
    """Verify recorded adapter settings, not assert full engine semantic parity."""
    def require(ok, field):
        if not ok:
            raise RuntimeError(f'{engine} invalid adapter contract: {field}')
    for phase in ('after_load', 'after_updates', 'before_maintenance', 'after_maintenance'):
        stats = record.get(phase)
        require(isinstance(stats, dict), phase)
        require(type(stats.get('physical_bytes')) is int and stats['physical_bytes'] >= 0, 'physical_bytes')
        if engine == 'sqlite':
            require(stats.get('journal_mode') == 'WAL' and stats.get('synchronous') == 'FULL', 'durability')
            require(isinstance(stats.get('sqlite_version'), str), 'version')
        elif engine == 'redb':
            require(stats.get('engine_version') == 'redb 4.1.0', 'version')
            require(stats.get('durability') == 'Immediate', 'durability')
        elif engine == 'lmdb':
            require(stats.get('engine_version') == 'LMDB via lmdb-rkv 0.14.0', 'version')
            require(stats.get('durability') == 'default synchronous commit, no relaxed flags', 'durability')
            require(stats.get('configured_cache_bytes') is None, 'uncontrolled mmap cache')
        elif engine == 'rocksdb':
            require(stats.get('sync') is True and stats.get('wal_enabled') is True, 'sync/WAL')
            require('0.25.0' in stats.get('engine_version',''), 'version')
        elif engine == 'duckdb':
            require(stats.get('duckdb_version') == 'v1.5.5', 'version')
            require(type(stats.get('actual_memory_limit_bytes')) is int and stats['actual_memory_limit_bytes'] >= 67108864, 'memory floor')
            require(stats.get('threads') == 1, 'threads')
        elif engine == 'postgres':
            settings = stats.get('settings', {})
            require(all(settings.get(k) == 'on' for k in ('fsync','full_page_writes','synchronous_commit')), 'durability')
            require(settings.get('default_transaction_isolation') == 'serializable', 'isolation')
            require(isinstance(stats.get('server_version'), str), 'version')
            require(stats.get('transport') == 'loopback TCP, prepared SQL', 'transport')
            require('excludes cluster/WAL' in stats.get('physical_scope',''), 'relation scope')
        else:
            raise RuntimeError(f'no external adapter contract for {engine}')
        if engine not in ('sqlite',):
            for field in ('maintenance', 'cache_clear'):
                require(isinstance(stats.get(field), str) and bool(stats[field]), field)


def metric_qualification(engine: str, metric: str) -> str | None:
    record = validate_engine_name(engine, shared_kv=True)
    if metric == 'cleared_app_cache_p50_us' and not record['cache_clear_supported']:
        return 'not comparable: adapter has no application-cache clear, raw warm observations retained'
    if metric == 'maintenance_ms':
        return record['maintenance']
    if metric == 'reopen_ms':
        return record['reopen']
    if metric == 'final_owned_file_bytes':
        return record['physical_scope']
    return None


if __name__ == '__main__':
    for name in load_capabilities()['engines']:
        validate_engine_name(name, shared_kv=True)
    print('engine capability schema PASS')
