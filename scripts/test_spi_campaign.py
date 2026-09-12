"""Boundary tests for accepting value-cache campaign evidence. No database I/O."""
import copy
import unittest

from run_spi_campaign import validate_record


def result(engine="spi-value-cache", cache_bytes=8192):
    enabled = engine == "spi-value-cache"
    stats = {"cache_bytes": 512, "value_cache_enabled": enabled,
             "retired_cache_value_capacity": 0, "cache_value_entries": 1 if enabled else 0}
    metric = {"samples_ns": [10, 20], "sample_count": 2, "total_ns": 30}
    return {"benchmark_schema": 2, "value_cache_workload_extension": 1,
            "engine": engine, "rows": 2000, "seed": 17, "value_bytes": 64,
            "cache_bytes": cache_bytes, "full_output_validation": "PASS",
            **{name: copy.deepcopy(metric) for name in ["warm_hits", "reused_16_key_hits", "unique_value_reads", "one_off_scan"]},
            **{name: copy.deepcopy(stats) for name in ["cache_after_reused_hits", "cache_after_unique_reads", "before_maintenance"]},
            **{name: {**stats, "cache_value_entries": 0} for name in ["cache_after_one_off_scan", "after_maintenance"]}}


class CampaignResultTests(unittest.TestCase):
    def check(self, record, engine="spi-value-cache", cache=8192):
        validate_record(record, engine, 2000, 17, 64, cache)

    def test_valid_control_and_cache_results(self):
        for engine in ["spi", "spi-value-cache", "sqlite"]:
            self.check(result(engine), engine)

    def test_old_or_mislabeled_contract_rejected(self):
        for field, value in [("benchmark_schema", 1), ("value_cache_workload_extension", None),
                             ("seed", 19), ("full_output_validation", "FAIL")]:
            record = result()
            record[field] = value
            with self.assertRaises(RuntimeError):
                self.check(record)

    def test_budget_admission_and_retirement_errors_rejected(self):
        for field, value in [("cache_bytes", 8193), ("value_cache_enabled", False),
                             ("retired_cache_value_capacity", 16)]:
            record = result()
            record["cache_after_reused_hits"][field] = value
            with self.assertRaises(RuntimeError):
                self.check(record)
        record = result()
        record["cache_after_one_off_scan"]["cache_value_entries"] = 1
        with self.assertRaises(RuntimeError):
            self.check(record)

    def test_disabled_or_tiny_cache_admission_rejected(self):
        record = result("spi")
        record["before_maintenance"]["cache_value_entries"] = 1
        with self.assertRaises(RuntimeError):
            self.check(record, "spi")
        with self.assertRaises(RuntimeError):
            self.check(result(cache_bytes=1024), cache=1024)

    def test_invalid_samples_and_totals_rejected(self):
        for field, value in [("samples_ns", [-1]), ("samples_ns", []),
                             ("samples_ns", [True]), ("sample_count", 50), ("total_ns", 31)]:
            record = result()
            record["warm_hits"][field] = value
            with self.assertRaises(RuntimeError):
                self.check(record)


if __name__ == "__main__":
    unittest.main()
