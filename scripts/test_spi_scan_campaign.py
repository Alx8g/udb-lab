"""Reject mismatched same-source scan controls without running benchmark I/O."""
import unittest
from pathlib import Path
from unittest.mock import patch
from run_spi_scan_campaign import inspect_pair, inspect_extra, mode_order, physical_checks


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

    @patch('run_spi_scan_campaign.inspect_binary')
    def test_delta_candidate_requires_matching_base_control(self, inspect):
        a = {'packed_scan_implementation': 'direct-base', 'crc32_implementation': 'ieee-slicing8',
             'source_sha256': {'storage': 'same'}}
        b = {**a, 'packed_scan_implementation': 'direct-delta'}
        inspect.side_effect = [a, b]
        self.assertEqual(set(inspect_pair(Path('a'), Path('b'), Path('.'),
                         control_mode='direct-base', candidate_mode='direct-delta')), {'direct-base', 'direct-delta'})
        inspect.side_effect = [a, b]
        with self.assertRaises(ValueError):
            inspect_pair(Path('a'), Path('b'), Path('.'))
        with self.assertRaises(ValueError):
            inspect_pair(Path('a'), Path('b'), Path('.'), control_mode='direct-base', candidate_mode='direct-base')

    @patch('run_spi_scan_campaign.inspect_binary')
    def test_third_mode_keeps_source_and_crc_controls(self, inspect):
        reference = {'packed_scan_implementation': 'direct-base', 'crc32_implementation': 'ieee-slicing8',
                     'source_sha256': {'storage': 'same'}}
        pair = {'materialized': {**reference, 'packed_scan_implementation': 'materialized'},
                'direct-base': reference}
        good = {**reference, 'packed_scan_implementation': 'direct-delta'}
        inspect.return_value = good
        self.assertEqual(inspect_extra(Path('delta'), Path('.'), pair), good)
        for bad in [reference, {**good, 'source_sha256': {'storage': 'wrong'}},
                    {**good, 'crc32_implementation': 'ieee-bitwise'}]:
            inspect.return_value = bad
            with self.assertRaises(ValueError):
                inspect_extra(Path('delta'), Path('.'), pair)
        with self.assertRaises(ValueError):
            inspect_extra(Path('delta'), Path('.'), {'direct-base': reference})

    def test_three_mode_order_covers_each_position(self):
        modes = ['materialized', 'direct-base', 'direct-delta']
        for case in range(3):
            orders = [mode_order(modes, case, seed) for seed in range(3)]
            for position in range(3):
                self.assertEqual({order[position] for order in orders}, set(modes))
        self.assertEqual(mode_order(modes[:2], 0, 0), modes[:2])
        self.assertEqual(mode_order(modes[:2], 0, 1), modes[1::-1])

    @patch('run_spi_scan_campaign.check_physical_pair')
    def test_all_third_mode_persistent_comparisons_are_required(self, compare):
        compare.return_value = {'manifest.spi': 'sha'}
        checks = physical_checks(Path('out'), 'normal', 17,
                                 ['materialized', 'direct-base', 'direct-delta'])
        self.assertEqual(compare.call_count, 4)
        self.assertEqual({(c['engine'], c['candidate_mode']) for c in checks},
                         {(e, m) for e in ['spi', 'spi-packed'] for m in ['direct-base', 'direct-delta']})
        self.assertTrue(all(c['control_mode'] == 'materialized' for c in checks))
        compare.side_effect = ValueError('different persistent bytes')
        with self.assertRaises(ValueError):
            physical_checks(Path('out'), 'normal', 17, ['direct-base', 'direct-delta'])


if __name__ == '__main__':
    unittest.main()
