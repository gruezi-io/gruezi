#!/usr/bin/env bash
# Bash 3.2 compatible. All external container/SQL calls have a hard deadline.
set -eEuo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
HERE="$ROOT/scripts/mariadb-ha-lab"
NAME=gruezi-mariadb-lab
NETWORK="$NAME"
IMAGE="localhost/$NAME:acceptance"
DB_LAB_IMAGE="$IMAGE-db"
DB_IMAGE="${MARIADB_IMAGE:-docker.io/library/mariadb:11.4}"
VIP=10.249.15.100
PODMAN=(podman)
WORK=""
OWN_RESOURCES=0
SCENARIO=setup
WITH_HOOK=0
FORCE=0
CHECK_ONLY=0
OWNER=""
ROW=""

usage() {
    printf '%s\n' "Usage: $0 [--force] [--with-hook] [--check-prerequisites]" \
        "Requires Linux, podman, ip netns, timeout, od and a release binary." \
        "Uses rootless Podman first; falls back to sudo -n podman if info fails."
}
for arg in "$@"; do
    case "$arg" in
        --force) FORCE=1 ;;
        --with-hook) WITH_HOOK=1 ;;
        --check-prerequisites) CHECK_ONLY=1 ;;
        --help|-h) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
done

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

# Same kernel-truth pattern as test-ha-netem.sh; never infer VIP from /status.
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
dump_state() {
    local node
    printf 'OBSERVED kernel_owner=%s\n' "$(owners 2>/dev/null || printf unreadable)"
    for node in a b; do
        p exec "$NAME-db-$node" ip -o addr show dev eth0 || true
        gx "$node" curl --max-time 2 -fsS http://127.0.0.1:9376/status || true
        sql "$node" 'SELECT * FROM lab.writes ORDER BY id DESC LIMIT 5; SHOW SLAVE STATUS\G' || true
        p logs --tail=12 "$NAME-gz-$node" || true
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
    printf 'FAIL %s kernel_owner=%s row=%s exit=%s\n' "$SCENARIO" \
        "$(owners 2>/dev/null || printf unreadable)" "${ROW:-none}" "$code" >&2
    dump_state >&2
    exit "$code"
}
cleanup() {
    local code=$? node failed=0
    trap - EXIT
    if [[ "$OWN_RESOURCES" == 1 ]]; then
        for node in a b; do
            if p pod exists "$NAME-$node"; then p pod rm -f "$NAME-$node" || failed=1; fi
            if p volume exists "$NAME-data-$node"; then p volume rm -f "$NAME-data-$node" || failed=1; fi
        done
        if p network exists "$NETWORK"; then p network rm "$NETWORK" || failed=1; fi
        if p image exists "$IMAGE"; then p rmi "$IMAGE" || failed=1; fi
        if p image exists "$DB_LAB_IMAGE"; then p rmi "$DB_LAB_IMAGE" || failed=1; fi
        for node in a b; do
            if p pod exists "$NAME-$node" || p volume exists "$NAME-data-$node"; then failed=1; fi
        done
        if p network exists "$NETWORK"; then failed=1; fi
        printf 'CLEANUP pods/containers/volumes/network/image removed=%s\n' "$((1 - failed))"
    fi
    if [[ -n "$WORK" ]]; then rm -rf "$WORK"; fi
    if [[ "$failed" != 0 ]]; then code=1; fi
    exit "$code"
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
    if ! p info >/dev/null 2>&1; then
        PODMAN=(sudo -n podman)
        if ! p info >/dev/null 2>&1; then
            printf 'Prerequisite missing: usable Podman (rootless or passwordless sudo podman)\n' >&2
            return 1
        fi
    fi
    printf 'Prerequisites OK: %s; ip netns\n' "${PODMAN[*]}"
}
prerequisites
if [[ "$CHECK_ONLY" == 1 ]]; then exit 0; fi
if [[ ! -x "$ROOT/target/release/gruezi" ]]; then
    printf 'Prerequisite missing: target/release/gruezi; run cargo build --release --locked\n' >&2
    exit 1
