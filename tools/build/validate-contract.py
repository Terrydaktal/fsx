#!/usr/bin/env python3
"""Repository-side strict smoke validation for debuggability.toml.

The full skill validator remains the authoritative local check. This compact
copy keeps CI independent of Codex's skill installation while catching the
contract properties that would make an artifact unverifiable.
"""

from __future__ import annotations

import sys
import tomllib
from pathlib import Path


REQUIRED_CONTROLS = {
    "architecture.state_owners",
    "architecture.mutation_points",
    "architecture.side_effect_boundaries",
    "architecture.fault_containment",
    "errors.causal_ids",
    "build.identity",
    "live.capabilities_command",
    "live.snapshot_command",
    "history.schema",
    "resources.declared_budgets",
    "domain.state_dump",
    "assurance.contract_test",
    "assurance.overhead_test",
    "assurance.snapshot_test",
    "deployment_modes.minimal_release",
    "deployment_modes.observable_release",
    "deployment_modes.global_runtime_switch",
    "deployment_modes.per_category_switches",
    "deployment_modes.diagnostic_builds",
    "performance_isolation.critical_hot_paths",
    "performance_isolation.minimal_mode_test",
    "performance_isolation.runtime_disabled_test",
    "performance_isolation.always_on_test",
    "performance_isolation.activated_mode_tests",
}

STATUS_FIELDS = {
    "errors.typed_codes",
    "errors.preserves_source_chain",
    "errors.preserves_first_failure",
    "configuration.effective_config",
    "configuration.decision_provenance",
    "configuration.dynamic_change_history",
    "build.exact_artifact_retention",
    "build.symbols_and_unwind",
    "build.source_retrieval",
    "artifacts.cache_identity",
    "artifacts.atomic_publication",
    "artifacts.generation_manifest",
    "live.independent_of_failure_loop",
    "history.flight_recorder",
    "history.drop_and_overwrite_reporting",
    "audit_trail.mutation_audit",
    "audit_trail.direct_write_coverage",
    "concurrency.task_registry",
    "concurrency.lock_wait_introspection",
    "concurrency.cross_process_correlation",
    "resources.ownership_reporting",
    "resources.high_water_reporting",
    "postmortem.crash_capture",
    "postmortem.includes_history",
    "postmortem.oom_or_external_kill_detection",
    "recovery.pre_repair_evidence",
    "recovery.restart_or_fallback_history",
    "recovery.last_failure_retention",
    "budgets.measurement_status",
    "deployment_modes.diagnostic_builds",
    "performance_isolation.baseline_comparison",
}


def has_placeholder(value: object) -> bool:
    if isinstance(value, str):
        return "REPLACE_ME" in value or "TODO" in value.upper()
    if isinstance(value, dict):
        return any(has_placeholder(item) for item in value.values())
    if isinstance(value, list):
        return any(has_placeholder(item) for item in value)
    return False


def main() -> int:
    path = Path(sys.argv[1] if len(sys.argv) > 1 else "debuggability.toml")
    with path.open("rb") as stream:
        data = tomllib.load(stream)
    errors = []
    if data.get("schema_version") != 5:
        errors.append("schema_version must be 5")
    if data.get("template") is not False:
        errors.append("template must be false")
    if data.get("profile") not in {"micro", "standard", "stateful", "resilient"}:
        errors.append("profile is invalid")
    if has_placeholder(data):
        errors.append("contract contains a placeholder or empty string")
    controls = data.get("controls", {})
    missing = REQUIRED_CONTROLS - set(controls)
    if missing:
        errors.append(f"missing controls: {', '.join(sorted(missing))}")
    for name, record in controls.items():
        if not isinstance(record, dict) or set(record) != {"status", "reason", "implementation", "test"}:
            errors.append(f"control {name} must contain status/reason/implementation/test")
    for status_path in sorted(STATUS_FIELDS):
        table, field = status_path.split(".", 1)
        value = data.get(table, {}).get(field)
        if isinstance(value, str) and value in {"planned", "implemented_untested", "absent"}:
            errors.append(f"{status_path} is not fully realized: {value}")
    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"valid repository debuggability contract: {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
