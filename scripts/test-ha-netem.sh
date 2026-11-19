#!/usr/bin/env bash
set -euo pipefail

# Destructive, isolated Linux lab. No real watchdog device is opened here.
# A process crash/freeze leaves its namespace and residual VIP alive.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
GRUEZI_BIN="${GRUEZI_BIN:-$REPO_ROOT/target/release/gruezi}"
LAB_ROOT="${LAB_ROOT:-/tmp/gruezi-netem-lab}"
NS_A=gzha-a
NS_B=gzha-b
IF_A=gza0
IF_B=gzb0
IP_A=10.250.0.1
IP_B=10.250.0.2
VIP_CIDR=10.250.0.100/24
SCENARIO_DIR=""
POLICY=""
SCENARIO=""
SHARED_KEY=""
OBSERVED=""
NODE_JSON=""
NODE_PRESENT=""
FROZEN_OBSERVATION=""
HANDOFF_OBSERVATION=""
MODE_A=live
MODE_B=live
SOURCE_A=peer
SOURCE_B=peer

usage() {
    cat <<'EOF'
Usage: sudo ./scripts/test-ha-netem.sh [all|SCENARIO|cleanup] [strict|available|external|all]
Scenarios: startup graceful-failover active-crash standby-crash full-partition
           a-to-b-loss b-to-a-loss process-freeze failed-withdrawal recovery-failback
Environment: GRUEZI_BIN (release binary), LAB_ROOT (logs and generated configs).
Prerequisites: Linux, root, iproute2 (ip/tc), curl, jq, timeout, od, mktemp.
Build first: cargo build --release --locked
All waits poll observations with a 30 second deadline. Watchdog is off.
The external arbiter and fence are throwaway lab scripts, not production fencing.
EOF
}

require_root() {
    if [[ "$(id -u)" -ne 0 ]]; then
        echo "Prerequisite: run as root on a disposable Linux lab VM (iproute2, curl, jq, timeout required)." >&2
        return 1
    fi
}

require_commands() {
    local command
    for command in ip tc curl jq timeout od mktemp awk tr; do
        command -v "$command" >/dev/null 2>&1 || {
            echo "Missing required command: $command" >&2
            return 1
        }
    done
    [[ -x "$GRUEZI_BIN" ]] || {
        echo "Missing executable $GRUEZI_BIN; run cargo build --release --locked" >&2
        return 1
    }
}

ns_exec() {
    timeout 2 ip netns exec "$1" "${@:2}"
}

status_json() {
    ns_exec "$1" curl -fsS --max-time 1 http://127.0.0.1:9376/status
}

vip_present() {
    local interface addresses
    case "$1" in
        "$NS_A") interface="$IF_A" ;;
        "$NS_B") interface="$IF_B" ;;
        *) return 2 ;;
    esac
    addresses="$(ns_exec "$1" ip -o addr show dev "$interface")" || return 2
    printf '%s\n' "$addresses" | awk -v vip="$VIP_CIDR" '
        ($3 == "inet" || $3 == "inet6") && $4 == vip { found = 1 }
        END { exit !found }'
}

# Every observation includes the kernel and all five requested status fields.
# Down/frozen nodes have no current status: never substitute a cached snapshot.
observe_node() {
    local namespace="$1" mode="$2" present=false json fields
    if vip_present "$namespace"; then
        present=true
    else
        [[ "$?" == 1 ]] || { OBSERVED="$OBSERVED $namespace kernel=ERROR"; return 1; }
    fi
    if json="$(status_json "$namespace" 2>/dev/null)"; then
        fields="$(printf '%s' "$json" | jq -er '
            .ha | "state=\(.state) owned=\(.owned) authority.source=\(.authority.source) safety.level=\(.safety.level) resource.state=\(.resource.state)"')" || return 1
        OBSERVED="$OBSERVED $namespace kernel=$present $fields;"
        [[ "$mode" == live ]] || return 1
        NODE_JSON="$json"
    else
        OBSERVED="$OBSERVED $namespace kernel=$present state=unavailable owned=unavailable authority.source=unavailable safety.level=unavailable resource.state=unavailable ($mode);"
        [[ "$mode" != live ]] || return 1
        NODE_JSON=""
    fi
    NODE_PRESENT="$present"
}

