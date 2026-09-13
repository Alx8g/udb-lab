"""Validate comparison labels and unavailable controls without running engines."""
import copy
import json
import unittest
from unittest.mock import patch

from validate_engine_result import load_capabilities, metric_qualification, validate_engine_name


class CapabilityTests(unittest.TestCase):
    def test_spi_aliases_and_optional_kv_engines(self):
        for name in ('spi', 'spi-packed', 'spi-grouped', 'spi-unbuffered', 'spi-value-cache'):
            record = validate_engine_name(name, shared_kv=True)
            self.assertEqual(record['canonical_engine'], 'spi')
        for name in ('redb', 'lmdb', 'rocksdb', 'duckdb', 'postgres', 'sqlite'):
            self.assertEqual(validate_engine_name(name, shared_kv=True)['engine'], name)
        with self.assertRaises(ValueError):
            validate_engine_name('not-an-adapter')

    def test_unavailable_cache_controls_are_not_ranked(self):
        for name in ('lmdb', 'postgres'):
            self.assertTrue(metric_qualification(name, 'cleared_app_cache_p50_us').startswith('not comparable'))
        for name in ('spi', 'spi-packed', 'redb', 'rocksdb', 'sqlite', 'duckdb'):
            self.assertIsNone(metric_qualification(name, 'cleared_app_cache_p50_us'))

    def test_lifecycle_scopes_and_transport_are_explicit(self):
        self.assertIn('NOT compaction', metric_qualification('lmdb', 'maintenance_ms'))
        self.assertIn('NOT server restart', metric_qualification('postgres', 'reopen_ms'))
        self.assertIn('cluster/WAL', metric_qualification('postgres', 'final_owned_file_bytes'))
        self.assertIn('loopback', validate_engine_name('postgres')['qualification'])
        self.assertIn('NOT a typed analytics benchmark', validate_engine_name('duckdb')['qualification'])

    def test_missing_or_false_type_declarations_rejected(self):
        valid = load_capabilities()
        for field in ('durability', 'maintenance', 'cache', 'physical_scope'):
            bad = copy.deepcopy(valid)
            bad['engines']['rocksdb'].pop(field)
            with patch('validate_engine_result.CAPABILITIES') as path:
                path.read_text.return_value = json.dumps(bad)
                with self.assertRaises(ValueError):
                    load_capabilities()
        for value in (None, 'false', 0):
            bad = copy.deepcopy(valid)
            bad['engines']['lmdb']['cache_clear_supported'] = value
            with patch('validate_engine_result.CAPABILITIES') as path:
                path.read_text.return_value = json.dumps(bad)
                with self.assertRaises(ValueError):
                    load_capabilities()

    def test_unsupported_spi_domains_are_not_omitted(self):
        gaps = load_capabilities()['coverage_gaps']
        self.assertTrue({'general SQL/OLTP', 'analytical SQL', 'vector search',
                         'full-text search', 'distributed transactions'} <= {g['domain'] for g in gaps})
        self.assertTrue(all(g['reason'] and g['engines'] for g in gaps))


if __name__ == '__main__':
    unittest.main()
