#!/usr/bin/env bash
set -euo pipefail

# Linux namespace and tc netem lab for HA behavior validation.
#
# This is intentionally separate from cargo tests because it requires root,
# network namespaces, traffic shaping, and host-level process control.

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
GRUEZI_BIN="${GRUEZI_BIN:-$REPO_ROOT/target/release/gruezi}"
LAB_ROOT="${LAB_ROOT:-/tmp/gruezi-netem-lab}"

NS_A="gzha-a"
NS_B="gzha-b"
IF_A="gza0"
IF_B="gzb0"
IP_A="10.250.0.1"
IP_B="10.250.0.2"
VIP_CIDR="10.250.0.100/24"
GROUP_ID="netem-ha"
SHARED_KEY="netem-shared-secret"
PRIORITY_A="110"
PRIORITY_B="100"
ADVERT_INTERVAL_MS="200"
DEAD_FACTOR="3"
HOLD_DOWN_MS="200"
JITTER_MS="0"

SCENARIO=""
SCENARIO_DIR=""

usage() {
    cat <<'EOF'
Usage:
  sudo ./scripts/test-ha-netem.sh all
  sudo ./scripts/test-ha-netem.sh baseline
  sudo ./scripts/test-ha-netem.sh delay-jitter
  sudo ./scripts/test-ha-netem.sh one-way-loss
  sudo ./scripts/test-ha-netem.sh full-partition
  sudo ./scripts/test-ha-netem.sh heal-after-partition
  sudo ./scripts/test-ha-netem.sh cleanup

What it does:
  - creates two Linux network namespaces
  - starts one gruezi node per namespace
  - injects tc netem delay or loss on the HA link
  - validates the resulting Active/Standby behavior

Environment:
  GRUEZI_BIN=/abs/path/to/gruezi   override the binary path
  LAB_ROOT=/tmp/gruezi-netem-lab   override the lab work directory

Notes:
  - build the release binary first: cargo build --release --locked
  - this script requires root and Linux iproute2 tools
  - logs and generated configs are kept under $LAB_ROOT
EOF
}

require_root() {
    if [[ "$(id -u)" -ne 0 ]]; then
        echo "This script must run as root." >&2
        exit 1
    fi
}

require_commands() {
    local command
    for command in ip tc curl grep sed tail; do
        if ! command -v "$command" >/dev/null 2>&1; then
            echo "Missing required command: $command" >&2
            exit 1
        fi
    done
}

require_binary() {
    if [[ ! -x "$GRUEZI_BIN" ]]; then
        cat >&2 <<EOF
gruezi binary not found or not executable:
  $GRUEZI_BIN

Build it first:
  cargo build --release --locked
EOF
        exit 1
    fi
}

log() {
    printf '[%s] %s\n' "$(date '+%H:%M:%S')" "$*"
}

ns_exec() {
    ip netns exec "$1" "${@:2}"
}

json_string_field() {
    local json="$1"
    local field="$2"

    printf '%s\n' "$json" \
        | grep -o "\"$field\":\"[^\"]*\"" \
        | head -n1 \
        | sed -e "s/^\"$field\":\"//" -e 's/"$//'
}

json_optional_string_field() {
    local json="$1"
    local field="$2"

    if printf '%s\n' "$json" | grep -q "\"$field\":null"; then
        printf 'null\n'
        return 0
    fi

    json_string_field "$json" "$field"
}

json_bool_field() {
    local json="$1"
    local field="$2"

    printf '%s\n' "$json" \
        | grep -o "\"$field\":\(true\|false\)" \
        | head -n1 \
        | cut -d: -f2
}

status_json() {
    ns_exec "$1" curl -fsS --max-time 1 http://127.0.0.1:9376/status
}

status_summary() {
    local namespace="$1"
    local json state peer_alive decision_reason transition_reason

    json="$(status_json "$namespace")"
    state="$(json_string_field "$json" state)"
    peer_alive="$(json_bool_field "$json" peer_alive)"
    decision_reason="$(json_string_field "$json" decision_reason)"
    transition_reason="$(json_optional_string_field "$json" last_transition_reason)"

    printf '%s state=%s peer_alive=%s decision_reason=%s last_transition_reason=%s\n' \
        "$namespace" "$state" "$peer_alive" "$decision_reason" "$transition_reason"
}

show_statuses() {
    log "status: $(status_summary "$NS_A" 2>/dev/null || echo "$NS_A unavailable")"
    log "status: $(status_summary "$NS_B" 2>/dev/null || echo "$NS_B unavailable")"
}