# Role is independent of ownership. An elected Active without authority is
# legitimate in strict mode. Residual VIPs are never treated as owned.
assert_state() {
    local present="$1" source="$2" kind="$3"
    [[ -n "$NODE_JSON" ]] || return 0
    printf '%s' "$NODE_JSON" | jq -e \
        --arg source "$source" --arg kind "$kind" --argjson present "$present" '
        .ha |
        (.state == "Active" or .state == "Standby") and
        (if $kind == "fault" then
            .owned == false and
            (.resource.state == "uncertain" or .resource.state == "releasing") and
            .safety.level == "uncertain"
         elif $present then
            .state == "Active" and .owned == true and
            .resource.state == "present" and .authority.source == $source and
            .authority.state == "valid" and
            .safety.level == (if $source == "assumed" then "assumed" else "safe" end)
         else
            .owned == false and .resource.state == "absent" and
            .authority.state != "valid" and
            (.authority.source == null or .authority.source == "peer" or
             .authority.source == "external" or .authority.source == "assumed") and
            .safety.level == (if .authority.source == "assumed" then "assumed"
                             else "safe" end)
         end)' >/dev/null
}

assert_vip_owner() {
    local expected="$1" kind="${2:-normal}" namespace mode source wanted ok=0
    OBSERVED=""
    for namespace in "$NS_A" "$NS_B"; do
        if [[ "$namespace" == "$NS_A" ]]; then mode="$MODE_A"; source="$SOURCE_A"
        else mode="$MODE_B"; source="$SOURCE_B"; fi
        wanted=false
        [[ "$expected" != both && "$expected" != "$namespace" ]] || wanted=true
        if observe_node "$namespace" "$mode"; then
            [[ "$NODE_PRESENT" == "$wanted" ]] || ok=1
            if [[ "$namespace" == "$NS_A" && "$kind" == fault ]]; then
                assert_state "$wanted" "$source" fault || ok=1
            else
                assert_state "$wanted" "$source" normal || ok=1
            fi
        else ok=1; fi
    done
    return "$ok"
}

wait_owner() {
    local expected="$1" kind="${2:-normal}" minimum="${3:-0}"
    local start="$SECONDS"
    while (( SECONDS - start < 30 )); do
        if assert_vip_owner "$expected" "$kind" && (( SECONDS - start >= minimum )); then
            return 0
        fi
        # Poll delay only; success always depends on observed kernel and API.
        sleep 0.1
    done
    echo "Timeout: policy=$POLICY scenario=$SCENARIO expected=$expected $OBSERVED" >&2
    return 1
}

atomic_value() {
    local path="$1" value="$2" temporary
    temporary="$(mktemp "$path.XXXXXX")"
    printf '%s\n' "$value" >"$temporary"
    mv "$temporary" "$path"
}

