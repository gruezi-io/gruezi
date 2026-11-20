#!/usr/bin/env bash
# lib.sh - shared framework for the MariaDB HA acceptance lab.
#
# Sourced by run.sh (the loader) and by run.sh --test-report-emitter. It holds
# the deadline-bounded Podman/SQL helpers, the writer loop, the independent
# observation helpers, timing, and the report emission layer. Scenario files
# under scenarios/ are sourced on top of this and use these functions directly.
#
# Bash 3.2 compatible (no associative arrays, no mapfile). Every external
# container or SQL call goes through a hard deadline; observable predicates use
# wait_for's bounded poll. A poll interval never determines success.
#
# Resource discipline: every Podman resource this lab creates is named
# gruezi-mariadb-lab*; --force destroys only those names; cleanup prints a
# receipt for every container, pod, volume and network it touched.

LAB_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
ROOT="$(cd "$LAB_LIB_DIR/../.." && pwd -P)"
HERE="$LAB_LIB_DIR"
NAME=gruezi-mariadb-lab
NETWORK="$NAME"
IMAGE="localhost/$NAME:acceptance"
DB_LAB_IMAGE="$IMAGE-db"
DB_IMAGE="${MARIADB_IMAGE:-docker.io/library/mariadb:11.8}"
DB_DIGEST="unpinned"
VIP=10.249.15.100
PODMAN=(podman)
WORK=""
OWN_RESOURCES=0
SCENARIO=setup
WITH_HOOK=0
FORCE=0
OWNER=""
ROW=""
PROFILE=available
SCENARIO_FILTER=""
REPORT_DIR="${REPORT_DIR:-${TMPDIR:-/tmp}/gruezi-mariadb-lab-report}"
RUN_ID="run-$(date -u +%Y%m%dT%H%M%SZ)-$$"
REPORT_HEADER_WRITTEN=0
SCENARIO_IDS=()
SCENARIO_PROFILES=()
SCENARIO_FUNCTIONS=()

register_scenario() {
    SCENARIO_PROFILES+=("$1")
    SCENARIO_IDS+=("$2")
    SCENARIO_FUNCTIONS+=("$3")
}

validate_scenario() {
    local i
    [[ -n "$SCENARIO_FILTER" ]] || return 0
    for ((i=0; i<${#SCENARIO_IDS[@]}; i++)); do
        if [[ "${SCENARIO_PROFILES[i]}:${SCENARIO_IDS[i]}" == "$PROFILE:$SCENARIO_FILTER" ]]; then
            SELECTED_HANDLER="${SCENARIO_FUNCTIONS[i]}"
            case "$SCENARIO_FILTER" in read-only-hook*) WITH_HOOK=1 ;; esac
            return 0
        fi
    done
    printf 'Unknown scenario for %s: %s\n' "$PROFILE" "$SCENARIO_FILTER" >&2
    exit 2
}

run_available() {
    run_available_core
    if [[ "$WITH_HOOK" == 1 ]]; then run_available_hooks; fi
}

setup_cluster() {
    local node peer master file position rest leftovers=0 kind
    [[ -x "$ROOT/target/release/gruezi" ]] || {
        printf 'Missing target/release/gruezi; run cargo build --release --locked\n' >&2; exit 1;
    }
    # Probe only the lab prefix, including containers outside a surviving pod.
    for kind in container pod volume network; do
        case "$kind" in
            container) rest="$(p ps -a --format '{{.Names}}' --filter "name=$NAME")" ;;
            pod) rest="$(p pod ps --format '{{.Name}}' --filter "name=$NAME")" ;;
            *) rest="$(p "$kind" ls --format '{{.Name}}' --filter "name=$NAME")" ;;
        esac
        rest="$(printf '%s\n' "$rest" | awk '$0 !~ /^gruezi-mariadb-lab-(fix|verify)/')"
        [[ -z "$rest" ]] || leftovers=1
    done
    if p image exists "$IMAGE" || p image exists "$DB_LAB_IMAGE"; then leftovers=1; fi
    if [[ "$leftovers" == 1 && "$FORCE" == 0 ]]; then
        printf 'Lab resources exist; use --force to remove only %s resources\n' "$NAME" >&2
        exit 1
    fi
    OWN_RESOURCES=1
    trap cleanup EXIT
    trap 'fail "$LINENO" "$BASH_COMMAND"' ERR
    trap 'exit 130' INT
    trap 'exit 143' TERM
    if [[ "$FORCE" == 1 ]]; then
        for node in a b; do
            reap_resource container "$NAME-gz-$node"
            reap_resource container "$NAME-db-$node"
            reap_resource pod "$NAME-$node"
            reap_resource volume "$NAME-data-$node"
            reap_resource volume "$NAME-run-$node"
        done
        reap_resource network "$NETWORK"
    fi
    WORK="$(mktemp -d "${TMPDIR:-/tmp}/gruezi-mariadb.XXXXXXXX")"
    chmod 700 "$WORK"
    KEY="$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')"
    PASSWORD="$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')"
    printf '[client]\nuser=root\npassword=%s\n' "$PASSWORD" | atomic "$WORK/db-client.cnf"
    printf 'MARIADB_ROOT_PASSWORD=%s\n' "$PASSWORD" | atomic "$WORK/db.env"
    report_reset
    long_p build -f "$HERE/Containerfile" -t "$IMAGE" "$ROOT"
    long_p pull "$DB_IMAGE"
    DB_DIGEST="$(p image inspect --format '{{index .RepoDigests 0}}' "$DB_IMAGE")"
    [[ "$DB_DIGEST" == *@sha256:* ]]
    printf 'IMAGE pinned=%s\n' "$DB_DIGEST"
    printf 'FROM %s\nUSER root\nRUN apt-get update && apt-get install -y --no-install-recommends iproute2 && rm -rf /var/lib/apt/lists/*\n' "$DB_DIGEST" \
        | atomic "$WORK/Containerfile.db"
    long_p build -f "$WORK/Containerfile.db" -t "$DB_LAB_IMAGE" "$WORK"
    p network create --subnet 10.249.15.0/24 "$NETWORK"
    if [[ "$PROFILE" == strict ]]; then strict_credentials; fi
    for node in a b; do
        p volume create "$NAME-data-$node"
        if [[ "$PROFILE" == strict ]]; then p volume create "$NAME-run-$node"; fi
        start_pod "$node"
        wait_for "MariaDB $node ready" ready "$node"
        if [[ "$PROFILE" == strict ]]; then
            strict_provision "$node"
            # The adapter (gruezi-actuator) derives the DB incarnation from this
            # pidfile; expose only the pid, read-only, through the shared run volume.
            p exec --user root "$NAME-db-$node" sh -c 'chmod 755 /run/mysqld && chmod 644 /run/mysqld/gruezi-lab.pid'
        else
            sql "$node" "SET SESSION sql_log_bin=0;
CREATE USER 'repl'@'%' IDENTIFIED BY '$PASSWORD';
GRANT REPLICATION SLAVE ON *.* TO 'repl'@'%';
CREATE USER 'writer'@'%' IDENTIFIED BY '$PASSWORD';
CREATE DATABASE lab;
CREATE TABLE lab.writes (id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
 token VARCHAR(64) UNIQUE NOT NULL, origin INT NOT NULL);
GRANT SELECT,INSERT ON lab.* TO 'writer'@'%';"
        fi
    done
    for node in a b; do
        peer="$(other "$node")"
        if [[ "$PROFILE" == strict ]]; then
            sql "$node" "CHANGE MASTER TO MASTER_HOST='$(ip_for "$peer")',
