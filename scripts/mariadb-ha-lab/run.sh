#!/usr/bin/env bash
set -eEuo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
source "$HERE/lib.sh"
usage() {
    printf '%s\n' "Usage: $0 [--force] [--with-hook] [--profile available|strict] [--scenario <id>] [--check-prerequisites]"
}
CHECK_ONLY=0
for scenario_file in "$HERE"/scenarios/*.sh; do source "$scenario_file"; done
while (($#)); do
    case "$1" in
        --force) FORCE=1 ;;
        --with-hook) WITH_HOOK=1 ;;
        --profile|--scenario)
            (($# >= 2)) || { usage >&2; exit 2; }
            case "$1" in --profile) PROFILE="$2" ;; --scenario) SCENARIO_FILTER="$2" ;; esac
            shift ;;
        --check-prerequisites) CHECK_ONLY=1 ;;
        --test-report-emitter) report_emitter_self_test; exit ;;
        --help|-h) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
    shift
done
case "$PROFILE" in available|strict) ;; *) usage >&2; exit 2 ;; esac
validate_scenario
prerequisites
[[ "$CHECK_ONLY" == 0 ]] || exit 0
setup_cluster
if [[ "$PROFILE" == available ]]; then
    run_available
else
    run_strict
fi
