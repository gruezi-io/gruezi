#!/usr/bin/env bash
# Security-boundary regression: only synthetic passwords; no container or DB.
set -euo pipefail
TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
source "${LAB_TEST_LIB:-$TEST_DIR/lib.sh}"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
lab_root="$ROOT"
ROOT="$scratch/root"
mkdir -p "$ROOT/target/release"
: >"$ROOT/target/release/gruezi"
chmod 700 "$ROOT/target/release/gruezi"
failed=0
reject_sql=0

check() {
    if "$@"; then return 0; fi
    printf 'FAIL %s\n' "$*"
    failed=1
}
# setup_cluster's randomness is synthetic too; no generated live credentials.
od() { printf '42424242424242424242424242424242\n'; }
strict_credentials() {
    REPL_PASSWORD=synthetic-replication
    APP_PASSWORD=synthetic-application
    ADAPTER_PASSWORD=synthetic-adapter
    MONITOR_PASSWORD=synthetic-monitor
}
long_p() { :; }
start_pod() { :; }
start_gruezi() { :; }
wait_for() { :; }
cleanup() { trap - EXIT; if [[ -n "$WORK" ]]; then rm -rf "$WORK"; fi; }
fail() { local code=$?; trap - ERR; exit "$code"; }
p() {
    local argument query="" node="" interactive=0
    printf '%s\n' "$@" >>"$scratch/$PROFILE.argv"
    case "$1 $2" in
        "image exists") return 1 ;;
        "image inspect") printf 'synthetic@sha256:fixture\n'; return ;;
        "exec "*)
            shift
            if [[ "$1" == -i ]]; then interactive=1; shift; fi
            node="${1##*-}"
            shift
            [[ "$1" == mariadb ]] || return 0
            for argument in "$@"; do
                case "$argument" in --execute=*) query="${argument#--execute=}" ;; esac
            done
            if [[ "$interactive" == 1 ]]; then query="$(cat)"; fi
            printf '%s\n-- next query --\n' "$query" >>"$scratch/$PROFILE.$node.sql"
            if [[ "$reject_sql" == 1 ]]; then return 73; fi
            case "$query" in
                'SHOW MASTER STATUS') printf 'mariadb-bin.000001\t328\t\t\n' ;;
                'SELECT @@global.read_only') printf '1\n' ;;
                'SELECT @@global.gtid_strict_mode,'*) printf '1\t1\t1\n' ;;
                'SELECT @@global.gtid_binlog_pos') printf '1-1-1,2-2-1\n' ;;
            esac ;;
    esac
}

for PROFILE in strict available; do
    (setup_cluster) >"$scratch/$PROFILE.stdout" 2>"$scratch/$PROFILE.stderr"
    for node in a b; do
        # A fingerprint receipt compares complete SQL bodies before/after the
        # transport repair, including privileges, schema and replication mode.
        printf 'SQL profile=%s node=%s sha256=%s\n' "$PROFILE" "$node" \
            "$(sha256sum "$scratch/$PROFILE.$node.sql" | cut -d' ' -f1)"
        check grep -Fq 'SET SESSION sql_log_bin=0;' "$scratch/$PROFILE.$node.sql"
        check grep -Fq "CHANGE MASTER TO MASTER_HOST='$(ip_for "$(other "$node")")'" "$scratch/$PROFILE.$node.sql"
        check grep -Fq 'START SLAVE;' "$scratch/$PROFILE.$node.sql"
        if [[ "$PROFILE" == strict ]]; then
            for account in repl app gruezi_adapter gruezi_monitor; do
                check grep -Fq "CREATE USER '$account'@'%'" "$scratch/$PROFILE.$node.sql"
            done
            check grep -Fq 'MASTER_USE_GTID=slave_pos;' "$scratch/$PROFILE.$node.sql"
            check grep -Fq "VALUES('seed-$node',@@server_id);" "$scratch/$PROFILE.$node.sql"
            check grep -Fq 'REPLICATION SLAVE ADMIN, BINLOG ADMIN' "$scratch/$PROFILE.$node.sql"
        else
            check grep -Fq "CREATE USER 'writer'@'%'" "$scratch/$PROFILE.$node.sql"
            check grep -Fq 'MASTER_LOG_POS=328, MASTER_USE_GTID=no;' "$scratch/$PROFILE.$node.sql"
        fi
    done
    for password in synthetic-replication synthetic-application synthetic-adapter synthetic-monitor 42424242424242424242424242424242; do
        if grep -Fq "$password" "$scratch/$PROFILE.argv"; then
            printf 'FAIL provisioning_password_in_argv profile=%s\n' "$PROFILE"
            failed=1
        fi
    done
    check grep -Fxq -- '--defaults-extra-file=/lab/db-client.cnf' "$scratch/$PROFILE.argv"
    # Existing retained-output auditor still owns stdout/stderr checks.
    printf '%s\n' synthetic-replication synthetic-application synthetic-adapter synthetic-monitor 42424242424242424242424242424242 \
        | bash "$TEST_DIR/audit-output.sh" "$scratch/$PROFILE.stdout" "$scratch/$PROFILE.stderr"
done
# A client error must stop provisioning, including before the seed write.
PROFILE=strict
strict_credentials
reject_sql=1
set +e
(set -e; strict_provision a; printf 'unexpected success\n') >"$scratch/failure.stdout" 2>"$scratch/failure.stderr"
code=$?
set -e
check test "$code" -eq 73
check test ! -s "$scratch/failure.stdout"
printf 'provisioning client failure exit=%s\n' "$code"
ROOT="$lab_root"
printf 'provisioning stdin synthetic suite exit=%s\n' "$failed"
exit "$failed"
