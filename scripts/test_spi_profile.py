"""Profile evidence acceptance tests. No native process or benchmark execution."""
import copy
import unittest
from run_spi_profile import PHASES, validate_profile
from test_spi_campaign import result


def profile_result(engine='spi'):
    record = result(engine)
    record['diagnostic_only'] = True
    record['packed_scan_implementation'] = 'direct-base'
    record['crc32_implementation'] = 'ieee-slicing8'
    counters = dict.fromkeys(('read_calls', 'read_bytes', 'read_ns', 'write_calls', 'write_bytes',
        'write_ns', 'checksum_calls', 'checksum_bytes', 'checksum_ns', 'node_cache_hits',
        'node_cache_misses', 'value_cache_hits', 'value_cache_misses', 'node_saves', 'value_records',
        'build_ns', 'data_sync_ns', 'manifest_write_ns', 'manifest_sync_ns', 'manifest_replace_ns',
        'directory_sync_ns', 'fault_checks', 'fault_check_ns'), 0)
    metric = {'phase_wall_ns_including_harness': 100, 'process_cpu_ns_including_harness': 0,
        'rust_allocation_calls': 2, 'rust_requested_allocation_bytes': 100,
        'rust_live_requested_bytes': 40, 'process_peak_rust_requested_bytes': 100,
        'os_memory_after_phase': None, 'storage': counters}
    record['diagnostic_phases'] = {name: copy.deepcopy(metric) for name in PHASES}
    return record


class ProfileTests(unittest.TestCase):
    def check(self, r, engine='spi'):
        validate_profile(r, engine, 2000, 17, 64, 8192)

    def test_zero_and_unavailable_cpu_are_not_fabricated(self):
        for engine in ('spi', 'spi-grouped', 'sqlite'):
            r = profile_result(engine)
            self.check(r, engine)
            r['diagnostic_phases']['scan']['process_cpu_ns_including_harness'] = None
            self.check(r, engine)

    def test_profile_marker_and_complete_phases_required(self):
        for marker in (False, None, 1):
            r = profile_result()
            r['diagnostic_only'] = marker
            with self.assertRaises(RuntimeError): self.check(r)
        r = profile_result()
        del r['diagnostic_phases']['scan']
        with self.assertRaises(RuntimeError): self.check(r)

    def test_invalid_counters_and_peaks_rejected(self):
        for field, value in [('rust_allocation_calls', True), ('rust_requested_allocation_bytes', -1),
                ('process_cpu_ns_including_harness', -1), ('rust_live_requested_bytes', 101)]:
            r = profile_result()
            r['diagnostic_phases']['scan'][field] = value
            with self.assertRaises(RuntimeError): self.check(r)
        r = profile_result()
        r['diagnostic_phases']['scan']['storage']['read_calls'] = -1
        with self.assertRaises(RuntimeError): self.check(r)

    def test_sqlite_cannot_claim_spi_instrumentation(self):
        r = profile_result('sqlite')
        r['diagnostic_phases']['load']['storage']['node_saves'] = 1
        with self.assertRaises(RuntimeError): self.check(r, 'sqlite')


if __name__ == '__main__':
    unittest.main()
