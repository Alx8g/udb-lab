"""Boundary tests for accepting value-cache campaign evidence. No database I/O."""
import copy
import unittest

from run_spi_campaign import validate_record


def result(engine="spi-value-cache", cache_bytes=8192):
    enabled = engine == "spi-value-cache"
    stats = {"cache_bytes": 512, "value_cache_enabled": enabled,
             "retired_cache_value_capacity": 0, "cache_value_entries": 1 if enabled else 0,
             "grouped_updates_enabled": engine == "spi-grouped",
             "append_buffer_enabled": engine != "spi-unbuffered",
             "bytes_written": 1000, "arena_write_calls": 10,
             "packed_format": engine == "spi-packed", "retired_cache_page_capacity": 0,
             "cache_page_entries": 0}
    if engine == "sqlite":
        stats.update({"physical_bytes": 1000, "journal_mode": "WAL", "synchronous": "FULL", "sqlite_version": "fixture"})
    metric = {"samples_ns": [10, 20], "sample_count": 2, "total_ns": 30}
    return {"benchmark_schema": 2, "value_cache_workload_extension": 1, "diagnostic_only": False,
            "grouped_write_workload_extension": 1, "scan_workload_extension": 1,
            "engine": engine, "rows": 2000, "seed": 17, "value_bytes": 64,
            "cache_bytes": cache_bytes, "full_output_validation": "PASS",
            **{name: copy.deepcopy(metric) for name in ["load_batches_256", "updates_batches_64", "single_row_commits", "warm_hits", "reused_16_key_hits", "unique_value_reads", "one_off_scan", "post_mutation_scan", "post_mutation_ranges", "post_compaction_scan"]},
            **{name: copy.deepcopy(stats) for name in ["after_load", "after_updates", "cache_after_reused_hits", "cache_after_unique_reads", "before_maintenance"]},
            **{name: {**stats, "cache_value_entries": 0} for name in ["cache_after_one_off_scan", "after_maintenance"]}}


class CampaignResultTests(unittest.TestCase):
    def check(self, record, engine="spi-value-cache", cache=8192):
        validate_record(record, engine, 2000, 17, 64, cache)

    def test_valid_control_and_cache_results(self):
        for engine in ["spi", "spi-value-cache", "spi-unbuffered", "spi-grouped", "spi-packed", "sqlite"]:
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

    def test_diagnostic_output_is_not_performance_evidence(self):
        for flag in (True, None, 0, "false"):
            record = result()
            record["diagnostic_only"] = flag
            with self.assertRaisesRegex(RuntimeError, "diagnostic/performance"):
                self.check(record)

    def test_grouped_control_and_counter_regressions_rejected(self):
        for field, value in [("grouped_updates_enabled", False), ("append_buffer_enabled", False),
                             ("bytes_written", -1), ("arena_write_calls", True)]:
            record = result("spi-grouped")
            record["after_updates"][field] = value
            with self.assertRaises(RuntimeError):
                self.check(record, "spi-grouped")
        record = result("spi-grouped")
        record.pop("grouped_write_workload_extension")
        with self.assertRaises(RuntimeError):
            self.check(record, "spi-grouped")

    def test_packed_format_and_retired_capacity_are_checked(self):
        for field, value in [("packed_format", False), ("retired_cache_page_capacity", 16)]:
            record = result("spi-packed")
            record["after_load"][field] = value
            with self.assertRaises(RuntimeError):
                self.check(record, "spi-packed")
        record = result("spi")
        record["after_load"]["cache_page_entries"] = 1
        with self.assertRaises(RuntimeError):
            self.check(record, "spi")

    def test_post_mutation_and_compaction_scans_are_required(self):
        record = result()
        record.pop("scan_workload_extension")
        with self.assertRaises(RuntimeError):
            self.check(record)
        for field in ("post_mutation_scan", "post_mutation_ranges", "post_compaction_scan"):
            record = result()
            record.pop(field)
            with self.assertRaises(RuntimeError):
                self.check(record)
            record = result()
            record[field]["total_ns"] += 1
            with self.assertRaises(RuntimeError):
                self.check(record)

    def test_invalid_samples_and_totals_rejected(self):
        for field, value in [("samples_ns", [-1]), ("samples_ns", []),
                             ("samples_ns", [True]), ("sample_count", 50), ("total_ns", 31)]:
            record = result()
            record["warm_hits"][field] = value
            with self.assertRaises(RuntimeError):
                self.check(record)


if __name__ == "__main__":
    unittest.main()
