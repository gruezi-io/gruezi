#!/usr/bin/env bash
# Synthetic observers exercise the real predicates and v1 report boundary.
set -euo pipefail
TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
source "${LAB_TEST_LIB:-$TEST_DIR/lib.sh}"
source "$TEST_DIR/scenarios/strict-network.sh"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
REPORT_DIR="$scratch"
failed=0
exists_a=0 exists_b=0 address_a=0 address_b=0 held=none exists_output=""

check() {
    if "$@"; then return 0; fi
    printf 'FAIL %s\n' "$*"
    failed=1
}
p() {
    local node code
    case "$1 $2" in
        "pod exists")
            node="${3##*-}"
            if [[ "$node" == a ]]; then code="$exists_a"; else code="$exists_b"; fi
            if [[ "$code" != 0 && "$code" != 1 ]]; then printf '%s' "$exists_output"; fi
            return "$code" ;;
        "exec "*)
            node="${2##*-}"
            if [[ "$node" == a ]]; then code="$address_a"; else code="$address_b"; fi
            # A failed observer may have written partial output.
            printf '2: eth0 inet 10.249.15.1/24 scope global eth0\n'
            if [[ "$code" != 0 ]]; then return "$code"; fi
            case "$held:$node" in
                a:a|b:b|ab:*) printf '2: eth0 inet %s/24 scope global eth0\n' "$VIP" ;;
            esac ;;
        *) printf 'Unexpected synthetic observer call\n' >&2; return 99 ;;
    esac
}
sql() { printf '1\n'; }
read_only_of() {
    case "$held:$1" in a:a|b:b) printf 0 ;; *) printf 1 ;; esac
}
replica_field() { printf 'Yes\n'; }
status_json() {
    printf '%s\n' '{"ha":{"owned":false,"authority":{"state":"none","term":0,"source":null},"lifecycle":{"activation_confirmed":false}}}'
}

report_matches() {
    awk -F'\t' -v owners="$1" -v a="$2" -v b="$3" '
        /^#/ {next}
        !header {for(i=1;i<=NF;i++) h[$i]=i; header=1; next}
        {ok=($h["kernel_vip_owners"]==owners && $h["owner_a"]==a && $h["owner_b"]==b && $h["result"]=="FAIL")}
        END {exit !ok}' "$REPORT_DIR/report.tsv"
}
capture() {
    report_reset
    report_capture_observed
    report_emit FAIL >"$scratch/emission"
    check report_matches "$@"
    check test -s "$REPORT_DIR/report.txt"
}
for held in none a b ab; do
    check test "$(owners)" = "$held"
    check owner_is "$held"
    case "$held" in
        a) check single_owner; check test "$OWNER" = a; check strict_owner_activated; capture a yes no ;;
        b) check single_owner; check test "$OWNER" = b; check strict_owner_activated; capture b no yes ;;
        ab) if single_owner; then failed=1; fi; capture ab yes yes ;;
        none) if single_owner; then failed=1; fi; check network_restricted; capture none no no ;;
    esac
done
# Podman existence status 1 is confirmed absence, including a destroyed host.
for node in a b; do
    held=ab
    printf -v "exists_$node" '%s' 1
    expected="$(other "$node")"
    check test "$(owners)" = "$expected"
    check owner_is "$expected"
    printf -v "exists_$node" '%s' 0
done
exists_a=1 exists_b=1
check owner_is none
exists_a=0 exists_b=0

# Failures of either observation on either node must yield no owner claim.
for exists_output in "" none; do
    for node in a b; do
        for observation in exists address; do
            for code in 1 2 124 125 137; do
                [[ "$observation:$code" != exists:1 ]] || continue
                held=ab
                printf -v "${observation}_$node" '%s' "$code"
                observed="" actual=0
                observed="$(owners)" || actual=$?
                check test "$actual" -ne 0
                check test -z "$observed"
                for expected in none a b ab ""; do
                    if owner_is "$expected"; then
                        printf 'FAIL unknown accepted as %s (%s/%s/%s)\n' "$expected" "$node" "$observation" "$code"
                        failed=1
                    fi
                done
                if single_owner || strict_owner_activated || network_restricted; then
                    printf 'FAIL unknown accepted by readiness/absence predicate\n'
                    failed=1
                fi
                capture unreadable unreadable unreadable
                printf -v "${observation}_$node" '%s' 0
            done
        done
    done
done
# The report must decode only the closed set even if a reader misbehaves.
owners() { printf '%s' "$synthetic_owners"; return "$synthetic_exit"; }
for synthetic_owners in unreadable banana ba ""; do
    synthetic_exit=0
    capture unreadable unreadable unreadable
done
synthetic_owners=a synthetic_exit=125
capture unreadable unreadable unreadable
printf 'ownership observation synthetic suite exit=%s\n' "$failed"
exit "$failed"
