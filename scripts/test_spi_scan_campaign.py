"""Reject mismatched same-source scan controls without running benchmark I/O."""
import unittest
from pathlib import Path
from unittest.mock import patch
from run_spi_scan_campaign import inspect_pair


class ScanCampaignTests(unittest.TestCase):
    @patch('run_spi_scan_campaign.inspect_binary')
    def test_controls_require_matching_source_and_crc(self, inspect):
        a = {'packed_scan_implementation': 'materialized', 'crc32_implementation': 'ieee-slicing8',
             'source_sha256': {'storage': 'same'}}
        b = {**a, 'packed_scan_implementation': 'direct-base'}
        inspect.side_effect = [a, b]
        self.assertEqual(len(inspect_pair(Path('a'), Path('b'), Path('.'))), 2)
        for left, right in [(b, a), (a, {**b, 'source_sha256': {'storage': 'different'}}),
                            (a, {**b, 'crc32_implementation': 'ieee-bitwise'})]:
            inspect.side_effect = [left, right]
            with self.assertRaises(ValueError):
                inspect_pair(Path('a'), Path('b'), Path('.'))


if __name__ == '__main__':
    unittest.main()