write_config() {
    local path="${1:?config path required}" node="${2:?node required}"
    local peer="${3:?peer id required}" bind="${4:?bind required}"
    local remote="${5:?peer address required}" interface="${6:?interface required}"
    local priority="${7:?priority required}" preempt="${8:-false}" temporary
    : "${POLICY:?policy required}" "${SHARED_KEY:?throwaway key required}" "${SCENARIO_DIR:?scenario directory required}"
    case "$POLICY" in strict|available|external) ;; *) return 1 ;; esac
    [[ ${#SHARED_KEY} -ge 16 ]] || return 1
    temporary="$(mktemp "$path.XXXXXX")"
    cat >"$temporary" <<EOF
mode: ha
node:
  id: $node
ha:
  failure_policy: $POLICY
  peer_id: $peer
  bind: $bind:9375
  interface: $interface
  addresses: [$VIP_CIDR]
  peer: $remote:9375
  group_id: netem-ha
  priority: $priority
  preempt: $preempt
  advert_interval_ms: 200
  dead_factor: 3
  hold_down_ms: 200
  jitter_ms: 0
  command_timeout_ms: 500
  withdraw_timeout_ms: 1000
  watchdog:
    mode: off
  auth:
    key: $SHARED_KEY
EOF
    if [[ "$POLICY" == external ]]; then
        cat >>"$temporary" <<EOF
  authority:
    check_command: '$SCENARIO_DIR/check-$node'
    fence_command: '$SCENARIO_DIR/fence'
    interval_ms: 200
    grant_ttl_ms: 6000
EOF
    fi
    mv "$temporary" "$path"
}

write_helpers() {
    local temporary name
    mkdir -p "$SCENARIO_DIR/bin"
    for name in ip check-node-a check-node-b fence; do
        temporary="$(mktemp "$SCENARIO_DIR/helper.XXXXXX")"
        {
            printf '#!/usr/bin/env bash\nset -euo pipefail\n'
            printf 'root=%q\nreal_ip=%q\nns_a=%q\nns_b=%q\nif_a=%q\nif_b=%q\nvip=%q\n' \
                "$SCENARIO_DIR" "$(command -v ip)" "$NS_A" "$NS_B" "$IF_A" "$IF_B" "$VIP_CIDR"
            cat <<'EOF'
name="${0##*/}"
case "$name" in
    ip)
        if [[ "${1:-}" == address && "${2:-}" == del && -f "$root/fail-del" ]]; then
            echo "Injected VIP withdrawal failure" >&2
            exit 1
        fi
        exec "$real_ip" "$@"
        ;;
    check-*)
        read -r grant < "$root/grant"
        [[ "$grant" == "${name#check-}" ]]
        ;;
    fence)
        # This is real lab fencing: kill the predecessor and remove its VIP
        # through the host namespace. A nonzero result never authorizes B.
        [[ -f "$root/fence-allowed" ]] || exit 1
        case "${1:-}" in
            node-a) namespace="$ns_a"; interface="$if_a"; node=node-a ;;
            node-b) namespace="$ns_b"; interface="$if_b"; node=node-b ;;
            *) exit 2 ;;
        esac
        read -r pid < "$root/$node.pid"
        kill -KILL "$pid" 2>/dev/null || true
        timeout 2 "$real_ip" -n "$namespace" address del "$vip" dev "$interface" || {
            ! timeout 2 "$real_ip" -n "$namespace" -o address show dev "$interface" |
                awk -v vip="$vip" '$4 == vip { found=1 } END { exit !found }'
        }
        ;;
esac
EOF
        } >"$temporary"
        chmod 700 "$temporary"
        if [[ "$name" == ip ]]; then mv "$temporary" "$SCENARIO_DIR/bin/ip"
        else mv "$temporary" "$SCENARIO_DIR/$name"; fi
    done
}

start_node() {
    local side="$1" namespace config
    if [[ "$side" == a ]]; then namespace="$NS_A"; else namespace="$NS_B"; fi
    config="$SCENARIO_DIR/node-$side.yaml"
    ip netns exec "$namespace" env PATH="$SCENARIO_DIR/bin:$PATH" \
        "$GRUEZI_BIN" start --config "$config" >"$SCENARIO_DIR/node-$side.log" 2>&1 &
    atomic_value "$SCENARIO_DIR/node-$side.pid" "$!"
}

signal_node() {
    local pid
    read -r pid <"$SCENARIO_DIR/node-$1.pid"
    kill "-$2" "$pid"
}

cleanup_all() {
    local side pid start
    if [[ -n "$SCENARIO_DIR" ]]; then
        rm -f "$SCENARIO_DIR/fail-del"
        for side in a b; do
            [[ -f "$SCENARIO_DIR/node-$side.pid" ]] || continue
            read -r pid <"$SCENARIO_DIR/node-$side.pid"
            kill -CONT "$pid" 2>/dev/null || true
            kill -TERM "$pid" 2>/dev/null || true
            start="$SECONDS"
            while kill -0 "$pid" 2>/dev/null && (( SECONDS - start < 5 )); do sleep 0.1; done
            kill -KILL "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        done
    fi
    timeout 2 ip netns del "$NS_A" 2>/dev/null || true
    timeout 2 ip netns del "$NS_B" 2>/dev/null || true
}

