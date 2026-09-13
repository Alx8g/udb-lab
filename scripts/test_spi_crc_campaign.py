"""Ensure CRC controls differ only in declared implementation and persistent equality is mandatory."""
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
from run_spi_crc_campaign import check_physical_pair, inspect_pair


class CrcCampaignTests(unittest.TestCase):
    @patch('run_spi_crc_campaign.inspect_binary')
    def test_implementation_and_source_identity_required(self, inspect):
        a = {'crc32_implementation': 'ieee-bitwise', 'source_sha256': {'storage': 'same'}, 'packed_scan_implementation': 'direct-base'}
        b = {'crc32_implementation': 'ieee-slicing8', 'source_sha256': {'storage': 'same'}, 'packed_scan_implementation': 'direct-base'}
        inspect.side_effect = [a, b]
        self.assertEqual(len(inspect_pair(Path('a'), Path('b'), Path('.'))), 2)
        inspect.side_effect = [b, a]
        with self.assertRaises(ValueError):
            inspect_pair(Path('a'), Path('b'), Path('.'))
        inspect.side_effect = [a, {**b, 'source_sha256': {'storage': 'other'}}]
        with self.assertRaises(ValueError):
            inspect_pair(Path('a'), Path('b'), Path('.'))
        inspect.side_effect = [a, {**b, 'packed_scan_implementation': 'materialized'}]
        with self.assertRaises(ValueError):
            inspect_pair(Path('a'), Path('b'), Path('.'))

    def test_persistent_equality_includes_all_files(self):
        scratch = Path(__file__).resolve().parents[1] / '.working/tmp/crc-contract-tests'
        scratch.mkdir(parents=True, exist_ok=True)
        root = Path(tempfile.mkdtemp(dir=scratch))
        a, b = root / 'a', root / 'b'
        a.mkdir(); b.mkdir()
        with self.assertRaises(ValueError):
            check_physical_pair(a, b)
        (a / 'manifest.spi').write_bytes(b'exact')
        (b / 'manifest.spi').write_bytes(b'exact')
        self.assertEqual(len(check_physical_pair(a, b)), 1)
        (b / 'extra.spi').write_bytes(b'orphan')
        with self.assertRaises(ValueError):
            check_physical_pair(a, b)


if __name__ == '__main__':
    unittest.main()
