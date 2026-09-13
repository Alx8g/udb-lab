"""Retention boundary tests using synthetic evidence, never a database server."""
import copy
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from retain_engine_campaign import check_sources, child_files, evidence_files, retain

ROOT = Path(__file__).resolve().parents[1]


class RetentionTests(unittest.TestCase):
    def fixture(self):
        parent = ROOT / '.working/tmp/retention-tests'
        parent.mkdir(parents=True, exist_ok=True)
        source = Path(tempfile.mkdtemp(dir=parent))
        identity = {'source_sha256': {'Cargo.toml': hashlib.sha256(b'fixture').hexdigest()},
                    'compiled_engines': ['postgres'], 'debug_assertions': False, 'profile_enabled': False,
                    'crc32_implementation': 'ieee-slicing8', 'packed_scan_implementation': 'direct-delta'}
        env = {'git_head': 'fixture', 'git_status': '', 'binary_sha256': 'fixture',
               'binary_identity': identity, 'engines': ['postgres'], 'seeds': [17],
               'rows': 100, 'value_bytes': 64, 'cache_bytes': 8388608,
               'engine_capabilities': {'postgres': {}}, 'source_sha256': identity['source_sha256']}
        self.put(source / 'environment.json', env)
        self.put(source / 'lifecycle.json', {'startup_succeeded': True, 'server_stopped': True})
        self.put(source / 'command-01.json', {'exit_code': 0})
        child = source / 'trials'
        self.put(child / 'environment.json', env)
        self.put(child / 'summary.json', {'engines': {'postgres': {}}, 'trials': 1, 'all_full_outputs_match': True})
        self.put(child / 'command-postgres-17.json', {'exit_code': 0})
        self.put(child / 'postgres-seed-17/result.json', {'crc32_implementation': 'ieee-slicing8',
                                                     'packed_scan_implementation': 'direct-delta'})
        (source / 'private-password.txt').write_bytes(b'synthetic-not-a-real-credential\n')
        (source / 'postgres-server.log').write_bytes(b'private server log')
        (source / 'benchmark-postgresql.conf').write_bytes(b'private config')
        self.put(source / 'cluster/private.json', {'private': True})
        return source, env

    def put(self, path, obj):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes((json.dumps(obj) + '\r\n').encode())

    def test_source_validation_checks_binary_and_runner_hashes(self):
        _, env = self.fixture()
        with patch('retain_engine_campaign.subprocess.check_output', return_value=b'fixture'):
            check_sources(ROOT, env)
            env['runner_sha256'] = hashlib.sha256(b'changed').hexdigest()
            with self.assertRaisesRegex(ValueError, 'committed source mismatch'):
                check_sources(ROOT, env)

    def test_dirty_profile_unsafe_and_conflicting_sources_rejected(self):
        _, original = self.fixture()
        for mode in ('dirty', 'profile', 'unsafe', 'conflict'):
            env = copy.deepcopy(original)
            if mode == 'dirty': env['git_status'] = ' M Cargo.toml'
            if mode == 'profile': env['binary_identity']['profile_enabled'] = True
            if mode == 'unsafe': env['source_sha256']['../outside'] = 'bad'
            if mode == 'conflict': env['source_sha256'] = {'Cargo.toml': 'different'}
            with self.subTest(mode=mode), patch('retain_engine_campaign.subprocess.check_output', return_value=b'fixture'):
                with self.assertRaises(ValueError): check_sources(ROOT, env)

    @patch('retain_engine_campaign.check_sources')
    @patch('retain_engine_campaign.validate_record')
    def test_allowlist_preserves_raw_bytes_and_refuses_overwrite(self, validate, sources):
        source, _ = self.fixture()
        target = source.parent / (source.name + '-retained')
        retain(ROOT, source, target, True)
        manifest = json.loads((target / 'manifest.json').read_bytes())
        self.assertEqual(len(manifest['sha256']), 7)
        self.assertFalse((target / 'private-password.txt').exists())
        self.assertFalse((target / 'cluster').exists())
        self.assertFalse((target / 'postgres-server.log').exists())
        self.assertFalse((target / 'benchmark-postgresql.conf').exists())
        for name, digest in manifest['sha256'].items():
            self.assertEqual((source / name).read_bytes(), (target / name).read_bytes())
            self.assertEqual(hashlib.sha256((target / name).read_bytes()).hexdigest(), digest)
        validate.assert_called_once()
        with self.assertRaises(FileExistsError): retain(ROOT, source, target, True)

    @patch('retain_engine_campaign.check_sources')
    @patch('retain_engine_campaign.validate_record')
    def test_secret_leak_refuses_destination_creation(self, validate, sources):
        source, _ = self.fixture()
        self.put(source / 'command-01.json', {'exit_code': 0, 'stdout': 'synthetic-not-a-real-credential'})
        target = source / 'not-created'
        with self.assertRaisesRegex(ValueError, 'secret found'):
            retain(ROOT, source, target, True)
        self.assertFalse(target.exists())

    @patch('retain_engine_campaign.check_sources')
    @patch('retain_engine_campaign.validate_record')
    def test_running_server_and_failed_command_rejected(self, validate, sources):
        for mode in ('running', 'command'):
            source, _ = self.fixture()
            if mode == 'running': self.put(source / 'lifecycle.json', {'startup_succeeded': True, 'server_stopped': False})
            else: self.put(source / 'command-01.json', {'exit_code': 1})
            with self.subTest(mode=mode), self.assertRaises(ValueError): evidence_files(ROOT, source, True)

    @patch('retain_engine_campaign.check_sources')
    @patch('retain_engine_campaign.validate_record')
    def test_child_coverage_and_identity_mismatch_rejected(self, validate, sources):
        source, env = self.fixture()
        for field, value in [('engines', ['postgres', 'sqlite']), ('seeds', [17, 29]),
                             ('binary_sha256', 'changed'), ('rows', 200), ('git_head', 'changed')]:
            parent = {**env, field: value}
            with self.subTest(field=field), self.assertRaises(ValueError):
                child_files(ROOT, source / 'trials', parent)

    @patch('retain_engine_campaign.check_sources')
    @patch('retain_engine_campaign.validate_record', side_effect=RuntimeError('invalid output'))
    def test_invalid_native_record_never_retained(self, validate, sources):
        source, _ = self.fixture()
        with self.assertRaisesRegex(RuntimeError, 'invalid output'):
            retain(ROOT, source, source / 'not-created', True)
        self.assertFalse((source / 'not-created').exists())

    @patch('retain_engine_campaign.check_sources')
    @patch('retain_engine_campaign.validate_record')
    def test_embedded_case_coverage_and_trial_total(self, validate, sources):
        source, env = self.fixture()
        env['cases'] = {'normal': [100, 64, 8388608]}
        (source / 'trials').rename(source / 'normal')
        self.put(source / 'environment.json', env)
        self.put(source / 'command-normal.json', {'exit_code': 0})
        summary = {'cases': {'normal': {}}, 'trials': 1, 'all_full_outputs_match': True}
        self.put(source / 'summary.json', summary)
        self.assertEqual(len(evidence_files(ROOT, source, False)), 7)
        self.put(source / 'summary.json', {**summary, 'trials': 2})
        with self.assertRaisesRegex(ValueError, 'trial total mismatch'):
            evidence_files(ROOT, source, False)
        self.put(source / 'summary.json', {**summary, 'cases': {}})
        with self.assertRaisesRegex(ValueError, 'missing declared workload cases'):
            evidence_files(ROOT, source, False)

    @patch('retain_engine_campaign.check_sources')
    @patch('retain_engine_campaign.validate_record')
    def test_missing_secret_blocks_retention(self, validate, sources):
        source, _ = self.fixture()
        # Preserve the fixture without deleting anything.
        (source / 'private-password.txt').rename(source / 'preserved-password.txt')
        with self.assertRaises(FileNotFoundError): retain(ROOT, source, source / 'not-created', True)
        self.assertFalse((source / 'not-created').exists())


if __name__ == '__main__':
    unittest.main()
