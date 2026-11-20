#!/usr/bin/env bash
# Exercise the real smoke emitter with fixture SQL, retaining no auth values.
set -eEuo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
source "$HERE/lib.sh"
source "$HERE/scenarios/strict-smoke.sh"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
OWNER=a
ROW=credential-regression
fixture_password="fixture-secret-$RANDOM-$RANDOM"
fixture_verifier="*$(printf '%040d' 7)"
strict_record_start() { :; }
wait_for() { :; }
new_row() { :; }
report_field() { :; }
sql() {
    case "$2" in
        *"SHOW GRANTS"*)
            printf "GRANT SLAVE MONITOR ON *.* TO monitor IDENTIFIED BY PASSWORD '%s'\n" "$fixture_verifier"
            printf 'GRANT SELECT ON gruezi_ha.handoff_release TO monitor\n' ;;
        *"information_schema.USER_PRIVILEGES"*)
            printf 'global\t*.*\tSLAVE MONITOR\tNO\ntable\tgruezi_ha.handoff_release\tSELECT\tNO\n' ;;
        *"COUNT("* ) printf '1\n' ;;
    esac
}
db_with() {
    case "$*" in
        *INSERT*) printf 'ERROR 1290\n' >&2; return 1 ;;
        *DELETE*) printf 'ERROR 1142\n' >&2; return 1 ;;
    esac
}
b4_eaccensevidence() { :; }
adapter_portability_check() { :; }
pass() { printf 'PASS fixture smoke\n'; }
strict_smoke >"$scratch/stdout" 2>"$scratch/stderr"
# Include a report artifact: all retained surfaces must exclude fixture secrets.
printf 'scenario\tresult\nfixture\tPASS\n' >"$scratch/report.tsv"
matches=0
for path in "$scratch/stdout" "$scratch/stderr" "$scratch/report.tsv"; do
    if grep -Fq -e "$fixture_password" -e "$fixture_verifier" "$path"; then
        matches=$((matches + 1))
    fi
done
if ((matches)); then
    printf 'FAIL fixture_authentication_material_reaches_output affected_surfaces=%s\n' "$matches"
    exit 1
fi
printf 'PASS fixture_authentication_material_absent_from_stdout_stderr_reports\n'
printf '%s\n%s\n' "$fixture_password" "$fixture_verifier" \
    | bash "$HERE/audit-output.sh" "$scratch/stdout" "$scratch/stderr" "$scratch/report.tsv"
printf '%s\n%s\n' "$fixture_password" "$fixture_verifier" >"$scratch/negative"
if printf '%s\n%s\n' "$fixture_password" "$fixture_verifier" \
    | bash "$HERE/audit-output.sh" "$scratch/negative"; then
    printf 'FAIL authentication_auditor_missed_negative_control\n'
    exit 1
fi
printf 'PASS authentication_auditor_detects_plaintext_and_verifier_negative_control\n'