fi

# A known name permits deliberate cleanup after an interrupted previous run.
# Refuse live or stale resources until the caller explicitly supplies --force.
leftovers=0
for node in a b; do
    if p pod exists "$NAME-$node" || p volume exists "$NAME-data-$node"; then leftovers=1; fi
done
if p network exists "$NETWORK" || p image exists "$IMAGE" || p image exists "$DB_LAB_IMAGE"; then leftovers=1; fi
if [[ "$leftovers" == 1 && "$FORCE" == 0 ]]; then
    printf 'Lab resources already exist; rerun with --force to destroy only %s resources\n' "$NAME" >&2
    exit 1
fi
OWN_RESOURCES=1
trap cleanup EXIT
trap fail ERR
trap 'exit 130' INT
trap 'exit 143' TERM
if [[ "$FORCE" == 1 ]]; then
    for node in a b; do
        if p pod exists "$NAME-$node"; then p pod rm -f "$NAME-$node"; fi
        if p volume exists "$NAME-data-$node"; then p volume rm -f "$NAME-data-$node"; fi
    done
    if p network exists "$NETWORK"; then p network rm "$NETWORK"; fi
fi
WORK="$(mktemp -d "${TMPDIR:-/tmp}/gruezi-mariadb.XXXXXXXX")"
chmod 700 "$WORK"
KEY="$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')"
PASSWORD="$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')"
# All generated files use a unique temporary file + rename before container use.
atomic() {
    local destination="$1" temporary
    temporary="$(mktemp "$WORK/.atomic.XXXXXXXX")"
    cat >"$temporary"
    chmod 600 "$temporary"
    mv "$temporary" "$destination"
}
printf '[client]\nuser=root\npassword=%s\n' "$PASSWORD" | atomic "$WORK/db-client.cnf"
printf 'MARIADB_ROOT_PASSWORD=%s\n' "$PASSWORD" | atomic "$WORK/db.env"

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
long_p build -f "$HERE/Containerfile" -t "$IMAGE" "$ROOT"
long_p pull "$DB_IMAGE"
# The DB image needs ip for independent kernel assertions even when Gruezi died.
# Install it in a derived image, with no mutation of the upstream Containerfile.
printf 'FROM %s\nUSER root\nRUN apt-get update && apt-get install -y --no-install-recommends iproute2 && rm -rf /var/lib/apt/lists/*\n' "$DB_IMAGE" \
    | atomic "$WORK/Containerfile.db"
long_p build -f "$WORK/Containerfile.db" -t "$DB_LAB_IMAGE" "$WORK"
p network create --subnet 10.249.15.0/24 "$NETWORK"
start_pod() {
    local node="$1" id=1
    if [[ "$node" == b ]]; then id=2; fi
    p pod create --name "$NAME-$node" --network "$NETWORK" --ip "$(ip_for "$node")"
    p run -d --name "$NAME-db-$node" --pod "$NAME-$node" \
        --env-file "$WORK/db.env" -v "$WORK:/lab:ro,z" \
        -v "$NAME-data-$node:/var/lib/mysql" "$DB_LAB_IMAGE" \
        --server-id="$id" --log-bin=mariadb-bin --log-slave-updates \
        --binlog-format=ROW --gtid-domain-id="$id" \
        --auto-increment-increment=2 --auto-increment-offset="$id" \
        --bind-address=0.0.0.0
}
ready() { sql "$1" 'SELECT 1' >/dev/null 2>&1; }
start_gruezi() {
    local node="$1"
    write_config "$node"
    p run -d --name "$NAME-gz-$node" --pod "$NAME-$node" \
        --cap-add NET_ADMIN --cap-add NET_RAW -v "$WORK:/lab:ro,z" \
        "$IMAGE" start --config "/lab/$node.yaml"
}
for node in a b; do
    p volume create "$NAME-data-$node"
    start_pod "$node"
    wait_for "MariaDB $node ready" ready "$node"
    # Bootstrap independently with logging disabled, then start from each
    # master's observed binlog position. Never replicate duplicate CREATEs.
    sql "$node" "SET SESSION sql_log_bin=0;
