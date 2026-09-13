"""Reject relaxed durability, missing runtime identity and invalid external metrics."""
import copy
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
from validate_engine_result import external_inputs, validate_external_stats


STATS = {
    'sqlite': {'sqlite_version': 'fixture', 'journal_mode': 'WAL', 'synchronous': 'FULL'},
    'redb': {'engine_version': 'redb 4.1.0', 'durability': 'Immediate'},
    'lmdb': {'engine_version': 'LMDB via lmdb-rkv 0.14.0',
             'durability': 'default synchronous commit, no relaxed flags', 'configured_cache_bytes': None},
    'rocksdb': {'engine_version': 'rocksdb crate 0.25.0', 'sync': True, 'wal_enabled': True},
    'duckdb': {'duckdb_version': 'v1.5.5', 'actual_memory_limit_bytes': 67108864, 'threads': 1},
    'postgres': {'server_version': '18.4', 'transport': 'loopback TCP, prepared SQL',
                 'physical_scope': 'relation sizes, excludes cluster/WAL',
                 'settings': {'fsync': 'on', 'full_page_writes': 'on', 'synchronous_commit': 'on',
                              'default_transaction_isolation': 'serializable'}},
}


def record(engine):
    stats = {'physical_bytes': 100, 'maintenance': 'declared', 'cache_clear': 'declared', **STATS[engine]}
    return {k: copy.deepcopy(stats) for k in ('after_load', 'after_updates', 'before_maintenance', 'after_maintenance')}


class ExternalResultTests(unittest.TestCase):
    def test_supported_metadata(self):
        for engine in STATS:
            validate_external_stats(record(engine), engine)

    def test_negative_boolean_or_missing_physical_bytes_rejected(self):
        for engine in STATS:
            for value in (-1, True, 0.1, None):
                r = record(engine)
                r['after_load']['physical_bytes'] = value
                with self.assertRaises(RuntimeError):
                    validate_external_stats(r, engine)

    def test_relaxed_settings_rejected(self):
        for engine, field, value in [('sqlite', 'synchronous', 'OFF'), ('redb', 'durability', 'None'),
                                     ('lmdb', 'durability', 'NOSYNC'), ('rocksdb', 'sync', False),
                                     ('rocksdb', 'wal_enabled', False), ('duckdb', 'duckdb_version', 'v0.0')]:
            r = record(engine)
            r['after_updates'][field] = value
            with self.assertRaises(RuntimeError):
                validate_external_stats(r, engine)
        r = record('postgres')
        r['before_maintenance']['settings']['fsync'] = 'off'
        with self.assertRaises(RuntimeError):
            validate_external_stats(r, 'postgres')

    def test_duckdb_dependency_hash_detects_library_changes(self):
        scratch = Path(__file__).resolve().parents[1] / '.working/tmp/dependency-tests'
        scratch.mkdir(parents=True, exist_ok=True)
        p = Path(tempfile.mkdtemp(dir=scratch)) / 'fixture.dll'
        p.write_bytes(b'not loaded by this test')
        with patch.dict(os.environ, {'SPI_DUCKDB_LIBRARY': str(p)}):
            a = external_inputs(['duckdb'])
            self.assertEqual(len(a['duckdb']['sha256']), 64)
            p.write_bytes(b'changed fixture')
            self.assertNotEqual(a, external_inputs(['duckdb']))
        with patch.dict(os.environ, {}, clear=True):
            with self.assertRaises(ValueError):
                external_inputs(['duckdb'])
            with self.assertRaises(ValueError):
                external_inputs(['postgres'])
        self.assertEqual(external_inputs(['spi', 'redb']), {})


if __name__ == '__main__':
    unittest.main()