setup_cluster() {
    MODE_A=live; MODE_B=live; SOURCE_A=peer; SOURCE_B=peer
    [[ "$POLICY" != external ]] || { SOURCE_A=external; SOURCE_B=external; }
    SCENARIO_DIR="$(mktemp -d "$LAB_ROOT/$POLICY-$SCENARIO.XXXXXX")"
    timeout 2 ip netns add "$NS_A"
    timeout 2 ip netns add "$NS_B"
    timeout 2 ip link add "$IF_A" type veth peer name "$IF_B"
    timeout 2 ip link set "$IF_A" netns "$NS_A"
    timeout 2 ip link set "$IF_B" netns "$NS_B"
    ns_exec "$NS_A" ip link set lo up
    ns_exec "$NS_B" ip link set lo up
    ns_exec "$NS_A" ip link set "$IF_A" up
    ns_exec "$NS_B" ip link set "$IF_B" up
    ns_exec "$NS_A" ip address add "$IP_A/24" dev "$IF_A"
    ns_exec "$NS_B" ip address add "$IP_B/24" dev "$IF_B"
    atomic_value "$SCENARIO_DIR/grant" node-a
    write_helpers
    write_config "$SCENARIO_DIR/node-a.yaml" node-a node-b "$IP_A" "$IP_B" "$IF_A" 110
    write_config "$SCENARIO_DIR/node-b.yaml" node-b node-a "$IP_B" "$IP_A" "$IF_B" 100
    start_node a
    start_node b
    wait_owner "$NS_A"
}

loss() {
    case "$1" in
        a) ns_exec "$NS_A" tc qdisc replace dev "$IF_A" root netem loss 100% ;;
        b) ns_exec "$NS_B" tc qdisc replace dev "$IF_B" root netem loss 100% ;;
    esac
}

partition_expectation() {
    case "$POLICY" in
        strict) wait_owner none normal 2 ;;
        available)
            SOURCE_A=assumed; SOURCE_B=assumed
            case "$SCENARIO" in
                a-to-b-loss) wait_owner "$NS_B" normal 2 ;;
                b-to-a-loss) wait_owner "$NS_A" normal 2 ;;
                *) wait_owner both normal 2 ;;
            esac ;;
        external) wait_owner "$NS_A" normal 2 ;;
    esac
}

