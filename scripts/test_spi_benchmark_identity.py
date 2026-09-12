"""Deterministic provenance-guard tests. No database or benchmark is executed."""
from __future__ import annotations

import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from spi_benchmark_identity import (
    BENCHMARK_SCHEMA,
    IDENTITY_SCHEMA,
    REQUIRED_SOURCES,
    IdentityError,
    inspect_binary,
    validate_build_info,
)


class BenchmarkIdentityTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        parent = Path(__file__).resolve().parents[1] / ".working/tmp/identity-tests"
        parent.mkdir(parents=True, exist_ok=True)
        # Preserve disposable fixtures beside the project. Never remove or
        # overwrite a caller-supplied directory.
        cls.root = Path(tempfile.mkdtemp(prefix="sources-", dir=parent))
        cls.sources = {}
        for name in sorted(REQUIRED_SOURCES):
            target = cls.root / name
            target.parent.mkdir(parents=True, exist_ok=True)
            contents = f"// fixture for {name}\r\n// exact UTF-8: \u03bb\n"
            with target.open("xb") as stream:
                stream.write(contents.encode("utf-8"))
            cls.sources[name] = contents

    def info(self) -> dict:
        return {
            "identity_schema": IDENTITY_SCHEMA,
            "benchmark_schema": BENCHMARK_SCHEMA,
            "debug_assertions": False,
            "profile_enabled": False,
            "crc32_implementation": "ieee-slicing8",
            "packed_scan_implementation": "direct-base",
            "engines": ["spi", "sqlite"],
            "sources": dict(self.sources),
        }

    def test_matching_source_bytes_are_accepted(self) -> None:
        evidence = validate_build_info(self.info(), self.root, ["spi", "sqlite"])
        self.assertEqual(set(evidence["source_sha256"]), REQUIRED_SOURCES)
        self.assertTrue(all(len(h) == 64 for h in evidence["source_sha256"].values()))
        self.assertNotIn("sources", evidence)

    def test_each_changed_source_is_rejected(self) -> None:
        for name in REQUIRED_SOURCES:
            with self.subTest(source=name):
                info = self.info()
                info["sources"][name] += "// compiled old content\n"
                with self.assertRaisesRegex(IdentityError, "stale benchmark binary"):
                    validate_build_info(info, self.root, ["spi"])

    def test_line_endings_are_not_silently_normalized(self) -> None:
        info = self.info()
        info["sources"]["src/lib.rs"] = info["sources"]["src/lib.rs"].replace("\r\n", "\n")
        with self.assertRaisesRegex(IdentityError, "stale benchmark binary"):
            validate_build_info(info, self.root, ["spi"])

    def test_old_contract_and_missing_protocol_are_rejected(self) -> None:
        for field, value in [("benchmark_schema", 1), ("identity_schema", None)]:
            with self.subTest(field=field):
                info = self.info()
                info[field] = value
                with self.assertRaises(IdentityError):
                    validate_build_info(info, self.root, ["spi"])

    def test_debug_and_unknown_profiles_are_rejected(self) -> None:
        for value in (True, None, "false", 0):
            with self.subTest(value=value):
                info = self.info()
                info["debug_assertions"] = value
                with self.assertRaisesRegex(IdentityError, "debug/instrumented"):
                    validate_build_info(info, self.root, ["spi"])

    def test_diagnostic_profile_requires_explicit_permission(self) -> None:
        info = self.info()
        info["profile_enabled"] = True
        with self.assertRaisesRegex(IdentityError, "diagnostic-profile"):
            validate_build_info(info, self.root, ["spi"])
        evidence = validate_build_info(info, self.root, ["spi"], allow_profile=True)
        self.assertTrue(evidence["profile_enabled"])
        for value in (None, 0, "false"):
            info["profile_enabled"] = value
            with self.assertRaisesRegex(IdentityError, "profile identity"):
                validate_build_info(info, self.root, ["spi"], allow_profile=True)

    def test_crc_implementation_identity_is_explicit(self):
        for name in ("ieee-slicing8", "ieee-bitwise"):
            info = self.info()
            info["crc32_implementation"] = name
            self.assertEqual(validate_build_info(info, self.root, ["spi"])["crc32_implementation"], name)
        for name in (None, "crc32c", False):
            info = self.info()
            info["crc32_implementation"] = name
            with self.assertRaisesRegex(IdentityError, "CRC implementation"):
                validate_build_info(info, self.root, ["spi"])

    def test_scan_identity_is_explicit(self):
        for name in ("direct-base", "direct-delta", "materialized"):
            info = self.info()
            info["packed_scan_implementation"] = name
            self.assertEqual(validate_build_info(info, self.root, ["spi"])["packed_scan_implementation"], name)
        for name in (None, False, "unknown"):
            info = self.info()
            info["packed_scan_implementation"] = name
            with self.assertRaisesRegex(IdentityError, "packed scan implementation"):
                validate_build_info(info, self.root, ["spi"])

    def test_missing_engine_is_rejected(self) -> None:
        info = self.info()
        info["engines"] = ["spi"]
        with self.assertRaisesRegex(IdentityError, "requested engine"):
            validate_build_info(info, self.root, ["sqlite"])
        validate_build_info(info, self.root, ["spi"])

    def test_unexpected_or_missing_inventory_is_rejected(self) -> None:
        info = self.info()
        del info["sources"]["Cargo.lock"]
        with self.assertRaisesRegex(IdentityError, "inventory"):
            validate_build_info(info, self.root, ["spi"])
        info = self.info()
        info["sources"]["../../outside"] = "not read"
        with self.assertRaisesRegex(IdentityError, "inventory"):
            validate_build_info(info, self.root, ["spi"])

    def test_malformed_response_is_rejected(self) -> None:
        for value in (None, [], "text"):
            with self.assertRaises(IdentityError):
                validate_build_info(value, self.root, ["spi"])
        info = self.info()
        info["engines"] = [None]
        with self.assertRaises(IdentityError):
            validate_build_info(info, self.root, ["spi"])
        info = self.info()
        info["sources"]["Cargo.toml"] = None
        with self.assertRaises(IdentityError):
            validate_build_info(info, self.root, ["spi"])

    @patch("spi_benchmark_identity.subprocess.run")
    def test_probe_only_requests_build_info(self, run) -> None:
        binary = self.root / "comparison.exe"
        run.return_value = subprocess.CompletedProcess([], 0, json.dumps(self.info()), "")
        inspect_binary(binary, self.root, ["spi"])
        self.assertEqual(run.call_args.args[0], [str(binary), "--build-info"])
        self.assertEqual(run.call_args.kwargs["timeout"], 15)
        self.assertEqual(run.call_count, 1)

    @patch("spi_benchmark_identity.subprocess.run")
    def test_legacy_binary_is_rejected_before_workload(self, run) -> None:
        run.return_value = subprocess.CompletedProcess([], 1, "", "usage")
        with self.assertRaisesRegex(IdentityError, "rebuild"):
            inspect_binary(self.root / "old.exe", self.root, ["spi"])
        self.assertEqual(run.call_count, 1)

    @patch("spi_benchmark_identity.subprocess.run")
    def test_timeout_or_non_json_probe_is_rejected(self, run) -> None:
        run.side_effect = subprocess.TimeoutExpired("probe", 15)
        with self.assertRaises(IdentityError):
            inspect_binary(self.root / "slow.exe", self.root, ["spi"])
        run.side_effect = None
        run.return_value = subprocess.CompletedProcess([], 0, "not JSON", "")
        with self.assertRaises(IdentityError):
            inspect_binary(self.root / "bad.exe", self.root, ["spi"])


if __name__ == "__main__":
    unittest.main()
