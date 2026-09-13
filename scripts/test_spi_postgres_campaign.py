"""Pure boundary tests for isolated PostgreSQL setup. Never starts a server."""
import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from run_spi_postgres_campaign import config_string, free_loopback_port, validate_server_version
from validate_engine_result import external_inputs


class PostgresBoundaryTests(unittest.TestCase):
    def test_only_pinned_server_version_is_accepted(self):
        validate_server_version('postgres (PostgreSQL) 18.4')
        for version in ('postgres (PostgreSQL) 18.3', 'postgres (PostgreSQL) 19.0', '', '18.4'):
            with self.subTest(version=version), self.assertRaises(ValueError):
                validate_server_version(version)

    def test_config_string_quotes_literal_paths(self):
        self.assertEqual(config_string("C:/one's folder/cluster"), "'C:/one''s folder/cluster'")
        self.assertEqual(config_string('/tmp/isolated'), "'/tmp/isolated'")

    def test_available_port_is_loopback_and_socket_is_released(self):
        import socket
        port = free_loopback_port()
        self.assertTrue(0 < port < 65536)
        # The port may be claimed by another process; only type/range is promised.
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            sock.bind(('127.0.0.1', 0))
            self.assertEqual(sock.getsockname()[0], '127.0.0.1')

    def fixture(self):
        parent = Path(__file__).resolve().parents[1] / '.working/tmp/postgres-boundary-tests'
        parent.mkdir(parents=True, exist_ok=True)
        root = Path(tempfile.mkdtemp(dir=parent))
        (root / 'cluster').mkdir()
        config = root / 'benchmark-postgresql.conf'
        config.write_bytes(b"listen_addresses='127.0.0.1'\nfsync=on\n")
        marker = {'schema': 1, 'host': '127.0.0.1', 'port': 55439,
                  'cluster_name': 'spi_benchmark_' + 'a'*32,
                  'data_directory': str((root / 'cluster').resolve()),
                  'postgres_version': 'postgres (PostgreSQL) 18.4',
                  'config_sha256': hashlib.sha256(config.read_bytes()).hexdigest()}
        path = root / 'cluster-identity.json'
        path.write_bytes(json.dumps(marker).encode())
        env = {'SPI_PG_CLUSTER_FILE': str(path), 'SPI_PG_PORT': '55439',
               'SPI_PG_PASSWORD': 'synthetic-test-secret-not-a-credential'}
        return root, path, marker, env

    def test_preflight_omits_credentials_and_checks_config_integrity(self):
        root, path, marker, env = self.fixture()
        with patch.dict(os.environ, env, clear=True):
            result = external_inputs(['postgres'])
            self.assertNotIn(env['SPI_PG_PASSWORD'], json.dumps(result))
            self.assertEqual(result['postgres']['port'], marker['port'])
            (root / 'benchmark-postgresql.conf').write_bytes(b'changed')
            with self.assertRaisesRegex(ValueError, 'configuration changed'):
                external_inputs(['postgres'])

    def test_wrong_port_host_and_data_directory_rejected(self):
        root, path, marker, env = self.fixture()
        for field, value in [('port', 55440), ('host', '0.0.0.0'), ('data_directory', str(root))]:
            path.write_bytes(json.dumps({**marker, field: value}).encode())
            with patch.dict(os.environ, env, clear=True), self.assertRaises(ValueError):
                external_inputs(['postgres'])


if __name__ == '__main__':
    unittest.main()