CREATE USER 'repl'@'%' IDENTIFIED BY '$PASSWORD';
GRANT REPLICATION SLAVE ON *.* TO 'repl'@'%';
CREATE USER 'writer'@'%' IDENTIFIED BY '$PASSWORD';
CREATE DATABASE lab;
CREATE TABLE lab.writes (id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
 token VARCHAR(64) UNIQUE NOT NULL, origin INT NOT NULL);
GRANT SELECT,INSERT ON lab.* TO 'writer'@'%';"
done
for node in a b; do
    peer="$(other "$node")"
    master="$(sql "$peer" 'SHOW MASTER STATUS')"
    read -r file position rest <<<"$master"
    sql "$node" "CHANGE MASTER TO MASTER_HOST='$(ip_for "$peer")',
MASTER_USER='repl', MASTER_PASSWORD='$PASSWORD', MASTER_LOG_FILE='$file',
MASTER_LOG_POS=$position, MASTER_USE_GTID=no; START SLAVE;"
done
for node in a b; do start_gruezi "$node"; done

peer_healthy() {
    local node json
    for node in a b; do
        json="$(gx "$node" curl -fsS --max-time 2 http://127.0.0.1:9376/status)" || return 1
        printf '%s' "$json" | grep -Eq '"reachable"[[:space:]]*:[[:space:]]*true' || return 1
    done
}
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
    printf 'PASS %s kernel_owner=%s row_a=%s row_b=%s %s\n' "$SCENARIO" \
        "$(owners)" "$(row_on a 2>/dev/null || printf unavailable)" \
        "$(row_on b 2>/dev/null || printf unavailable)" "${1:-}"
}
write_and_replicate() {
    wait_for "exactly one kernel VIP" single_owner
    new_row "$SCENARIO"
    wait_for "VIP writer reaches owner $OWNER" writer_loop "$OWNER"
    wait_for "row replicates" replicated
}

SCENARIO=normal
wait_for "HA bilateral session" peer_healthy
write_and_replicate
pass "VIP writes originate on the kernel owner and replicate"

SCENARIO=unfenced-direct-write
non_owner="$(other "$OWNER")"
new_row unfenced
gx "$non_owner" env MYSQL_PWD="$PASSWORD" mariadb --protocol=tcp \
    --host="$(ip_for "$non_owner")" --user=writer --connect-timeout=1 \
    --execute="INSERT INTO lab.writes(token,origin) VALUES('$ROW',@@server_id)"
wait_for "direct write replicates" replicated
pass "LIMITATION: direct non-owner=$non_owner accepts writes; VIP removal is not write fencing"

SCENARIO=graceful-failover
old="$OWNER"
new="$(other "$old")"
p kill --signal TERM "$NAME-gz-$old"
wait_for "SIGTERM releases VIP to $new" owner_is "$new"
new_row graceful
wait_for "writer continues on successor" writer_loop "$new"
wait_for "successor row replicates" replicated
pass "SIGTERM handoff"
# Observe termination before restarting; podman start must not race shutdown.
stopped() { [[ "$(p inspect --format '{{.State.Running}}' "$NAME-gz-$1")" == false ]]; }
wait_for "old Gruezi exits" stopped "$old"
p start "$NAME-gz-$old"
wait_for "HA restored" peer_healthy

SCENARIO=active-crash
old="$new"; new="$(other "$old")"
# Model a HOST crash, not a Gruezi-only crash: SIGKILL all pod processes and
# destroy its network namespace. Volumes survive. Killing Gruezi alone leaves
# a residual VIP and does not prove exclusive ownership under available.
p pod kill --signal KILL "$NAME-$old"
p pod rm -f "$NAME-$old"
wait_for "survivor kernel owns VIP after host SIGKILL" owner_is "$new"
new_row crash
wait_for "writer continues after crash" writer_loop "$new"
pass "host crash; crashed DB unavailable; namespace destroyed explicitly"
start_pod "$old"
wait_for "crashed DB restarts" ready "$old"
start_gruezi "$old"
wait_for "crash row replicates after restart" replicated
wait_for "HA recovers" peer_healthy

