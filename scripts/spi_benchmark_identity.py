"""Reject stale benchmark executables before a campaign creates any output.

The native adapter's --build-info response contains compile-time embedded
source text. Compare bytes, not modification times or the working Git HEAD.
A source match establishes source provenance, not trustworthy timing or a
complete compiler/OS/hardware attestation.
"""
from __future__ import annotations

import hashlib
import json
import subprocess
from pathlib import Path
from typing import Any

IDENTITY_SCHEMA = 1
BENCHMARK_SCHEMA = 2
REQUIRED_SOURCES = frozenset({
    "Cargo.toml",
    "Cargo.lock",
    "src/lib.rs",
    "src/spi/mod.rs",
    "src/spi/profile.rs",
    "src/spi/append_buffer.rs",
    "src/spi/storage.rs",
    "src/spi/transaction.rs",
    "src/bin/spi-compare.rs",
})


class IdentityError(ValueError):
    """The executable cannot be tied to the source under comparison."""


def validate_build_info(
    info: Any,
    root: Path,
    engines: list[str],
    *,
    allow_profile: bool = False,
) -> dict[str, Any]:
    if not isinstance(info, dict):
        raise IdentityError("benchmark build information must be an object")
    if info.get("identity_schema") != IDENTITY_SCHEMA:
        raise IdentityError("binary lacks the supported identity protocol; rebuild it")
    if info.get("benchmark_schema") != BENCHMARK_SCHEMA:
        raise IdentityError("binary uses an obsolete benchmark contract; rebuild it")
    if info.get("debug_assertions") is not False:
        raise IdentityError("debug/instrumented binary rejected; use a release build")
    profile_enabled = info.get("profile_enabled")
    if type(profile_enabled) is not bool:
        raise IdentityError("missing diagnostic-profile identity; rebuild it")
    if profile_enabled and not allow_profile:
        raise IdentityError("diagnostic-profile binary rejected for performance campaign")
    compiled_engines = info.get("engines")
    if not isinstance(compiled_engines, list) or any(
        not isinstance(engine, str) for engine in compiled_engines
    ):
        raise IdentityError("binary has no valid compiled-engine list")
    if not set(engines).issubset(compiled_engines):
        raise IdentityError("binary does not contain every requested engine adapter")
    sources = info.get("sources")
    if not isinstance(sources, dict) or set(sources) != REQUIRED_SOURCES:
        raise IdentityError("binary source inventory is incomplete or unexpected")
    discovered = {p.relative_to(root).as_posix() for p in (root / "src/spi").rglob("*.rs")}
    expected_modules = {p for p in REQUIRED_SOURCES if p.startswith("src/spi/")}
    if discovered != expected_modules:
        raise IdentityError("storage module inventory changed; update and rebuild binary identity")
    hashes = {}
    mismatches = []
    for relative in sorted(REQUIRED_SOURCES):
        source = sources[relative]
        if not isinstance(source, str):
            raise IdentityError(f"invalid embedded source text: {relative}")
        embedded = source.encode("utf-8")
        try:
            current = (root / relative).read_bytes()
        except OSError as exc:
            raise IdentityError(f"cannot read benchmark source: {relative}") from exc
        if embedded != current:
            mismatches.append(relative)
        hashes[relative] = hashlib.sha256(embedded).hexdigest()
    if mismatches:
        raise IdentityError("stale benchmark binary; changed files: " + ", ".join(mismatches))
    return {
        "identity_schema": IDENTITY_SCHEMA,
        "benchmark_schema": BENCHMARK_SCHEMA,
        "debug_assertions": False,
        "profile_enabled": profile_enabled,
        "compiled_engines": compiled_engines,
        "source_sha256": hashes,
        "limitations": "Source-byte identity is checked. Build flags beyond debug assertions and external dependencies are recorded separately, not attested by this protocol.",
    }


def inspect_binary(binary: Path, root: Path, engines: list[str], *, allow_profile: bool = False) -> dict[str, Any]:
    try:
        result = subprocess.run(
            [str(binary), "--build-info"],
            cwd=root,
            capture_output=True,
            text=True,
            encoding="utf-8",
            timeout=15,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired, UnicodeError) as exc:
        raise IdentityError("cannot obtain benchmark binary identity") from exc
    if result.returncode:
        raise IdentityError("benchmark binary rejected --build-info; rebuild before running")
    try:
        info = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise IdentityError("benchmark binary returned invalid build information") from exc
    return validate_build_info(info, root, engines, allow_profile=allow_profile)