MASTER_USER='repl', MASTER_PASSWORD='$REPL_PASSWORD', MASTER_USE_GTID=slave_pos; START SLAVE;"
        else
            master="$(sql "$peer" 'SHOW MASTER STATUS')"
            read -r file position rest <<<"$master"
            sql "$node" "CHANGE MASTER TO MASTER_HOST='$(ip_for "$peer")',
MASTER_USER='repl', MASTER_PASSWORD='$PASSWORD', MASTER_LOG_FILE='$file',
MASTER_LOG_POS=$position, MASTER_USE_GTID=no; START SLAVE;"
        fi
    done
    if [[ "$PROFILE" == strict ]]; then
        wait_for "both executed vectors contain both seeded domains" strict_seeded
        START_RO_A="$(read_only_of a)"
        START_RO_B="$(read_only_of b)"
        [[ "$START_RO_A" == 1 && "$START_RO_B" == 1 ]]
        printf 'STRICT initial read_only=%s/%s watchdog=off: pods have no watchdog device\n' "$START_RO_A" "$START_RO_B"
        printf 'STRICT real-server scalars (strict_mode,log_bin,log_slave_updates): '
        sql a 'SELECT @@global.gtid_strict_mode,@@global.log_bin,@@global.log_slave_updates;'
    fi
    for node in a b; do start_gruezi "$node"; done
}

strict_credentials() {
    local node peer id peer_id user password
    APP_PASSWORD="$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')"
    ADAPTER_PASSWORD="$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')"
    MONITOR_PASSWORD="$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')"
    REPL_PASSWORD="$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')"
    for user in app monitor repl; do
        case "$user" in
            app) password="$APP_PASSWORD" ;;
            monitor) password="$MONITOR_PASSWORD" ;;
            repl) password="$REPL_PASSWORD" ;;
        esac
        local sql_user="$user"
        if [[ "$user" == monitor ]]; then sql_user=gruezi_monitor; fi
        printf '[client]\nuser=%s\npassword=%s\n' "$sql_user" "$password" | atomic "$WORK/$user.cnf"
    done
    for node in a b; do
        peer="$(other "$node")"; id=1; peer_id=2
        if [[ "$node" == b ]]; then id=2; peer_id=1; fi
        mkdir -m 700 "$WORK/strict-$node"
        printf '[client]\nuser=gruezi_adapter\npassword=%s\nhost=127.0.0.1\nprotocol=tcp\n' "$ADAPTER_PASSWORD" \
            | atomic "$WORK/strict-$node/adapter-local.cnf"
        printf '[client]\nuser=gruezi_monitor\npassword=%s\nhost=%s\nprotocol=tcp\n' "$MONITOR_PASSWORD" "$(ip_for "$peer")" \
            | atomic "$WORK/strict-$node/adapter-peer.cnf"
        {
            printf 'OWN_SERVER_ID=%s\nPEER_SERVER_ID=%s\nOWN_DOMAIN=%s\nPEER_DOMAIN=%s\n' "$id" "$peer_id" "$id" "$peer_id"
            printf 'LOCAL_OPTION_FILE=/etc/gruezi/mariadb/adapter-local.cnf\nPEER_OPTION_FILE=/etc/gruezi/mariadb/adapter-peer.cnf\n'
            printf 'STATE_DIR=/var/lib/gruezi/mariadb-adapter\nCONNECT_TIMEOUT_S=1\nSTATEMENT_TIMEOUT_S=3\nCOMMAND_TIMEOUT_S=7\nMAX_APPLY_WAIT_S=3\n'
        } | atomic "$WORK/strict-$node/adapter.conf"
    done
}

strict_provision() {
    local node="$1"
    sql "$node" "SET SESSION sql_log_bin=0;
CREATE USER 'repl'@'%' IDENTIFIED BY '$REPL_PASSWORD';
GRANT REPLICATION SLAVE ON *.* TO 'repl'@'%';
CREATE USER 'app'@'%' IDENTIFIED BY '$APP_PASSWORD';
CREATE DATABASE lab;
CREATE TABLE lab.writes (id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
 token VARCHAR(64) UNIQUE NOT NULL, origin INT NOT NULL);
GRANT SELECT,INSERT ON lab.* TO 'app'@'%';
CREATE DATABASE gruezi_ha;
CREATE TABLE gruezi_ha.handoff_release (
singleton_id TINYINT UNSIGNED NOT NULL PRIMARY KEY,
transfer_id BIGINT UNSIGNED NOT NULL, server_id INT UNSIGNED NOT NULL,
gruezi_instance_id VARCHAR(64) NOT NULL, generation BIGINT UNSIGNED NOT NULL,
db_incarnation VARCHAR(128) NOT NULL, target_gtid_binlog_pos VARCHAR(512) NOT NULL,
target_gtid_current_pos VARCHAR(512) NOT NULL, target_gtid_slave_pos VARCHAR(512) NOT NULL,
target_gtid_binlog_state VARCHAR(1024) NOT NULL,
release_nonce CHAR(32) NOT NULL, record_digest CHAR(64) NOT NULL,
released_at_utc VARCHAR(32) NOT NULL) ENGINE=MEMORY DEFAULT CHARSET=ascii;
CREATE USER 'gruezi_adapter'@'%' IDENTIFIED BY '$ADAPTER_PASSWORD';
GRANT READ_ONLY ADMIN, CONNECTION ADMIN, REPLICA MONITOR, PROCESS,
REPLICATION SLAVE ADMIN, BINLOG ADMIN ON *.* TO 'gruezi_adapter'@'%';
GRANT SELECT,INSERT,UPDATE,DELETE ON gruezi_ha.handoff_release TO 'gruezi_adapter'@'%';
CREATE USER 'gruezi_monitor'@'%' IDENTIFIED BY '$MONITOR_PASSWORD';
GRANT REPLICA MONITOR ON *.* TO 'gruezi_monitor'@'%';
GRANT SELECT ON gruezi_ha.handoff_release TO 'gruezi_monitor'@'%';"
    # F-02: exactly one binlogged sentinel transaction in each own domain.
    sql "$node" "INSERT INTO lab.writes(token,origin) VALUES('seed-$node',@@server_id);"
}

strict_seeded() {
    local node vector
    for node in a b; do
        vector="$(sql "$node" 'SELECT @@global.gtid_binlog_pos')" || return 1
        [[ ",$vector," == *,1-1-* && ",$vector," == *,2-2-* ]] || return 1
        [[ "$(replica_field "$node" Slave_IO_Running)" == Yes && "$(replica_field "$node" Slave_SQL_Running)" == Yes ]] || return 1
    done
}

# ---------------------------------------------------------------------------
# Deadline-bounded Podman and SQL helpers (the pre-split run.sh surface).
# ---------------------------------------------------------------------------

p() { timeout --signal=KILL 45 "${PODMAN[@]}" "$@"; }
long_p() { timeout --signal=KILL 600 "${PODMAN[@]}" "$@"; }
gx() { local node="$1"; shift; p exec "$NAME-gz-$node" "$@"; }
db() {
    local node="$1"; shift
    p exec "$NAME-db-$node" mariadb --defaults-extra-file=/lab/db-client.cnf \
        --connect-timeout=1 --batch --skip-column-names "$@"
}
sql() { db "$1" --execute="$2"; }
other() { if [[ "$1" == a ]]; then printf b; else printf a; fi; }
ip_for() { if [[ "$1" == a ]]; then printf 10.249.15.11; else printf 10.249.15.12; fi; }

