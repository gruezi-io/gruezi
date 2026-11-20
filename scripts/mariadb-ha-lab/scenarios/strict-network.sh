#!/usr/bin/env bash

# Every network run subscribes before either daemon exists. Kernel netlink
# subscription readiness is checked by socket inode, rather than a fixed delay.
# Raw ip -ts timestamps and the adapter's realtime_ns use the same host clock.
lab_before_gruezi_start() {
    local node="$1" token=""
    case "$SCENARIO_FILTER" in
        strict-startup|strict-partition-bilateral|strict-asymmetric-loss|strict-gateway-reachable-peer-isolated|strict-firewall-blocks-ha) ;;
        *) return 0 ;;
    esac
    if [[ "${NETWORK_OBSERVERS_ARMED:-0}" != 1 ]]; then
        NETWORK_OBSERVERS_ARMED=1
        NETWORK_EVIDENCE="$REPORT_DIR/network"
        mkdir -p "$NETWORK_EVIDENCE"
        if [[ "$SCENARIO_FILTER" == strict-startup ]]; then
            NETWORK_START_TOKEN="startup-$RUN_ID"
            token="$NETWORK_START_TOKEN"
        fi
        cat <<'OBSERVER' | atomic "$WORK/network-observer.sh"
#!/usr/bin/env bash
set -euo pipefail
vip="$1" token="$2"
ip_pid="" client_pid=""
inode=""
printf '%s\n' "$$" >/tmp/network-observer.pid
stop() {
    trap - TERM INT EXIT
    if [[ -n "$inode" ]]; then
        awk -v inode="$inode" '$NF==inode {print "netlink_drops=" $(NF-1)}' /proc/net/netlink >>/tmp/network-meta
    fi
    if [[ -n "$client_pid" ]]; then kill "$client_pid" 2>/dev/null || true; wait "$client_pid" 2>/dev/null || true; fi
    if [[ -n "$ip_pid" ]]; then kill "$ip_pid" 2>/dev/null || true; wait "$ip_pid" 2>/dev/null || true; fi
    printf 'end_ns=%s\n' "$(date +%s%N)" >>/tmp/network-meta
    printf '0\n' >/tmp/network-observer.exit
    exit 0
}
trap stop TERM INT
printf 'begin_ns=%s\n' "$(date +%s%N)" >/tmp/network-meta
printf 'clock=CLOCK_REALTIME time_namespace=%s\n' "$(readlink /proc/self/ns/time)" >>/tmp/network-meta
ip -o address show dev eth0 >/tmp/network-initial
TZ=UTC ip -ts monitor address dev eth0 >/tmp/network-ip.raw 2>/tmp/network-ip.stderr &
ip_pid=$!
deadline=$((SECONDS + 5))
subscribed=0
while ((SECONDS < deadline)); do
    kill -0 "$ip_pid"
    for fd in /proc/"$ip_pid"/fd/*; do
        link="$(readlink "$fd" 2>/dev/null || true)"
        case "$link" in
            socket:\[*\])
                inode="${link#socket:\[}"; inode="${inode%\]}"
                if awk -v inode="$inode" '$NF==inode && $4!="00000000" {ok=1} END {exit !ok}' /proc/net/netlink; then subscribed=1; break; fi ;;
        esac
    done
    [[ "$subscribed" == 0 ]] || break
    sleep 0.01
done
[[ "$subscribed" == 1 ]]
printf 'ready_ns=%s\n' "$(date +%s%N)" >>/tmp/network-meta
printf 'ready\n' >/tmp/network-observer.ready
if [[ -n "$token" ]]; then
    (
        deadline=$((SECONDS + 60))
        while ((SECONDS < deadline)); do
            if awk -v vip="$vip/24" 'index($0,vip) && !/Deleted/ {found=1} END {exit !found}' /tmp/network-ip.raw; then
                begin="$(date +%s%N)" code=0
                origin="$(timeout --signal=KILL 3 mariadb --defaults-extra-file=/lab/app.cnf \
                    --protocol=tcp --host="$vip" --connect-timeout=1 --batch --skip-column-names \
                    --execute="INSERT INTO lab.writes(token,origin) VALUES('$token',@@server_id);
SELECT origin FROM lab.writes WHERE token='$token';" 2>/tmp/network-client.stderr)" || code=$?
                printf '%s\t%s\t%s\t%s\t%s\n' "$begin" "$(date +%s%N)" "$code" "$origin" "$token" >/tmp/network-client.tsv
                exit "$code"
            fi
            sleep 0.01
        done
        printf 'client event window expired\n' >/tmp/network-client.stderr
        exit 124
    ) &
    client_pid=$!
fi
wait "$ip_pid"
OBSERVER
        local observed
        for observed in a b; do
            p cp "$WORK/network-observer.sh" "$NAME-db-$observed:/tmp/network-observer.sh"
            p exec -d "$NAME-db-$observed" timeout --signal=KILL 180 bash /tmp/network-observer.sh "$VIP" "$token"
            wait_for "netlink observer $observed subscribed" network_observer_ready "$observed"
        done
        printf 'NETWORK_OBSERVERS_ARMED realtime_ns=%s initial_read_only=%s/%s\n' "$(date +%s%N)" "$START_RO_A" "$START_RO_B"
    fi
    # The sibling DB owns this volume; it is read-only in the daemon container.
    # Keep the lifecycle worker's wrapper intact and preserve adapter stderr
    # losslessly rather than relying on truncated /status diagnostics.
    printf '#!/usr/bin/env bash\nset -euo pipefail\numask 077\nexec /usr/local/libexec/lifecycle-wrapper 2>>/tmp/network-adapter.log\n' \
        | atomic "$WORK/network-lifecycle-wrapper"
    p cp "$WORK/network-lifecycle-wrapper" "$NAME-db-$node:/run/mysqld/network-lifecycle-wrapper"
    p exec "$NAME-db-$node" chmod 755 /run/mysqld/network-lifecycle-wrapper
    sed 's|/usr/local/libexec/lifecycle-wrapper|/run/mysqld/network-lifecycle-wrapper|g' "$WORK/$node.yaml" \
        | atomic "$WORK/$node.yaml"
}

network_observer_ready() { p exec "$NAME-db-$1" test -s /tmp/network-observer.ready; }

lab_observers_cleanup() {
    local node pid suffix failed=0
    [[ "${NETWORK_OBSERVERS_ARMED:-0}" == 1 ]] || return 0
    for node in a b; do
        pid="$(p exec "$NAME-db-$node" cat /tmp/network-observer.pid)" || { failed=1; continue; }
        p exec "$NAME-db-$node" kill -TERM "$pid" || failed=1
        wait_for "netlink observer $node reaped" p exec "$NAME-db-$node" test -s /tmp/network-observer.exit || failed=1
        for suffix in ip.raw ip.stderr meta initial observer.exit client.tsv client.stderr; do
            if p exec "$NAME-db-$node" test -f "/tmp/network-$suffix"; then
                p cp "$NAME-db-$node:/tmp/network-$suffix" "$NETWORK_EVIDENCE/$node-$suffix" || failed=1
            fi
        done
        if gx "$node" test -f /tmp/network-adapter.log; then
            p cp "$NAME-gz-$node:/tmp/network-adapter.log" "$NETWORK_EVIDENCE/$node-adapter.log" || failed=1
        fi
        gx "$node" readlink /proc/self/ns/time >"$NETWORK_EVIDENCE/$node-adapter-clock" || failed=1
        p exec "$NAME-db-$node" ip -o address show dev eth0 >"$NETWORK_EVIDENCE/$node-final" || failed=1
    done
    NETWORK_OBSERVERS_ARMED=0
    return "$failed"
}

network_events() {
    local node line stamp action ns
    : >"$NETWORK_EVIDENCE/events.tsv"
    for node in a b; do
        [[ "$(cat "$NETWORK_EVIDENCE/$node-observer.exit")" == 0 ]]
        [[ ! -s "$NETWORK_EVIDENCE/$node-ip.stderr" ]]
        grep -qx 'netlink_drops=0' "$NETWORK_EVIDENCE/$node-meta"
        grep -Fxq "clock=CLOCK_REALTIME time_namespace=$(cat "$NETWORK_EVIDENCE/$node-adapter-clock")" "$NETWORK_EVIDENCE/$node-meta"
        while IFS= read -r line; do
            [[ "$line" == *"$VIP/24"* ]] || continue
            stamp="${line%%]*}"; stamp="${stamp#\[}"
            ns="$(TZ=UTC date -d "$stamp" +%s%N)"
            action=add; if [[ "$line" == *Deleted* ]]; then action=delete; fi
            printf '%s\t%s\t%s\n' "$ns" "$node" "$action" >>"$NETWORK_EVIDENCE/events.tsv"
        done <"$NETWORK_EVIDENCE/$node-ip.raw"
    done
    sort -n "$NETWORK_EVIDENCE/events.tsv" -o "$NETWORK_EVIDENCE/events.tsv"
    cat "$NETWORK_EVIDENCE/events.tsv"
}

network_exclusive() {
    local initial_owner="$1" final_owner="$2"
    network_events
    [[ "$(grep -c '^event=enable_readback_verified ' "$NETWORK_EVIDENCE/$initial_owner-adapter.log")" == 1 ]]
    ! grep -q '^event=enable_readback_verified ' "$NETWORK_EVIDENCE/$(other "$initial_owner")-adapter.log"
    # Neither namespace had a VIP before subscription. Requiring the standby
    # to have zero add events proves absence throughout, including between polls.
    awk -F'\t' -v owner="$initial_owner" -v final="$final_owner" '
        $2!=owner && $3=="add" {standby_add++}
        $3=="add" {if (held[$2]) invalid=1; held[$2]=1; adds++; if(held["a"] && held["b"]) overlap++}
        $3=="delete" {if(!held[$2]) invalid=1; held[$2]=0; deletes++}
        END {
            ok=adds==1 && !standby_add && !invalid && !overlap
            if(final=="none") ok=ok && deletes==1 && !held["a"] && !held["b"]
            else ok=ok && deletes==0 && held[owner]
            printf "NETWORK_TIMELINE owner=%s standby_add=%d overlap=%d adds=%d deletes=%d result=%s\n",owner,standby_add,overlap,adds,deletes,(ok?"PASS":"FAIL")
            exit !ok
        }' "$NETWORK_EVIDENCE/events.tsv"
    local node
    for node in a b; do
        ! awk -v vip="$VIP/24" '$3=="inet" && $4==vip {found=1} END {exit !found}' "$NETWORK_EVIDENCE/$node-initial"
    done
    [[ "$(owners)" == "$final_owner" ]]
}

strict_network_startup() {
    local begin end code origin token enabled added owner expected
    strict_record_start strict-startup 1
    if [[ "${NETWORK_OBSERVERS_ARMED:-0}" != 1 ]]; then
        report_not_run "$SCENARIO" 1 "initial formation was not prearmed; use --scenario strict-startup on a fresh fixture"
        return
    fi
    wait_for "startup kernel owner and SQL readiness" strict_owner_activated
    owner="$OWNER"; expected=1; if [[ "$owner" == b ]]; then expected=2; fi
    wait_for "first app write event recorded" p exec "$NAME-db-$owner" test -s /tmp/network-client.tsv
    lab_observers_cleanup
    network_exclusive "$owner" "$owner"
    IFS=$'\t' read -r begin end code origin token <"$NETWORK_EVIDENCE/$owner-client.tsv"
    [[ "$code" == 0 && "$origin" == "$expected" && "$token" == "$NETWORK_START_TOKEN" ]]
    enabled="$(awk '/event=enable_readback_verified/ {for(i=1;i<=NF;i++) if($i~/^realtime_ns=/) {sub(/^realtime_ns=/,"",$i); print $i; exit}}' "$NETWORK_EVIDENCE/$owner-adapter.log")"
    added="$(awk -F'\t' -v owner="$owner" '$2==owner && $3=="add" {print $1; exit}' "$NETWORK_EVIDENCE/events.tsv")"
    [[ "$enabled" =~ ^[0-9]{19}$ && "$added" =~ ^[0-9]{19}$ && "$begin" =~ ^[0-9]{19}$ ]]
    [[ "$enabled" -lt "$added" && "$added" -lt "$begin" && "$begin" -le "$end" ]]
    printf 'STARTUP_ORDER readback_ns=%s vip_add_ns=%s first_write_begin_ns=%s first_write_ack_ns=%s origin=%s token=%s\n' "$enabled" "$added" "$begin" "$end" "$origin" "$token"
    ROW="$token"
    wait_for "first startup token replicated" replicated
    report_field write_origin_server_id "$origin"
    report_field write_via "$VIP"
    report_field failover_ms "$(( (end - enabled) / 1000000 ))"
    pass "event-ordered exact readback < VIP add < first client write; initial read_only=1/1; zero overlap"
}

network_restricted() {
    local node json authority
    [[ "$(owners)" == none ]] || return 1
    for node in a b; do
        [[ "$(read_only_of "$node")" == 1 ]] || return 1
        json="$(status_json "$node")" || return 1
        # Strict silence leaves an elected candidate pending forever; Pending
        # carries no grant (ownership.rs evaluate), just as None carries none.
        authority="$(printf '%s' "$json" | json_get '$.ha.authority.state')"
        [[ "$authority" == none || "$authority" == pending ]] || return 1
        [[ "$(printf '%s' "$json" | json_get '$.ha.authority.term')" == 0 &&
           "$(printf '%s' "$json" | json_get '$.ha.authority.source')" == null &&
           "$(printf '%s' "$json" | json_get '$.ha.owned')" == false &&
           "$(printf '%s' "$json" | json_get '$.ha.lifecycle.activation_confirmed')" == false ]] || return 1
    done
}

network_drop() {
    local node="$1" chain="$2" operation="$3"
    if [[ "$chain" == OUTPUT ]]; then
        gx "$node" iptables "$operation" OUTPUT -p udp -d "$(ip_for "$(other "$node")")" --dport 9375 -j DROP
    else
        gx "$node" iptables "$operation" INPUT -p udp -s "$(ip_for "$(other "$node")")" --dport 9375 -j DROP
    fi
}

network_refused_write() {
    local node="$1" error code=0
    new_row "$SCENARIO-refused"
    error="$(strict_vip_write "$node" 2>&1)" || code=$?
    printf 'NETWORK_CLIENT realtime_ns=%s exit=%s token=%s result=%s\n' "$(date +%s%N)" "$code" "$ROW" "$error"
    [[ "$code" != 0 ]]
    [[ -z "$(row_on a)" && -z "$(row_on b)" ]]
    report_field write_origin_server_id refused
    report_field write_via "$VIP;client_exit=$code"
}

network_peer_stale() {
    local blocked="$1" node json
    for node in a b; do
        json="$(status_json "$node")" || return 1
        printf 'NETWORK_STATUS node=%s %s\n' "$node" "$json"
        # INPUT drops silence only the blocked node's receiver. The other
        # node may still receive authenticated datagrams without a usable
        # grant; that is not bilateral consent (status.rs PeerSessionStatus).
        if [[ "$node" == "$blocked" ]]; then
            [[ "$(printf '%s' "$json" | json_get '$.ha.peer_session.reachable')" == false &&
               "$(printf '%s' "$json" | json_get '$.ha.peer_session.session_established')" == false ]] || return 1
        fi
    done
}

strict_network_fault() {
    local scenario="$1" row="$2" owner standby started restricted gateway node
    strict_record_start "$scenario" "$row"
    if [[ "${NETWORK_OBSERVERS_ARMED:-0}" != 1 ]]; then
        report_not_run "$scenario" "$row" "initial formation was not prearmed; use --scenario $scenario on a fresh fixture"
        return
    fi
    wait_for "network fault begins with authorized writable owner" strict_owner_activated
    owner="$OWNER"; standby="$(other "$owner")"
    new_row "$scenario-before"
    wait_for "origin-matched write before fault" strict_writer_loop "$owner"
    wait_for "pre-fault token replicated" replicated
    started="$(now_ms)"
    printf 'NETWORK_FAULT scenario=%s begin_ms=%s initial_owner=%s\n' "$scenario" "$started" "$owner"
    case "$scenario" in
        strict-partition-bilateral|strict-gateway-reachable-peer-isolated)
            network_drop a OUTPUT -I
            network_drop b OUTPUT -I ;;
        strict-asymmetric-loss) network_drop a OUTPUT -I ;;
        strict-firewall-blocks-ha) network_drop "$standby" INPUT -I ;;
    esac
    if [[ "$scenario" == strict-gateway-reachable-peer-isolated ]]; then
        gateway="$(gx a ip -4 route show default | awk '$1=="default" {print $3; exit}')"
        [[ "$gateway" == 10.249.15.1 ]]
        for node in a b; do
            printf 'NETWORK_GATEWAY node=%s realtime_ns=%s\n' "$node" "$(date +%s%N)"
            gx "$node" ping -n -c 2 -W 1 "$gateway"
            ! grep -qi gateway "$WORK/$node.yaml"
        done
    elif [[ "$scenario" == strict-firewall-blocks-ha ]]; then
        for node in a b; do
            gx "$node" ping -n -c 2 -W 1 "$(ip_for "$(other "$node")")"
            [[ "$(replica_field "$node" Slave_IO_Running)" == Yes && "$(replica_field "$node" Slave_SQL_Running)" == Yes ]]
        done
    fi
    wait_for "authority expired, kernel VIP absent, both SQL servers restricted" network_restricted
    restricted="$(now_ms)"
    report_field failover_ms "$((restricted - started))"
    printf 'NETWORK_RESTRICTED scenario=%s observed_ms=%s fault_to_restriction_ms=%s read_only=1/1 kernel_owners=none\n' "$scenario" "$restricted" "$((restricted - started))"
    for node in a b; do
        status_json "$node" >"$NETWORK_EVIDENCE/$node-status.json"
        gx "$node" iptables -nvL >"$NETWORK_EVIDENCE/$node-firewall.txt"
    done
    if [[ "$scenario" == strict-asymmetric-loss ]]; then
        # Swap the only blocked direction without a healed bilateral window.
        network_drop b OUTPUT -I
        network_drop a OUTPUT -D
        gx b ping -n -c 2 -W 1 "$(ip_for a)"
        network_restricted
        printf 'NETWORK_ASYMMETRIC reverse_only=blocked usable_authority=none kernel_owners=none\n'
    fi
    if [[ "$scenario" == strict-firewall-blocks-ha ]]; then
        wait_for "blocked receiver session becomes stale despite live TCP replication" network_peer_stale "$standby"
        for node in a b; do
            [[ "$(replica_field "$node" Slave_IO_Running)" == Yes && "$(replica_field "$node" Slave_SQL_Running)" == Yes ]]
        done
        # An explicitly privileged fixture transaction proves that the TCP
        # stream advances during the HA-only block. This is not an app write
        # or evidence of fencing; privileged bypass remains a documented limit.
        new_row firewall-replication-fixture
        sql "$owner" "INSERT INTO lab.writes(token,origin) VALUES('$ROW',@@server_id);"
        wait_for "replication advances while HA input remains blocked" replicated
        printf 'NETWORK_REPLICATION token=%s origin=%s replicated=yes fixture=lab-root-privilege-bypass\n' "$ROW" "$(row_on "$owner")"
    fi
    network_refused_write "$standby"
    lab_observers_cleanup
    network_exclusive "$owner" none
    pass "fault_to_restriction_ms=$((restricted-started)); standby never acquired kernel VIP; zero event overlap; refused client write; divergence=unknown"
}

register_scenario strict strict-startup strict_network_startup
register_scenario strict strict-partition-bilateral 'strict_network_partition'
register_scenario strict strict-asymmetric-loss 'strict_network_asymmetric'
register_scenario strict strict-gateway-reachable-peer-isolated 'strict_network_gateway'
register_scenario strict strict-firewall-blocks-ha 'strict_network_firewall'
strict_network_partition() { strict_network_fault strict-partition-bilateral 2; }
strict_network_asymmetric() { strict_network_fault strict-asymmetric-loss 3; }
strict_network_gateway() { strict_network_fault strict-gateway-reachable-peer-isolated 4; }
strict_network_firewall() { strict_network_fault strict-firewall-blocks-ha 5; }
