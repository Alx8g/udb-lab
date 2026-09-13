"""Test diagnostic scope, missing counters, source hashes and resource totals."""
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

ROOT = next(p for p in Path(__file__).resolve().parents if (p / 'Cargo.toml').is_file())
sys.path.insert(0, str(ROOT / 'scripts'))
from test_spi_profile import profile_result

SPEC = importlib.util.spec_from_file_location('resource_report', ROOT / 'scripts/summarize_engine_profiles.py')
REPORT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(REPORT)


class ResourceTests(unittest.TestCase):
    def fixture(self):
        root = ROOT / '.working/tmp/synthetic-resource-report'
        record = profile_result()
        env = {'git_head': 'fixture', 'engine_capabilities': {'spi': {}}, 'seeds': [17],
               'rows': 2000, 'value_bytes': 64, 'cache_bytes': 8192, 'limits': ['diagnostic only']}
        objects = {'manifest.json': {'diagnostic_only': True, 'sha256': {'environment.json': 'digest'}},
                   'environment.json': env, 'spi-seed-17/result.json': record}
        return root, record, objects

    def summarize(self, root, objects):
        with patch.object(REPORT, 'load', side_effect=lambda p: copy.deepcopy(objects[p.relative_to(root).as_posix()])), \
             patch.object(REPORT, 'digest', return_value='digest'):
            return REPORT.summarize(root)

    def test_zero_cpu_and_missing_memory_stay_explicit(self):
        root, record, objects = self.fixture()
        result = self.summarize(root, objects)['records'][0]
        self.assertEqual(result['sum_measured_phase_cpu_ns_including_harness'], 0)
        self.assertIsNone(result['max_sampled_working_set_bytes_including_harness'])
        self.assertIsNone(result['max_sampled_private_commit_bytes_including_harness'])
        self.assertEqual(result['sum_phase_rust_allocation_requests_bytes'], 100 * len(record['diagnostic_phases']))
        self.assertIn('spi_publication_attribution', result)
        record['diagnostic_phases']['scan']['process_cpu_ns_including_harness'] = None
        self.assertIsNone(self.summarize(root, objects)['records'][0]['sum_measured_phase_cpu_ns_including_harness'])

    def test_memory_is_maximum_of_phase_samples_not_engine_only(self):
        root, record, objects = self.fixture()
        record['diagnostic_phases']['scan']['os_memory_after_phase'] = {'working_set_bytes': 1000, 'private_commit_bytes': 500}
        record['diagnostic_phases']['load']['os_memory_after_phase'] = {'working_set_bytes': 900}
        result = self.summarize(root, objects)['records'][0]
        self.assertEqual(result['max_sampled_working_set_bytes_including_harness'], 1000)
        self.assertEqual(result['max_sampled_private_commit_bytes_including_harness'], 500)

    def test_performance_manifest_and_hash_mismatch_rejected(self):
        root, record, objects = self.fixture()
        objects['manifest.json']['diagnostic_only'] = False
        with self.assertRaisesRegex(ValueError, 'Only diagnostic'): self.summarize(root, objects)
        objects['manifest.json']['diagnostic_only'] = True
        objects['manifest.json']['sha256']['environment.json'] = 'changed'
        with self.assertRaisesRegex(ValueError, 'Manifest mismatch'): self.summarize(root, objects)

    def test_result_contract_is_validated_before_reporting(self):
        root, record, objects = self.fixture()
        record['diagnostic_only'] = False
        with self.assertRaises(RuntimeError): self.summarize(root, objects)


if __name__ == '__main__':
    unittest.main()