SCENARIO=partition
for node in a b; do
    gx "$node" iptables -I OUTPUT -p udp --dport 9375 -d "$(ip_for "$(other "$node")")" -j DROP
done
wait_for "available policy exposes dual kernel ownership" owner_is ab
new_row partition
# Direct inserts avoid an ambiguous VIP route while both namespaces own it.
sql a "INSERT INTO lab.writes(token,origin) VALUES('$ROW',@@server_id)"
wait_for "DB link remains healthy during HA partition" replicated
pass "EXPECTED UNSAFE OVERLAP: available policy permits two owners"
for node in a b; do
    gx "$node" iptables -D OUTPUT -p udp --dport 9375 -d "$(ip_for "$(other "$node")")" -j DROP
done
wait_for "HA partition healed" peer_healthy
wait_for "single kernel owner after heal" single_owner

SCENARIO=replication-link-loss
before="$OWNER"
# STOP SLAVE severs both replication streams while preserving HA UDP and SQL.
# Unlike killing a server, this isolates replication health from HA health.
for node in a b; do sql "$node" 'STOP SLAVE'; done
new_row replication-loss
wait_for "writer reaches unchanged VIP" writer_loop "$before"
[[ "$(owners)" == "$before" ]]
[[ -z "$(row_on "$(other "$before")")" ]]
peer_healthy
pass "HA healthy; VIP unchanged; peer lacks row: Gruezi does not track replication"

SCENARIO=recovery
for node in a b; do sql "$node" 'START SLAVE'; done
wait_for "all row sets converge after replication heal" converged
wait_for "single VIP after recovery" single_owner
pass "complete ordered row sets equal after heal"

if [[ "$WITH_HOOK" == 1 ]]; then
    SCENARIO=read-only-hook
    # Seed read_only before initial standby; no assumption that demote ran
    # before a new node ever owned the VIP.
    for node in a b; do
        p kill --signal TERM "$NAME-gz-$node"
    done
    for node in a b; do
        wait_for "Gruezi stopped for hook configuration" stopped "$node"
        p rm "$NAME-gz-$node"
        sql "$node" 'SET GLOBAL read_only=1'
    done
    HOOK_PHASE=1
    for node in a b; do start_gruezi "$node"; done
    hook_state() {
        single_owner || return 1
        [[ "$(sql "$OWNER" 'SELECT @@global.read_only')" == 0 &&
           "$(sql "$(other "$OWNER")" 'SELECT @@global.read_only')" == 1 ]]
    }
    wait_for "hook makes standby read-only and owner writable" hook_state
    new_row hook-rejected
    non_owner="$(other "$OWNER")"
    error=""
    if error="$(gx "$non_owner" env MYSQL_PWD="$PASSWORD" mariadb --protocol=tcp \
        --host="$(ip_for "$non_owner")" --user=writer --connect-timeout=1 \
        --execute="INSERT INTO lab.writes(token,origin) VALUES('$ROW',@@server_id)" 2>&1)"; then
        printf 'Non-owner unexpectedly accepted a fenced write\n' >&2; false
    fi
    # Connection failures are not successful write fencing.
    printf '%s' "$error" | grep -q 'ERROR 1290'
    [[ -z "$(row_on a)" && -z "$(row_on b)" ]]
    write_and_replicate
    pass "read_only owner=0 standby=1; unprivileged non-owner write rejected ERROR 1290"
    SCENARIO=read-only-hook-handoff
    old="$OWNER"; new="$(other "$old")"
    p kill --signal TERM "$NAME-gz-$old"
    wait_for "hook flips read_only after demote/promote" hook_state
    [[ "$OWNER" == "$new" ]]
    write_and_replicate
    pass "demoted read_only=1; promoted read_only=0"
fi
