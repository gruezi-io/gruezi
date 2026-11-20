#!/usr/bin/env bash
run_available_hooks() {
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
}

register_scenario available read-only-hook run_available_hooks
register_scenario available read-only-hook-handoff run_available_hooks
