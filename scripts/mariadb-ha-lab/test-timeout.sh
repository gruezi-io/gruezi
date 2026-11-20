#!/usr/bin/env bash
# Genuine bounded wait failure on the same disposable strict fixture.
# The required outcome is exit 1, a FAIL record, diagnostics and cleanup.
set -eEuo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
source "$HERE/lib.sh"
for scenario_file in "$HERE"/scenarios/*.sh; do source "$scenario_file"; done
PROFILE=strict
SCENARIO_FILTER=strict-smoke
validate_scenario
prerequisites
setup_cluster
strict_smoke
strict_record_start strict-timeout-probe timeout
never_satisfied() { return 1; }
printf 'TIMEOUT_BEGIN monotonic_ms=%s expected_wait_seconds=60\n' "$(handoff_monotonic_ms)"
wait_for "deliberate never-satisfied predicate" never_satisfied
