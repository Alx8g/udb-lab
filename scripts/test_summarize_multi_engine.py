"""Exercise report acceptance using fixtures and retained embedded evidence."""
import contextlib
import copy
import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = next(p for p in Path(__file__).resolve().parents if (p / 'Cargo.toml').is_file())
SPEC = importlib.util.spec_from_file_location('multi_engine_report', ROOT / 'scripts/summarize_multi_engine.py')
REPORT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(REPORT)
BASE = ROOT / 'results/spi/embedded-kv-98daeeb-v1'


class ReportTests(unittest.TestCase):
    def test_all_retained_embedded_aggregates_and_unsupported_cache(self):
        REPORT.verify_manifest(BASE)
        for name in ['normal', 'small', 'large', 'scale']:
            env, engines = REPORT.case_metrics(BASE / name)
            self.assertEqual(len(engines), 7)
            self.assertEqual(env['seeds'], [17, 29, 43])
            self.assertIsNone(engines['lmdb']['cleared_app_cache_p50_us']['median'])
            self.assertIn('observed_warm_median_not_comparable', engines['lmdb']['cleared_app_cache_p50_us'])
            self.assertEqual(len(engines['spi-packed']['load_ms']['samples']), 3)

    def test_corrupted_manifest_is_rejected(self):
        fixture = ROOT / '.working/tmp/report-fixtures'
        fixture.mkdir(parents=True, exist_ok=True)
        path = Path(tempfile.mkdtemp(dir=fixture))
        (path / 'result.json').write_bytes(b'changed')
        (path / 'manifest.json').write_text(json.dumps({'sha256': {'result.json': 'bad'}}))
        with self.assertRaises(AssertionError): REPORT.verify_manifest(path)

    def test_changed_aggregate_is_rejected(self):
        real_load = REPORT.load
        def load(path):
            value = real_load(path)
            if path.name == 'summary.json':
                value['engines']['spi']['load_ms']['median'] += 1
            return value
        with patch.object(REPORT, 'load', side_effect=load), self.assertRaises(AssertionError):
            REPORT.case_metrics(BASE / 'normal')

    def test_multi_case_report_preserves_contract_and_suppresses_ratios(self):
        fixture = ROOT / '.working/tmp/report-fixtures'
        fixture.mkdir(parents=True, exist_ok=True)
        output = Path(tempfile.mkdtemp(dir=fixture)) / 'generated'
        parent = REPORT.load(BASE / 'environment.json')
        env, engines = REPORT.case_metrics(BASE / 'normal')
        pg = Path('synthetic-postgres')
        pg_env = {**copy.deepcopy(parent), 'rows': 2000, 'value_bytes': 64, 'seeds': [17, 29, 43]}
        pg_metrics = {'postgres': copy.deepcopy(engines['sqlite'])}
        real_load, real_case, real_manifest = REPORT.load, REPORT.case_metrics, REPORT.verify_manifest
        def load(path):
            if path == pg / 'environment.json': return pg_env
            if path == pg / 'trials/summary.json': return {'trials': 3}
            return real_load(path)
        def cases(path):
            if path == pg / 'trials': return env, pg_metrics
            return real_case(path)
        def manifests(path):
            if path == pg: return {'source_commit': 'different-evidence-head'}
            return real_manifest(path)
        real_digest = REPORT.digest
        def digest(path):
            if path == pg / 'manifest.json': return 'synthetic'
            return real_digest(path)
        args = ['report', '--embedded', str(BASE), '--postgres', str(pg), '--out', str(output)]
        with patch.object(sys, 'argv', args), patch.object(REPORT, 'load', side_effect=load), \
             patch.object(REPORT, 'case_metrics', side_effect=cases), patch.object(REPORT, 'verify_manifest', side_effect=manifests), \
             patch.object(REPORT, 'digest', side_effect=digest), contextlib.redirect_stdout(io.StringIO()):
            REPORT.main()
            report = real_load(output / 'gap-matrix.json')
            self.assertEqual(report['embedded_trials'], 84)
            self.assertEqual(report['postgres_trials'], 3)
            comparisons = report['cases']['normal']['comparisons']
            for row in comparisons:
                if row['metric'] in ['maintenance_ms', 'reopen_ms', 'post_compaction_scan_ms', 'final_owned_file_bytes']:
                    self.assertIsNone(row['spi_over_baseline'])
                if row['spi_variant'] == 'spi-packed' and row['metric'] == 'warm_hit_p50_us':
                    self.assertIsNone(row['spi_over_baseline'])
            self.assertTrue(report['coverage_gaps'])
            self.assertIn('postgres', (output / 'operation-metrics.csv').read_text())
            with self.assertRaises(FileExistsError): REPORT.main()
            pg_env['binary_sha256'] = 'changed'
            with self.assertRaises(AssertionError): REPORT.main()


if __name__ == '__main__':
    unittest.main()
