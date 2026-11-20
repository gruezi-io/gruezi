#!/usr/bin/env bash
# Original available scenarios, in their original order.
run_available_core() {
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
}

for scenario_id in normal unfenced-direct-write graceful-failover active-crash partition replication-link-loss recovery; do
    register_scenario available "$scenario_id" run_available_core
done