# Run the mariadb client in a DB container with a caller-chosen option file.
# Credentials live only in option files, never in argv.
db_with() {
    local options="$1" node="$2"
    shift 2
    p exec "$NAME-db-$node" mariadb --defaults-extra-file="$options" \
        --connect-timeout=1 --batch --skip-column-names "$@"
}

# ---------------------------------------------------------------------------
# Independent observation helpers. Kernel truth for the VIP comes from the
# namespaces themselves; never infer VIP ownership from /status.
# ---------------------------------------------------------------------------

owners() {
    local node addresses result=""
    for node in a b; do
        # A missing pod is a crashed host, not an empty successful ip response.
        if ! p pod exists "$NAME-$node"; then continue; fi
        addresses="$(p exec "$NAME-db-$node" ip -o addr show dev eth0)" || return 1
        if printf '%s\n' "$addresses" | awk -v vip="$VIP/24" \
            '$3 == "inet" && $4 == vip {found=1} END {exit !found}'; then
            result="${result}${node}"
        fi
    done
    printf '%s' "${result:-none}"
}
owner_is() { [[ "$(owners)" == "$1" ]]; }
single_owner() {
    OWNER="$(owners)" || return 1
    [[ "$OWNER" == a || "$OWNER" == b ]]
}
now_ms() { date +%s%3N; }
read_only_of() { sql "$1" 'SELECT @@global.read_only'; }
gtid_binlog_state_of() { sql "$1" 'SELECT @@global.gtid_binlog_state'; }

# Parse one field from SHOW ALL SLAVES STATUS (\G output) by field name.
replica_field() {
    db "$1" --column-names --execute='SHOW ALL SLAVES STATUS\G' \
        | sed -n "s/^[[:space:]]*$2:[[:space:]]*//p" | sed -n 1p
}

status_json() { gx "$1" curl --max-time 2 -fsS http://127.0.0.1:9376/status; }

# JSON path scanner: prints "path<TAB>value" for every scalar in the JSON read
# from stdin. Handles nested objects/arrays and quoted braces/escapes, so a
# sanitized stderr tail cannot break extraction. POSIX awk only.
JSON_PATHS_AWK='
function fail(m) { print "json_error:" m > "/dev/stderr"; bad=1; exit 1 }
function skipws(   c) {
    while (i <= n) {
        c = substr(s, i, 1)
        if (c == " " || c == "\t" || c == "\n" || c == "\r") i++
        else return
    }
}
function scan_string(   c, out, esc) {
    i++
    out = ""
    while (i <= n) {
        c = substr(s, i, 1)
        if (c == "\\") {
            i++
            esc = substr(s, i, 1)
            if (esc == "n") out = out "\n"
            else if (esc == "t") out = out "\t"
            else if (esc == "r") out = out "\r"
            else if (esc == "u") { out = out "?"; i += 4 }
            else out = out esc
            i++
        } else if (c == "\"") { i++; STR = out; return }
        else { out = out c; i++ }
    }
    fail("unterminated_string")
}
function curpath(   j, path) {
    path = ""
    for (j = 1; j <= depth; j++) {
        if (kind[j] == "a") path = path "[" idx[j] "]"
        else path = path "." keys[j]
    }
    return "$" path
}
function parse_value(   c, t) {
    skipws()
    if (i > n) fail("eof")
    c = substr(s, i, 1)
    if (c == "{") { i++; parse_object() }
    else if (c == "[") { i++; parse_array() }
    else if (c == "\"") { scan_string(); printf "%s\t%s\n", curpath(), STR }
    else {
        t = ""
        while (i <= n && substr(s, i, 1) ~ /[0-9A-Za-z+\-.]/) { t = t substr(s, i, 1); i++ }
        if (t == "") fail("bad_value")
        printf "%s\t%s\n", curpath(), t
    }
}
function parse_object(   c) {
    depth++
    kind[depth] = "o"
    keys[depth] = ""
    skipws()
    if (substr(s, i, 1) == "}") { i++; depth--; return }
    while (1) {
        skipws()
        if (substr(s, i, 1) != "\"") fail("bad_key")
        scan_string()
        keys[depth] = STR
        skipws()
        if (substr(s, i, 1) != ":") fail("bad_colon")
        i++
        parse_value()
        skipws()
        c = substr(s, i, 1)
        i++
        if (c == "}") { depth--; return }
        if (c != ",") fail("bad_object")
    }
}
function parse_array(   c) {
    depth++
    kind[depth] = "a"
    idx[depth] = 0
    skipws()
    if (substr(s, i, 1) == "]") { i++; depth--; return }
    while (1) {
        idx[depth]++
        parse_value()
        skipws()
        c = substr(s, i, 1)
        i++
        if (c == "]") { depth--; return }
        if (c != ",") fail("bad_array")
    }
}
BEGIN {
    s = ""
    while ((getline line) > 0) s = s line "\n"
    n = length(s)
    i = 1
    depth = 0
    parse_value()
    skipws()
    if (i <= n) fail("trailing_input")
}
'

# json_get PATH [JSON-on-stdin]: exact-path lookup into the scanner output.
json_get() {
    awk "$JSON_PATHS_AWK" | awk -F'\t' -v p="$1" '!found && $1 == p { print $2; found = 1 }'
}

# ---------------------------------------------------------------------------
# Failure handling and cleanup receipts.
# ---------------------------------------------------------------------------

dump_state() {
    local node
    if [[ "${HANDOFF_OBSERVERS_ARMED:-0}" == 1 ]]; then handoff_collect_observers || true; fi
    printf 'OBSERVED kernel_owner=%s\n' "$(owners 2>/dev/null || printf unreadable)"
    for node in a b; do
        p exec "$NAME-db-$node" ip -o addr show dev eth0 || true
        gx "$node" curl --max-time 2 -fsS http://127.0.0.1:9376/status || true
        db "$node" --column-names --execute='SELECT * FROM lab.writes ORDER BY id DESC LIMIT 5; SHOW SLAVE STATUS\G' || true
        p logs "$NAME-gz-$node" || true
        # Strict-profile extras are best effort; their absence must never
        # mask the primary failure being dumped.
        if [[ "$PROFILE" == strict ]]; then
            db "$node" --column-names --execute='SHOW ALL SLAVES STATUS\G' || true
            gx "$node" ls -la /var/lib/gruezi/mariadb-adapter 2>/dev/null || true
        fi
    done
}
wait_for() {
    local description="$1" deadline=$((SECONDS + 60))
    shift
    while (( SECONDS < deadline )); do
        if "$@"; then return 0; fi
        # Polling interval only: completion always depends on observable state.
        sleep 0.2
    done
    printf 'Timeout: %s\n' "$description" >&2
    return 1
}
fail() {
    local code=$?
    trap - ERR
    printf 'FAIL_MONOTONIC_MS=' >&2
    awk '{printf "%.0f\n",$1*1000}' /proc/uptime >&2
    printf 'FAIL_DETAIL source=%s line=%s command=%s\n' "${BASH_SOURCE[1]}" "${1:-unknown}" "${2:-unknown}" >&2
    report_capture_observed || true
    report_field notes "exit=$code" || true
    report_emit FAIL || true
    printf 'FAIL %s kernel_owner=%s row=%s exit=%s\n' "$SCENARIO" \
        "$(owners 2>/dev/null || printf unreadable)" "${ROW:-none}" "$code" >&2
    dump_state >&2
    exit "$code"
}
# Remove one resource with a receipt. A resource a scenario destroyed earlier
# is reported as already_absent, never as a cleanup failure.
reap_resource() {
    local kind="$1" name="$2"
    if p "$kind" exists "$name" >/dev/null 2>&1; then
        local remove=( "$kind" rm )
        case "$kind" in container|pod|volume) remove+=( -f ) ;; esac
        if p "${remove[@]}" "$name" >/dev/null 2>&1; then
            printf 'RECEIPT %s %s removed=1\n' "$kind" "$name"
        else
            printf 'RECEIPT %s %s removed=0\n' "$kind" "$name"
            return 1
        fi
    else
        printf 'RECEIPT %s %s already_absent=1\n' "$kind" "$name"
    fi
    return 0
}
cleanup() {
    local code=$? node failed=0 resource
    trap - EXIT
    if [[ -n "${LAB_TRANSCRIPT:-}" && -n "$WORK" ]]; then
        audit_fixture_output || { printf 'FAIL fixture_authentication_material_in_output\n' >&2; failed=1; }
    fi
    if [[ "$OWN_RESOURCES" == 1 ]]; then
        for node in a b; do
            reap_resource container "$NAME-gz-$node" || failed=1
            reap_resource container "$NAME-db-$node" || failed=1
            reap_resource pod "$NAME-$node" || failed=1
            reap_resource container "$NAME-infra-$node" || failed=1
            reap_resource volume "$NAME-data-$node" || failed=1
            if [[ "$PROFILE" == strict ]]; then reap_resource volume "$NAME-run-$node" || failed=1; fi
        done
        reap_resource network "$NETWORK" || failed=1
        reap_resource image "$IMAGE" || failed=1
        reap_resource image "$DB_LAB_IMAGE" || failed=1
        audit_resources || failed=1
        printf 'CLEANUP pods/containers/volumes/network/image removed=%s\n' "$((1 - failed))"
    fi
    if [[ -n "$WORK" ]]; then
        if rm -rf "$WORK" && [[ ! -e "$WORK" ]]; then
            printf 'RECEIPT private_work path=%s removed=1\n' "$WORK"
        else
            printf 'RECEIPT private_work path=%s removed=0\n' "$WORK"; failed=1
        fi
    fi
    if [[ "$failed" != 0 ]]; then code=1; fi
    exit "$code"
}