show_logs() {
    local log_file

    for log_file in "$LAB_ROOT"/*/node-a.log "$LAB_ROOT"/*/node-b.log; do
        if [[ -f "$log_file" ]]; then
            log "tail of $log_file"
            tail -n 20 "$log_file" || true
        fi
    done
}

cleanup_namespaces() {
    ip netns del "$NS_A" >/dev/null 2>&1 || true
    ip netns del "$NS_B" >/dev/null 2>&1 || true
}

stop_cluster() {
    local pid_file pid

    if [[ -z "$SCENARIO_DIR" ]]; then
        return 0
    fi

    for pid_file in "$SCENARIO_DIR"/node-a.pid "$SCENARIO_DIR"/node-b.pid; do
        if [[ -f "$pid_file" ]]; then
            pid="$(cat "$pid_file")"
            kill "$pid" >/dev/null 2>&1 || true
            wait "$pid" >/dev/null 2>&1 || true
        fi
    done
}

cleanup_all() {
    stop_cluster
    cleanup_namespaces
}

on_error() {
    log "scenario '$SCENARIO' failed"
    show_statuses || true
    show_logs || true
}

trap on_error ERR
trap cleanup_all EXIT

write_config() {
    local path="$1"
    local node_id="$2"
    local bind_ip="$3"
    local peer_ip="$4"
    local interface="$5"
    local priority="$6"

    cat >"$path" <<EOF
mode: ha
node:
  id: $node_id
ha:
  bind: $bind_ip:9375
  interface: $interface
  addresses:
    - $VIP_CIDR
  peer: $peer_ip:9375
  group_id: $GROUP_ID
  protocol_version: 1
  priority: $priority
  preempt: false
  advert_interval_ms: $ADVERT_INTERVAL_MS
  dead_factor: $DEAD_FACTOR
  hold_down_ms: $HOLD_DOWN_MS
  jitter_ms: $JITTER_MS
  auth:
    mode: shared_key
    key: $SHARED_KEY
EOF
}

start_node() {
    local namespace="$1"
    local config_path="$2"
    local log_path="$3"
    local pid_path="$4"

    ip netns exec "$namespace" \
        "$GRUEZI_BIN" -v start --config "$config_path" \
        >"$log_path" 2>&1 &
    echo "$!" >"$pid_path"
}

wait_for_api() {
    local namespace="$1"
    local attempt

    for attempt in $(seq 1 60); do
        if status_json "$namespace" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.1
    done

    log "timed out waiting for API in $namespace"
    return 1
}

wait_for_state() {
    local namespace="$1"
    local expected_state="$2"
    local expected_peer_alive="$3"
    local timeout_seconds="${4:-6}"
    local attempt_count
    local json state peer_alive

    attempt_count=$((timeout_seconds * 10))
    while (( attempt_count > 0 )); do
        if json="$(status_json "$namespace" 2>/dev/null)"; then
            state="$(json_string_field "$json" state)"
            peer_alive="$(json_bool_field "$json" peer_alive)"

            if [[ "$state" == "$expected_state" && "$peer_alive" == "$expected_peer_alive" ]]; then
                return 0
            fi
        fi

        attempt_count=$((attempt_count - 1))
        sleep 0.1
    done

    log "timed out waiting for $namespace state=$expected_state peer_alive=$expected_peer_alive"
    show_statuses
    return 1
}

assert_state() {
    local namespace="$1"
    local expected_state="$2"
    local expected_peer_alive="$3"
    local json state peer_alive

    json="$(status_json "$namespace")"
    state="$(json_string_field "$json" state)"
    peer_alive="$(json_bool_field "$json" peer_alive)"

    if [[ "$state" != "$expected_state" || "$peer_alive" != "$expected_peer_alive" ]]; then
        log "unexpected state for $namespace"
        log "expected: state=$expected_state peer_alive=$expected_peer_alive"
        log "actual:   state=$state peer_alive=$peer_alive"
        show_statuses
        return 1
    fi
}