run_scenario() {
    setup_cluster
    case "$SCENARIO" in
        startup) ;;
        graceful-failover)
            signal_node a TERM; MODE_A=down
            if [[ "$POLICY" == external ]]; then atomic_value "$SCENARIO_DIR/grant" node-b; fi
            # First prove the cooperative release produced a successor.
            # Strict consent subsequently lapses when its peer has exited.
            case "$POLICY" in
                strict)
                    wait_owner "$NS_B"
                    HANDOFF_OBSERVATION="$OBSERVED"
                    wait_owner none normal 2
                    OBSERVED="handoff: $HANDOFF_OBSERVATION after consent expiry: $OBSERVED" ;;
                available) SOURCE_B=assumed; wait_owner "$NS_B" normal 2 ;;
                external) wait_owner "$NS_B" normal 2 ;;
            esac ;;
        active-crash)
            signal_node a KILL; MODE_A=down
            case "$POLICY" in
                strict) wait_owner "$NS_A" normal 2 ;;
                available) SOURCE_B=assumed; wait_owner both normal 2 ;;
                external)
                    atomic_value "$SCENARIO_DIR/grant" node-b
                    # An exclusive grant without fencing must not acquire.
                    wait_owner "$NS_A" normal 2
                    atomic_value "$SCENARIO_DIR/fence-allowed" yes
                    wait_owner "$NS_B" normal 2 ;;
            esac ;;
        standby-crash)
            signal_node b KILL; MODE_B=down
            case "$POLICY" in
                strict) wait_owner none normal 2 ;;
                available) SOURCE_A=assumed; wait_owner "$NS_A" normal 2 ;;
                external) wait_owner "$NS_A" normal 2 ;;
            esac ;;
        full-partition|a-to-b-loss|b-to-a-loss|recovery-failback)
            case "$SCENARIO" in
                a-to-b-loss) loss a ;;
                b-to-a-loss) loss b ;;
                *) loss a; loss b ;;
            esac
            partition_expectation
            if [[ "$SCENARIO" == recovery-failback ]]; then
                ns_exec "$NS_A" tc qdisc del dev "$IF_A" root
                # Match the runtime matrix's directional recovery: restore
                # A->B first and wait for B to yield before restoring B->A.
                if [[ "$POLICY" == available ]]; then wait_owner "$NS_A" normal 2; fi
                ns_exec "$NS_B" tc qdisc del dev "$IF_B" root
                if [[ "$POLICY" != external ]]; then SOURCE_A=peer; SOURCE_B=peer; fi
                wait_owner "$NS_A"
            fi ;;
        process-freeze)
            signal_node a STOP; MODE_A=frozen
            case "$POLICY" in
                available) SOURCE_B=assumed; wait_owner both normal 2 ;;
                *) wait_owner "$NS_A" normal 2 ;;
            esac
            FROZEN_OBSERVATION="$OBSERVED"
            signal_node a CONT; MODE_A=live
            # Recovery after SIGCONT must again agree with both kernels.
            if [[ "$POLICY" == available ]]; then
                SOURCE_A=peer; SOURCE_B=peer
                wait_owner "$NS_B"
            else wait_owner "$NS_A"; fi
            OBSERVED="frozen: $FROZEN_OBSERVATION resumed: $OBSERVED" ;;
        failed-withdrawal)
            atomic_value "$SCENARIO_DIR/fail-del" yes
            if [[ "$POLICY" == external ]]; then
                # Revoking A's grant forces withdrawal. B's fresh grant is
                # insufficient because fencing is deliberately unavailable.
                atomic_value "$SCENARIO_DIR/grant" node-b
            else
                # A live cooperative handoff exposes the failed resource
                # status, unlike SIGTERM which also stops the status API.
                # Restart B with a higher rank and preemption enabled.
                # Stop B first. Available keeps A's VIP with no peer, so wait
                # for that observed state before forcing the handoff.
                signal_node b TERM; MODE_B=down
                SOURCE_A=assumed; wait_owner "$NS_A" normal 2
                write_config "$SCENARIO_DIR/node-b.yaml" node-b node-a "$IP_B" "$IP_A" "$IF_B" 120 true
                start_node b; MODE_B=live
            fi
            wait_owner "$NS_A" fault 2 ;;
    esac
}

main() {
    local command="${1:-all}" selected="${2:-all}" policy scenario pid result=0
    local scenarios="startup graceful-failover active-crash standby-crash full-partition a-to-b-loss b-to-a-loss process-freeze failed-withdrawal recovery-failback"
    case "$command" in -h|--help|help) usage; return 0 ;; esac
    require_root || return 1
    require_commands || return 1
    case "$selected" in strict|available|external|all) ;; *) usage >&2; return 1 ;; esac
    if [[ "$command" == cleanup ]]; then cleanup_all; return; fi
    case " $scenarios " in *" $command "*) scenarios="$command" ;; *)
        [[ "$command" == all ]] || { usage >&2; return 1; } ;;
    esac
    [[ "$LAB_ROOT" == /* && "$LAB_ROOT" != *"'"* ]] || {
        echo "LAB_ROOT must be absolute and contain no single quote" >&2; return 1;
    }
    mkdir -p "$LAB_ROOT"
    SHARED_KEY="$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')"
    trap cleanup_all EXIT
    trap 'exit 130' INT TERM
    for policy in strict available external; do
        [[ "$selected" == all || "$selected" == "$policy" ]] || continue
        for scenario in $scenarios; do
            POLICY="$policy"; SCENARIO="$scenario"
            # Run with errexit active; testing a function in `if` would disable
            # errexit throughout it and could turn failed setup into a PASS.
            (
                trap 'rc=$?; if [[ "$rc" != 0 ]]; then printf "FAIL policy=%s scenario=%s %s logs=%s\n" "$POLICY" "$SCENARIO" "$OBSERVED" "$SCENARIO_DIR" >&2; fi; cleanup_all' EXIT
                set -e
                run_scenario
                printf 'PASS policy=%s scenario=%s %s logs=%s\n' "$POLICY" "$SCENARIO" "$OBSERVED" "$SCENARIO_DIR"
            ) &
            pid=$!
            if wait "$pid"; then :; else result=1; fi
        done
    done
    return "$result"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then main "$@"; fi