audit_fixture_output() {
    local password first escaped verifier path paths=("$LAB_TRANSCRIPT")
    for path in "$REPORT_DIR"/*; do if [[ -f "$path" ]]; then paths+=("$path"); fi; done
    {
        printf '%s\n' "${KEY:-}"
        for password in "${PASSWORD:-}" "${APP_PASSWORD:-}" "${ADAPTER_PASSWORD:-}" "${MONITOR_PASSWORD:-}" "${REPL_PASSWORD:-}"; do
            [[ -n "$password" ]] || continue
            printf '%s\n' "$password"
            first="$(printf '%s' "$password" | sha1sum | cut -c1-40)"
            escaped="$(printf '%s' "$first" | sed 's/../\\x&/g')"
            verifier="$(printf '%b' "$escaped" | sha1sum | cut -c1-40 | tr '[:lower:]' '[:upper:]')"
            printf '%s\n' "$verifier"
        done
    } | bash "$HERE/audit-output.sh" "${paths[@]}"
}

audit_resources() {
    local kind names remaining excluded failed=0
    for kind in container pod volume network; do
        case "$kind" in
            container) names="$(p ps -a --format '{{.Names}}' --filter "name=$NAME")" || return 1 ;;
            pod) names="$(p pod ps --format '{{.Name}}' --filter "name=$NAME")" || return 1 ;;
            *) names="$(p "$kind" ls --format '{{.Name}}' --filter "name=$NAME")" || return 1 ;;
        esac
        remaining="$(printf '%s\n' "$names" | awk 'NF && $0 !~ /^gruezi-mariadb-lab-(fix|verify)/')"
        excluded="$(printf '%s\n' "$names" | awk '$0 ~ /^gruezi-mariadb-lab-(fix|verify)/')"
        printf 'RECEIPT audit %s remaining=%s excluded_fixer=%s\n' "$kind" "${remaining:-zero}" "${excluded:-none}"
        [[ -z "$remaining" ]] || failed=1
    done
    return "$failed"
}
prerequisites() {
    local command
    for command in podman ip timeout od awk sed mktemp; do
        if ! command -v "$command" >/dev/null 2>&1; then
            printf 'Prerequisite missing: %s (install podman and iproute2; Rocky: sudo dnf install podman iproute coreutils)\n' "$command" >&2
            return 1
        fi
    done
    if ! ip netns list >/dev/null 2>&1; then
        printf 'Prerequisite missing: usable Linux ip netns support\n' >&2; return 1
    fi
    local error
    if ! error="$(p info 2>&1)"; then
        printf 'NOT RUN Podman-dependent acceptance: %s\n' "$error" >&2
        return 1
    fi
    printf 'Prerequisites OK: %s; ip netns\n' "${PODMAN[*]}"
}

# ---------------------------------------------------------------------------
# Report emission. One record per scenario with the required observation
# fields. Vocabulary is enforced at emission time: NOT RUN is never PASS,
# divergence is detected|unknown only (F-11), and data_loss_possible=no is
# accepted only with a cited PO-evidence reason. report.tsv is appended per
# run (run_id column gives freshness); report.txt is the readable table.
# ---------------------------------------------------------------------------

REPORT_COLUMNS="run_id generated_at_utc profile scenario row_id result kernel_vip_owners owner_a owner_b authority_a authority_b fencing_a fencing_b read_only_start_a read_only_start_b read_only_a read_only_b replication_a replication_b gtid_binlog_pos_a gtid_binlog_pos_b gtid_slave_pos_a gtid_slave_pos_b gtid_binlog_state_a gtid_binlog_state_b gtid_current_pos_a gtid_current_pos_b gtid_io_pos_a gtid_io_pos_b write_token write_origin_server_id write_via failover_ms recovery_ms divergence data_loss_possible data_loss_reason rpo_record receipt_identity receipt_barrier_attestations receipt_target coverage_result watchdog server_version image_digest notes"

report_field() {
    local key="$1" value="${2-}"
    case " $REPORT_COLUMNS " in
        *" $key "*) ;;
        *) printf 'report_field: unknown column: %s\n' "$key" >&2; return 1 ;;
    esac
    value="${value//$'\t'/ }"
    value="${value//$'\n'/ }"
    value="${value//$'\r'/ }"
    printf -v "REPORT_$key" '%s' "$value"
}
report_get() {
    local key="$1"
    [[ "$key" =~ ^[a-z0-9_]+$ ]] || return 1
    eval "printf '%s' \"\${REPORT_$key:-}\""
}
report_reset() {
    local key
    for key in $REPORT_COLUMNS; do
        printf -v "REPORT_$key" '%s' ""
    done
    report_field run_id "$RUN_ID"
    report_field profile "$PROFILE"
    report_field scenario "${SCENARIO:-unknown}"
    report_field row_id "n/a"
    report_field result ""
    report_field kernel_vip_owners "unobserved"
    report_field divergence unknown
    report_field data_loss_possible unknown
    report_field data_loss_reason "no_po_evidence_evaluated"
    report_field rpo_record "n/a"
    report_field receipt_identity "n/a"
    report_field receipt_barrier_attestations "n/a"
    report_field receipt_target "n/a"
    report_field coverage_result "n/a"
    report_field failover_ms "n/a"
    report_field recovery_ms "n/a"
    report_field write_token "n/a"
    report_field write_origin_server_id "n/a"
    report_field write_via "n/a"
    report_field notes ""
    if [[ "$PROFILE" == strict ]]; then
        report_field watchdog "off: pods expose no watchdog device; watchdog rows run only in the VM runner"
    else
        report_field watchdog "n/a: available profile"
    fi
}

# Best-effort collection of the shared observables at record time. Every
# command is individually guarded: collection must never change a scenario's
# outcome, and an unavailable value is recorded as such instead of guessed.
report_capture_observed() {
    local node owners_now ro json
    owners_now="$(owners 2>/dev/null || printf unreadable)"
    report_field kernel_vip_owners "$owners_now"
    for node in a b; do
        case "$owners_now" in
            *"$node"*) report_field "owner_$node" yes ;;
            *) report_field "owner_$node" no ;;
        esac
        ro="$(read_only_of "$node" 2>/dev/null || printf unreadable)"
        report_field "read_only_$node" "$ro"
        local io_running sql_running
        io_running="$(replica_field "$node" Slave_IO_Running 2>/dev/null || printf unknown)"
        sql_running="$(replica_field "$node" Slave_SQL_Running 2>/dev/null || printf unknown)"
        report_field "replication_$node" \
            "io=${io_running:-unknown};sql=${sql_running:-unknown}"
        report_field "gtid_binlog_pos_$node" \
            "$(sql "$node" 'SELECT @@global.gtid_binlog_pos' 2>/dev/null || printf unknown)"
        report_field "gtid_slave_pos_$node" \
            "$(sql "$node" 'SELECT @@global.gtid_slave_pos' 2>/dev/null || printf unknown)"
        report_field "gtid_binlog_state_$node" "$(gtid_binlog_state_of "$node" 2>/dev/null || printf unknown)"
        report_field "gtid_current_pos_$node" "$(sql "$node" 'SELECT @@global.gtid_current_pos' 2>/dev/null || printf unknown)"
        report_field "gtid_io_pos_$node" "$(replica_field "$node" Gtid_IO_Pos 2>/dev/null || printf unknown)"
        json="$(status_json "$node" 2>/dev/null || printf '')"
        if [[ -n "$json" ]]; then
            report_field "authority_$node" "$(printf '%s' "$json" | json_get '$.ha.authority.state')"
            report_field "fencing_$node" "$(printf '%s' "$json" | json_get '$.ha.authority.fencing')"
        else
            report_field "authority_$node" "unreadable"
            report_field "fencing_$node" "unreadable"
        fi
    done
    report_field server_version \
        "$(sql a 'SELECT VERSION()' 2>/dev/null || printf unknown)"
    report_field image_digest "$DB_DIGEST"
    return 0
}

report_emit() {
    local result="$1" key row="" value divergence data_loss reason
    case "$result" in
        PASS | FAIL | "NOT RUN") ;;
        *) printf 'report_emit: invalid result: %s\n' "$result" >&2; return 1 ;;
    esac
    # NOT RUN is structurally impossible as PASS: the caller's verb is the
    # only source of the result cell.
    divergence="$(report_get divergence)"
    case "$divergence" in
        detected | unknown) ;;
        *)
            printf 'report_emit: invalid divergence vocabulary: %s (F-11 allows detected|unknown)\n' \
                "$divergence" >&2
            return 1
            ;;
    esac
    data_loss="$(report_get data_loss_possible)"
    case "$data_loss" in
        yes | no | unknown) ;;
        *)
            printf 'report_emit: invalid data_loss_possible: %s\n' "$data_loss" >&2
            return 1
            ;;
    esac
    reason="$(report_get data_loss_reason)"
    if [[ "$data_loss" == no && "$reason" != *PO-* ]]; then
        printf 'report_emit: data_loss_possible=no requires a PO-evidence reason\n' >&2
        return 1
    fi
    report_field result "$result"
    report_field generated_at_utc "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    mkdir -p "$REPORT_DIR"
    if [[ -s "$REPORT_DIR/report.tsv" && "$REPORT_HEADER_WRITTEN" != 1 ]]; then
        local header
        header="$(awk '!/^#/ {print; exit}' "$REPORT_DIR/report.tsv")"
        [[ "$header" == "$(printf '%s' "$REPORT_COLUMNS" | tr ' ' '\t')" ]] || {
            printf 'Existing report schema differs; choose a new REPORT_DIR\n' >&2
            return 1
        }
        REPORT_HEADER_WRITTEN=1
    fi
    if [[ "$REPORT_HEADER_WRITTEN" != 1 && ! -s "$REPORT_DIR/report.tsv" ]]; then
        {
            printf '# gruezi-mariadb-lab report.tsv v1 (tab separated; one record per scenario)\n'
            printf '# divergence vocabulary: detected|unknown (F-11); data_loss_possible=no requires PO evidence\n'
            printf '%s\n' "$(printf '%s' "$REPORT_COLUMNS" | tr ' ' '\t')"
        } >>"$REPORT_DIR/report.tsv"
        REPORT_HEADER_WRITTEN=1
    fi
    for key in $REPORT_COLUMNS; do
        value="$(report_get "$key")"
        if [[ "$key" != run_id ]]; then row+=$'\t'; fi
        row+="$value"
    done
    printf '%s\n' "$row" >>"$REPORT_DIR/report.tsv"
    report_render_table
    printf '%s\n%s\n' "$(printf '%s' "$REPORT_COLUMNS" | tr ' ' '\t')" "$row"
    printf 'REPORT record scenario=%s row_id=%s result=%s file=%s/report.tsv\n' \
        "$(report_get scenario)" "$(report_get row_id)" "$result" "$REPORT_DIR"
    report_reset
    return 0
}

# Regenerate the readable table from report.tsv on every emission.
report_render_table() {
    awk -F'\t' '
        /^#/ { next }
        !seen_header { for (i = 1; i <= NF; i++) h[i] = $i; hn = NF; seen_header = 1; next }
        {
            n++
            for (i = 1; i <= NF; i++) {
                rowv[n, i] = $i
                if (length($i) > w[i]) w[i] = length($i)
            }
        }
        END {
            for (i = 1; i <= hn; i++) printf "%-*s%s", w[i], h[i], (i == hn ? "\n" : "  ")
            for (i = 1; i <= hn; i++) printf "%-*s%s", w[i], strrep("=", w[i]), (i == hn ? "\n" : "  ")
            for (r = 1; r <= n; r++) {
                for (i = 1; i <= hn; i++) printf "%-*s%s", w[i], rowv[r, i], (i == hn ? "\n" : "  ")
            }
        }
        function strrep(c, k,   out, j) { out = ""; for (j = 0; j < k; j++) out = out c; return out }
    ' "$REPORT_DIR/report.tsv" >"$REPORT_DIR/report.txt"
}

report_not_run() {
    local scenario="$1" row_id="$2" reason="$3"
    report_reset
    report_field scenario "$scenario"
    report_field row_id "$row_id"
    report_field notes "reason=$reason"
    report_emit "NOT RUN"
}

# ---------------------------------------------------------------------------
# Writer loop and shared scenario helpers (pre-split run.sh surface, kept
# byte-compatible in behavior: the printed lines are part of the baseline).
# ---------------------------------------------------------------------------

row_on() { sql "$1" "SELECT CONCAT(token,':server=',origin) FROM lab.writes WHERE token='$ROW'"; }
replicated() {
    local a b
    a="$(row_on a)" && b="$(row_on b)" && [[ -n "$a" && "$a" == "$b" ]]
}
converged() {
    local a b
    a="$(sql a 'SELECT id,token,origin FROM lab.writes ORDER BY id')" || return 1
    b="$(sql b 'SELECT id,token,origin FROM lab.writes ORDER BY id')" || return 1
    [[ -n "$a" && "$a" == "$b" ]]
}
vip_write() {
    local host="$1"
    gx "$host" env MYSQL_PWD="$PASSWORD" mariadb --protocol=tcp --host="$VIP" \
        --user=writer --connect-timeout=1 --batch --skip-column-names \
        --execute="INSERT INTO lab.writes(token,origin) VALUES('$ROW',@@server_id);"
}
writer_loop() {
    # Bounded retries of one idempotent transaction through the VIP. Reconnect
    # for every write, as a real client must after the old TCP endpoint dies.
    local node="$1" observed expected
    if observed="$(row_on "$node")" && [[ -n "$observed" ]]; then
        expected=1; if [[ "$node" == b ]]; then expected=2; fi
        [[ "$observed" == "$ROW:server=$expected" ]]
        return
    fi
    vip_write "$node" >/dev/null 2>&1 || return 1
    observed="$(row_on "$node")" || return 1
    expected=1; if [[ "$node" == b ]]; then expected=2; fi
    [[ "$observed" == "$ROW:server=$expected" ]]
}
new_row() { ROW="${1}-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"; }
pass() {
    if [[ "$PROFILE" == available && -n "$SCENARIO_FILTER" && "$SCENARIO_FILTER" != "$SCENARIO" ]]; then return 0; fi
    report_field scenario "$SCENARIO"
    report_field write_token "${ROW:-n/a}"
    report_field notes "${1:-}"
    report_capture_observed || true
    report_emit PASS
    printf 'PASS %s kernel_owner=%s row_a=%s row_b=%s %s\n' "$SCENARIO" \
        "$(owners)" "$(row_on a 2>/dev/null || printf unavailable)" \
        "$(row_on b 2>/dev/null || printf unavailable)" "${1:-}"
    if [[ "$PROFILE" == available && -n "$SCENARIO_FILTER" ]]; then exit 0; fi
}
write_and_replicate() {
    wait_for "exactly one kernel VIP" single_owner
    new_row "$SCENARIO"
    wait_for "VIP writer reaches owner $OWNER" writer_loop "$OWNER"
    wait_for "row replicates" replicated
}

# ---------------------------------------------------------------------------
# Cluster configuration and pod lifecycle (shared; profile selects shape).
# ---------------------------------------------------------------------------

atomic() {
    local destination="$1" temporary
    temporary="$(mktemp "$WORK/.atomic.XXXXXXXX")"
    cat >"$temporary"
    chmod 600 "$temporary"
    mv "$temporary" "$destination"
}

write_config() {
    local node="$1" peer priority hooks=""
    peer="$(other "$node")"
    priority=100
    if [[ "$node" == a ]]; then priority=110; fi
    if [[ "$WITH_HOOK" == 1 && "${HOOK_PHASE:-0}" == 1 ]]; then
        hooks='  hooks:\n    on_promote: /usr/local/bin/read-only-hook\n    on_demote: /usr/local/bin/read-only-hook\n    on_backup: /usr/local/bin/read-only-hook\n    on_fault: /usr/local/bin/read-only-hook\n    timeout_ms: 4000'
    fi
    sed -e "s/@NODE@/$node/g" -e "s/@PEER@/$peer/g" \
        -e "s/@IP@/$(ip_for "$node")/g" -e "s/@PEER_IP@/$(ip_for "$peer")/g" \
        -e "s/@PRIORITY@/$priority/g" -e "s/@KEY@/$KEY/g" \
        -e "s|@HOOKS@|$hooks|" "$HERE/node.yaml.in" | atomic "$WORK/$node.yaml"
}

write_strict_config() {
    local node="$1" peer priority
    peer="$(other "$node")"
    priority=100
    if [[ "$node" == a ]]; then priority=110; fi
    sed -e "s/@NODE@/$node/g" -e "s/@PEER@/$peer/g" \
        -e "s/@IP@/$(ip_for "$node")/g" -e "s/@PEER_IP@/$(ip_for "$peer")/g" \
        -e "s/@PRIORITY@/$priority/g" -e "s/@KEY@/$KEY/g" \
        "$HERE/node-strict.yaml.in" | atomic "$WORK/$node.yaml"
}

start_pod() {
    local node="$1" id=1 share=""
    if [[ "$node" == b ]]; then id=2; fi
    # The strict profile needs the DB process visible in the sibling adapter
    # container for the B1 /proc incarnation (pid file + /proc/<pid> start
    # time), so its pods share the PID namespace. cgroup is deliberately not
    # in the list: podman 5.4 rejects cgroup sharing alongside the infra
    # container's cgroup namespace.
    if [[ "$PROFILE" == strict ]]; then
        share="--share=ipc,net,uts,pid"
    fi
    # shellcheck disable=SC2086
    p pod create $share --name "$NAME-$node" --infra-name "$NAME-infra-$node" --network "$NETWORK" --ip "$(ip_for "$node")"
    if [[ "$PROFILE" == strict ]]; then
        p run -d --name "$NAME-db-$node" --pod "$NAME-$node" \
            --env-file "$WORK/db.env" -v "$WORK:/lab:ro,z" \
            -v "$NAME-data-$node:/var/lib/mysql" \
            -v "$NAME-run-$node:/run/mysqld" "$DB_LAB_IMAGE" \
            --server-id="$id" --gtid-domain-id="$id" --log-bin=mariadb-bin \
            --log-slave-updates --binlog-format=ROW \
            --auto-increment-increment=2 --auto-increment-offset="$id" \
            --read-only --gtid-strict-mode --event-scheduler=OFF \
            --pid-file=/run/mysqld/gruezi-lab.pid --bind-address=0.0.0.0
    else
        p run -d --name "$NAME-db-$node" --pod "$NAME-$node" \
            --env-file "$WORK/db.env" -v "$WORK:/lab:ro,z" \
            -v "$NAME-data-$node:/var/lib/mysql" "$DB_LAB_IMAGE" \
            --server-id="$id" --log-bin=mariadb-bin --log-slave-updates \
            --binlog-format=ROW --gtid-domain-id="$id" \
            --auto-increment-increment=2 --auto-increment-offset="$id" \
            --bind-address=0.0.0.0
    fi
}
# The entrypoint's temporary bootstrap server accepts socket queries, but runs
# with networking disabled. TCP proves we reached the final server, avoiding
# provisioning during the bootstrap socket's shutdown.
ready() { db "$1" --protocol=tcp --host=127.0.0.1 --execute='SELECT 1' >/dev/null 2>&1; }
start_gruezi() {
    local node="$1"
    if [[ "$PROFILE" == strict ]]; then
        write_strict_config "$node"
        p run -d --name "$NAME-gz-$node" --pod "$NAME-$node" \
            --cap-add NET_ADMIN --cap-add NET_RAW -v "$WORK:/lab:ro,z" \
            -v "$NAME-run-$node:/run/mysqld:ro" \
            --env "LAB_NODE=$node" \
            --env RUST_LOG=gruezi=debug \
            --env GRUEZI_MARIADB_ADAPTER_CONF=/etc/gruezi/mariadb/adapter.conf \
            "$IMAGE" start --config "/lab/$node.yaml"
    else
        write_config "$node"
        p run -d --name "$NAME-gz-$node" --pod "$NAME-$node" \
            --cap-add NET_ADMIN --cap-add NET_RAW -v "$WORK:/lab:ro,z" \
            "$IMAGE" start --config "/lab/$node.yaml"
    fi
}

peer_healthy() {
    local node json
    for node in a b; do
        json="$(gx "$node" curl -fsS --max-time 2 http://127.0.0.1:9376/status)" || return 1
        printf '%s' "$json" | grep -Eq '"reachable"[[:space:]]*:[[:space:]]*true' || return 1
    done
}

# ---------------------------------------------------------------------------
# Strict-profile helpers: seeding, receipt inspection, B4 evidence, gawk
# portability, and the F-05 account/privilege matrix checks.
# ---------------------------------------------------------------------------

# The app writer for the strict profile uses the app account (no SUPER and no
# READ_ONLY ADMIN); the origin is still recorded through @@server_id.
strict_vip_write() {
    local host="$1"
    gx "$host" mariadb --defaults-extra-file=/lab/app.cnf --protocol=tcp \
        --host="$VIP" --connect-timeout=1 --batch --skip-column-names \
        --execute="INSERT INTO lab.writes(token,origin) VALUES('$ROW',@@server_id);"
}
strict_writer_loop() {
    local node="$1" observed expected
    expected=1; if [[ "$node" == b ]]; then expected=2; fi
    if observed="$(row_on "$node")" && [[ -n "$observed" ]]; then
        [[ "$observed" == "$ROW:server=$expected" ]]
        return
    fi
    strict_vip_write "$node" >/dev/null 2>&1 || return 1
    observed="$(row_on "$node")" || return 1
    [[ "$observed" == "$ROW:server=$expected" ]]
}
strict_owner_activated() {
    local owner_other
    single_owner || return 1
    owner_other="$(other "$OWNER")"
    [[ "$(read_only_of "$OWNER")" == 0 && "$(read_only_of "$owner_other")" == 1 ]]
}

# Read the F-15 receipt row through the read-only monitor account (the exact
# carrier channel the successor's adapter uses). Prints tab-separated fields.
receipt_row_via_monitor() {
    local node="$1"
    db_with /lab/monitor.cnf "$node" --host="$(ip_for "$node")" --protocol=tcp \
        --execute="SELECT singleton_id,transfer_id,server_id,gruezi_instance_id,generation,db_incarnation,target_gtid_binlog_pos,target_gtid_current_pos,target_gtid_slave_pos,target_gtid_binlog_state,release_nonce,released_at_utc,record_digest FROM gruezi_ha.handoff_release;"
}

# Canonical serialization for the receipt digest (adapter CARRIER_FIELDS
# order): each field as "key=value\n" in fixed order.
receipt_canonical_digest() {
    local singleton_id="$1" transfer_id="$2" server_id="$3" instance="$4" \
        generation="$5" incarnation="$6" pos="$7" current="$8" slave="$9"
    shift 9
    local state="$1" nonce="$2" released="$3"
    {
        printf 'singleton_id=%s\n' "$singleton_id"
        printf 'transfer_id=%s\n' "$transfer_id"
        printf 'server_id=%s\n' "$server_id"
        printf 'gruezi_instance_id=%s\n' "$instance"
        printf 'generation=%s\n' "$generation"
        printf 'db_incarnation=%s\n' "$incarnation"
        printf 'target_gtid_binlog_pos=%s\n' "$pos"
        printf 'target_gtid_current_pos=%s\n' "$current"
        printf 'target_gtid_slave_pos=%s\n' "$slave"
        printf 'target_gtid_binlog_state=%s\n' "$state"
        printf 'release_nonce=%s\n' "$nonce"
        printf 'released_at_utc=%s\n' "$released"
    } | sha256sum | cut -d' ' -f1
}

# Adapter helper runs inside the strict gruezi container as gruezi-actuator
# (the identity the daemon's lifecycle children use), under container gawk.
adapter_in_container() {
    local node="$1"
    shift
    gx "$node" setpriv --reuid=gruezi-actuator --regid=gruezi-actuator --clear-groups \
        env GRUEZI_MARIADB_ADAPTER_CONF=/etc/gruezi/mariadb/adapter.conf \
        GRUEZI_ADAPTER_TEST=1 /usr/libexec/gruezi/gruezi-mariadb-adapter "$@"
}

# Re-run the adapter's POSIX-awk suite under BOTH host awk (mawk on this host)
# and the lab image's gawk, on identical vectors; outputs must be identical.
adapter_portability_check() {
    local vec="$1" covers_local="$2" covers_target="$3"
    local host_conf="$WORK/host-adapter.conf" host_state="$WORK/host-state"
    local host_parse box_parse host_cover box_cover host_awk box_awk
    mkdir -p "$host_state"
    chmod 700 "$host_state"
    printf '[client]\nuser=root\npassword=unused\n' | atomic "$WORK/host-local.cnf"
    printf '[client]\nuser=root\npassword=unused\n' | atomic "$WORK/host-peer.cnf"
    {
        printf 'OWN_SERVER_ID=1\nPEER_SERVER_ID=2\nOWN_DOMAIN=1\nPEER_DOMAIN=2\n'
        printf 'LOCAL_OPTION_FILE=%s\n' "$WORK/host-local.cnf"
        printf 'PEER_OPTION_FILE=%s\n' "$WORK/host-peer.cnf"
        printf 'STATE_DIR=%s\n' "$host_state"
        printf 'CONNECT_TIMEOUT_S=2\nSTATEMENT_TIMEOUT_S=5\nCOMMAND_TIMEOUT_S=5\nMAX_APPLY_WAIT_S=3\n'
    } | atomic "$host_conf"
    chmod 600 "$WORK/host-local.cnf" "$WORK/host-peer.cnf" "$host_conf"
    host_parse="$(env GRUEZI_MARIADB_ADAPTER_CONF="$host_conf" GRUEZI_ADAPTER_TEST=1 \
        bash "$ROOT/contrib/mariadb/gruezi-mariadb-adapter" internal-gtid-parse state "$vec")" || return 1
    box_parse="$(adapter_in_container a internal-gtid-parse state "$vec")" || return 1
    host_cover="$(env GRUEZI_MARIADB_ADAPTER_CONF="$host_conf" GRUEZI_ADAPTER_TEST=1 \
        bash "$ROOT/contrib/mariadb/gruezi-mariadb-adapter" internal-gtid-covers \
        state "$covers_local" state "$covers_target")" || return 1
    box_cover="$(adapter_in_container a internal-gtid-covers \
        state "$covers_local" state "$covers_target")" || return 1
    host_awk="$(awk -W version 2>&1 | sed -n 1p)"
    box_awk="$(gx a sh -c 'awk -W version 2>&1 | sed -n 1p')"
    printf 'PORTABILITY host_awk=%s container_awk=%s parse_match=%s covers_match=%s\n' \
        "$host_awk" "$box_awk" \
        "$([[ "$host_parse" == "$box_parse" ]] && printf yes || printf no)" \
        "$([[ "$host_cover" == "$box_cover" ]] && printf yes || printf no)"
    [[ "$host_parse" == "$box_parse" && "$host_cover" == "$box_cover" ]]
}

# B4 real-surface evidence: a notify-identity child cannot read the actuator's
# option file, the actuator STATE_DIR, or /proc/<actuator>/fd. The same-UID
# case is eliminated by construction in lifecycle mode (the executor spawns
# actuator and notify children under distinct packaged accounts and refuses to
# start when they are missing or aliased).
b4_eaccensevidence() {
    local node="$1" pid out option= state= proc= env_absent= privilege
    gx "$node" rm -f /tmp/actuator-probe.pid >/dev/null 2>&1 || true
    p exec -d "$NAME-gz-$node" setpriv --no-new-privs --reuid=gruezi-actuator \
        --regid=gruezi-actuator --clear-groups \
        sh -c 'echo $$ >/tmp/actuator-probe.pid; exec 3</etc/gruezi/mariadb/adapter-local.cnf; exec sleep 60'
    wait_for "actuator probe holder" gx_file_nonempty "$node" /tmp/actuator-probe.pid
    pid="$(gx "$node" cat /tmp/actuator-probe.pid)"
    out="$(gx "$node" setpriv --no-new-privs --reuid=gruezi-notify --regid=gruezi-notify --clear-groups \
        env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=/nonexistent \
        sh -c 'cat /etc/gruezi/mariadb/adapter-local.cnf' 2>&1)" \
        && option="UNEXPECTED_SUCCESS" || option="$out"
    out="$(gx "$node" setpriv --no-new-privs --reuid=gruezi-notify --regid=gruezi-notify --clear-groups \
        env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=/nonexistent \
        sh -c 'ls /var/lib/gruezi/mariadb-adapter' 2>&1)" \
        && state="UNEXPECTED_SUCCESS" || state="$out"
    out="$(gx "$node" setpriv --no-new-privs --reuid=gruezi-notify --regid=gruezi-notify --clear-groups \
        env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=/nonexistent \
        sh -c "ls /proc/$pid/fd" 2>&1)" \
        && proc="UNEXPECTED_SUCCESS" || proc="$out"
    out="$(gx "$node" setpriv --no-new-privs --reuid=gruezi-notify --regid=gruezi-notify --clear-groups \
        env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=/nonexistent \
        sh -c 'env' 2>/dev/null | grep -c '^GRUEZI_ADAPTER_TEST=' || true)"
    [[ "$out" == 0 ]] && env_absent=yes || env_absent=no
    privilege="$(gx "$node" setpriv --no-new-privs --reuid=gruezi-notify --regid=gruezi-notify --clear-groups /usr/local/bin/identity-probe)"
    printf 'B4 setuid_exec_with_no_new_privs=%s\n' "$privilege"
    [[ "$privilege" == "uid=992 euid=992" ]]
    printf 'B4 node=%s actuator_pid=%s\n' "$node" "$pid"
    printf 'B4 option_file_read=%s\n' "$option"
    printf 'B4 state_dir_list=%s\n' "$state"
    printf 'B4 proc_fd_list=%s\n' "$proc"
    printf 'B4 adapter_test_env_absent_in_notify_child=%s\n' "$env_absent"
    printf 'B4 same_uid_case=eliminated_by_construction_in_lifecycle_mode(distinct packaged accounts; startup fails closed when missing or aliased)\n'
    gx "$node" kill "$pid" >/dev/null 2>&1 || true
    [[ "$option" == *Permission\ denied* && "$state" == *Permission\ denied* \
        && "$proc" == *Permission\ denied* && "$env_absent" == yes ]]
}
gx_file_nonempty() {
    local node="$1" path="$2" value
    value="$(gx "$node" cat "$path" 2>/dev/null || true)"
    [[ -n "$value" ]]
}

# ---------------------------------------------------------------------------
# Report-emitter self-test (no Podman). Exercises the vocabulary gates, the
# sanitization, the NOT RUN path and the readable-table regeneration.
# ---------------------------------------------------------------------------

report_emitter_self_test() {
    local saved_dir="$REPORT_DIR" saved_id="$RUN_ID" tmp failed=0 out
    tmp="$(mktemp -d "${TMPDIR:-/tmp}/gruezi-report-test.XXXXXXXX")"
    REPORT_DIR="$tmp"
    RUN_ID=self-test
    REPORT_HEADER_WRITTEN=0
    printf 'self-test: report emitter unit paths (no Podman)\n'

    SCENARIO=unit-pass report_reset
    report_field scenario unit-pass
    report_emit PASS >/dev/null
    out="$(tail -n 1 "$tmp/report.tsv")"
    if [[ "$out" == *$'\t'"PASS"$'\t'* ]]; then
        printf 'PASS report_emitter_writes_pass_record\n'
    else
        printf 'FAIL report_emitter_writes_pass_record\n'; failed=1
    fi

    SCENARIO=unit-vocab report_reset
    report_field scenario unit-vocab
    report_field divergence clean
    if report_emit PASS >/dev/null 2>&1; then
        printf 'FAIL report_emitter_rejects_divergence_clean\n'; failed=1
    else
        printf 'PASS report_emitter_rejects_divergence_clean\n'
    fi

    SCENARIO=unit-dl report_reset
    report_field scenario unit-dl
    report_field data_loss_possible no
    report_field data_loss_reason "assumed_safe"
    if report_emit PASS >/dev/null 2>&1; then
        printf 'FAIL report_emitter_requires_po_evidence_for_no\n'; failed=1
    else
        printf 'PASS report_emitter_requires_po_evidence_for_no\n'
    fi

    report_not_run unit-not-run n/a podman_unavailable
    out="$(tail -n 1 "$tmp/report.tsv")"
    if [[ "$out" == *$'\t'"NOT RUN"$'\t'* ]]; then
        printf 'PASS report_emitter_not_run_is_never_pass\n'
    else
        printf 'FAIL report_emitter_not_run_is_never_pass\n'; failed=1
    fi

    SCENARIO=unit-sanitize report_reset
    report_field scenario unit-sanitize
    report_field notes "$(printf 'a\tb\nc')"
    report_emit PASS >/dev/null
    out="$(tail -n 1 "$tmp/report.tsv")"
    if [[ "$out" != *$'\t'* ]]; then
        printf 'FAIL report_emitter_row_is_tab_separated\n'; failed=1
    elif [[ "$(printf '%s' "$out" | tr -cd '\t' | wc -c)" == "$(( $(printf '%s' "$REPORT_COLUMNS" | wc -w) - 1 ))" ]]; then
        printf 'PASS report_emitter_sanitizes_control_whitespace\n'
    else
        printf 'FAIL report_emitter_sanitizes_control_whitespace\n'; failed=1
    fi

    if [[ -s "$tmp/report.txt" ]] && grep -q 'unit-pass' "$tmp/report.txt"; then
        printf 'PASS report_emitter_renders_readable_table\n'
    else
        printf 'FAIL report_emitter_renders_readable_table\n'; failed=1
    fi

    # JSON path scanner unit check (status extraction dependency).
    out="$(printf '%s' '{"authority":{"state":"granted","fencing":"granted"},"safety":{"why":"ok {x}"},"lifecycle":{"configured":true,"phase":"ready","last_critical_failure":{"outcome":"Failed","stderr_tail":"reason=a\"b{c}"}}}' \
        | json_get '$.lifecycle.last_critical_failure.stderr_tail')"
    if [[ "$out" == 'reason=a"b{c}' ]]; then
        printf 'PASS report_emitter_json_path_scanner_handles_nested_and_escaped\n'
    else
        printf 'FAIL report_emitter_json_path_scanner_handles_nested_and_escaped got=%s\n' "$out"; failed=1
    fi

    out="$(printf '%s' '{"mode":"ha","ha":{"authority":{"state":"granted","fencing":"none"}}}' \
        | json_get '$.ha.authority.state')"
    if [[ "$out" == granted ]]; then
        printf 'PASS report_emitter_reads_ha_authority_status\n'
    else
        printf 'FAIL report_emitter_reads_ha_authority_status got=%s\n' "$out"; failed=1
    fi

    rm -rf "$tmp"
    REPORT_DIR="$saved_dir"
    RUN_ID="$saved_id"
    return "$failed"
}