setup_cluster() {
    SCENARIO="$1"
    SCENARIO_DIR="$LAB_ROOT/$SCENARIO"

    cleanup_all

    rm -rf "$SCENARIO_DIR"
    mkdir -p "$SCENARIO_DIR"

    log "creating namespaces for scenario '$SCENARIO'"
    ip netns add "$NS_A"
    ip netns add "$NS_B"

    ip link add "$IF_A" type veth peer name "$IF_B"
    ip link set "$IF_A" netns "$NS_A"
    ip link set "$IF_B" netns "$NS_B"

    ns_exec "$NS_A" ip link set lo up
    ns_exec "$NS_B" ip link set lo up
    ns_exec "$NS_A" ip link set "$IF_A" up
    ns_exec "$NS_B" ip link set "$IF_B" up
    ns_exec "$NS_A" ip address add "$IP_A/24" dev "$IF_A"
    ns_exec "$NS_B" ip address add "$IP_B/24" dev "$IF_B"

    write_config "$SCENARIO_DIR/node-a.yaml" "node-a" "$IP_A" "$IP_B" "$IF_A" "$PRIORITY_A"
    write_config "$SCENARIO_DIR/node-b.yaml" "node-b" "$IP_B" "$IP_A" "$IF_B" "$PRIORITY_B"

    log "starting gruezi nodes"
    start_node "$NS_A" "$SCENARIO_DIR/node-a.yaml" "$SCENARIO_DIR/node-a.log" "$SCENARIO_DIR/node-a.pid"
    start_node "$NS_B" "$SCENARIO_DIR/node-b.yaml" "$SCENARIO_DIR/node-b.log" "$SCENARIO_DIR/node-b.pid"

    wait_for_api "$NS_A"
    wait_for_api "$NS_B"
    wait_for_state "$NS_A" "Active" "true"
    wait_for_state "$NS_B" "Standby" "true"
    show_statuses
}

apply_full_partition() {
    log "applying full packet loss in both directions"
    ns_exec "$NS_A" tc qdisc replace dev "$IF_A" root netem loss 100%
    ns_exec "$NS_B" tc qdisc replace dev "$IF_B" root netem loss 100%
}

apply_one_way_loss() {
    log "dropping node-a to node-b traffic only"
    ns_exec "$NS_A" tc qdisc replace dev "$IF_A" root netem loss 100%
}

apply_delay_jitter() {
    log "adding bounded delay and jitter on both sides"
    ns_exec "$NS_A" tc qdisc replace dev "$IF_A" root netem delay 250ms 50ms
    ns_exec "$NS_B" tc qdisc replace dev "$IF_B" root netem delay 250ms 50ms
}

clear_netem() {
    ns_exec "$NS_A" tc qdisc del dev "$IF_A" root >/dev/null 2>&1 || true
    ns_exec "$NS_B" tc qdisc del dev "$IF_B" root >/dev/null 2>&1 || true
}

scenario_baseline() {
    setup_cluster "baseline"
    assert_state "$NS_A" "Active" "true"
    assert_state "$NS_B" "Standby" "true"
    log "baseline passed: node-a stayed Active and node-b stayed Standby"
}

scenario_delay_jitter() {
    setup_cluster "delay-jitter"
    apply_delay_jitter
    sleep 3
    assert_state "$NS_A" "Active" "true"
    assert_state "$NS_B" "Standby" "true"
    log "delay-jitter passed: bounded delay did not cause a failover"
}

scenario_one_way_loss() {
    setup_cluster "one-way-loss"
    apply_one_way_loss
    wait_for_state "$NS_A" "Standby" "true" 8
    wait_for_state "$NS_B" "Active" "false" 8
    show_statuses
    log "one-way-loss passed: node-b promoted after losing inbound heartbeats"
}

scenario_full_partition() {
    setup_cluster "full-partition"
    apply_full_partition
    wait_for_state "$NS_A" "Active" "false" 8
    wait_for_state "$NS_B" "Active" "false" 8
    show_statuses
    log "full-partition passed: both nodes became Active during total isolation"
}

scenario_heal_after_partition() {
    setup_cluster "heal-after-partition"
    apply_full_partition
    wait_for_state "$NS_A" "Active" "false" 8
    wait_for_state "$NS_B" "Active" "false" 8
    log "healing the partition"
    clear_netem
    wait_for_state "$NS_A" "Active" "true" 8
    wait_for_state "$NS_B" "Standby" "true" 8
    show_statuses
    log "heal-after-partition passed: the pair converged back to a single Active node"
}

run_all() {
    scenario_baseline
    scenario_delay_jitter
    scenario_one_way_loss
    scenario_full_partition
    scenario_heal_after_partition
}

main() {
    local command="${1:-all}"

    case "$command" in
        -h|--help|help)
            usage
            return 0
            ;;
    esac

    require_root
    require_commands
    require_binary
    mkdir -p "$LAB_ROOT"

    case "$command" in
        all)
            run_all
            ;;
        baseline)
            scenario_baseline
            ;;
        delay-jitter)
            scenario_delay_jitter
            ;;
        one-way-loss)
            scenario_one_way_loss
            ;;
        full-partition)
            scenario_full_partition
            ;;
        heal-after-partition)
            scenario_heal_after_partition
            ;;
        cleanup)
            cleanup_all
            ;;
        *)
            usage >&2
            exit 1
            ;;
    esac
}

main "$@"
