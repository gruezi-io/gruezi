#!/usr/bin/env bash

strict_record_start() {
    SCENARIO="$1"
    report_reset
    report_field read_only_start_a "$START_RO_A"
    report_field read_only_start_b "$START_RO_B"
    report_field row_id "${2:-smoke}"
}

strict_smoke() {
    local standby error grants
    strict_record_start strict-smoke
    wait_for "strict owner is writable and standby read-only" strict_owner_activated
    standby="$(other "$OWNER")"
    new_row strict-smoke
    wait_for "app VIP write originates on kernel owner" strict_writer_loop "$OWNER"
    wait_for "app token replicated" replicated
    report_field write_token "$ROW"
    report_field write_origin_server_id "$(sql "$OWNER" "SELECT origin FROM lab.writes WHERE token='$ROW'")"
    report_field write_via "$VIP"
    if error="$(db_with /lab/app.cnf "$standby" --protocol=tcp --host="$(ip_for "$standby")" \
        --execute="INSERT INTO lab.writes(token,origin) VALUES('refused-$$',@@server_id);" 2>&1)"; then
        printf 'Direct app write unexpectedly succeeded\n' >&2; false
    fi
    printf 'STRICT direct_nonowner_app_write=%s\n' "$error"
    [[ "$error" == *"ERROR 1290"* ]]
    sql "$standby" "SET SESSION sql_log_bin=0; INSERT INTO lab.writes(token,origin) VALUES('root-bypass-$$',@@server_id);"
    [[ "$(sql "$standby" "SELECT COUNT(*) FROM lab.writes WHERE token='root-bypass-$$'")" == 1 ]]
    # Undo only this unlogged fixture row so later cooperative checks can use
    # equality as an observation. Vectors cannot detect this privileged bypass.
    sql "$standby" "SET SESSION sql_log_bin=0; DELETE FROM lab.writes WHERE token='root-bypass-$$';"
    printf 'STRICT lab_root_nonowner_write=accepted documented_privilege_bypass divergence=unknown\n'
    for node in a b; do
        db_with /lab/monitor.cnf "$node" --protocol=tcp --host="$(ip_for "$node")" \
            --execute='SELECT * FROM gruezi_ha.handoff_release;'
        if error="$(db_with /lab/monitor.cnf "$node" --protocol=tcp --host="$(ip_for "$node")" \
            --execute='DELETE FROM gruezi_ha.handoff_release;' 2>&1)"; then
            printf 'Monitor write unexpectedly succeeded\n' >&2; false
        fi
        printf 'STRICT monitor_write_denied node=%s error=%s\n' "$node" "$error"
        # A read-only server reports 1290 before it evaluates privileges, so the
        # privilege boundary is also proven from the monitor's actual grants.
        [[ "$error" == *"ERROR 1142"* || "$error" == *"ERROR 1290"* ]]
        # Select only privilege names and scope. SHOW GRANTS includes password
        # verifiers even for these disposable accounts and must never be logged.
        grants="$(sql "$node" "SET @monitor_grantee=CONCAT(CHAR(39),'gruezi_monitor',CHAR(39),'@',CHAR(39),'%',CHAR(39));
SELECT 'global','*.*',PRIVILEGE_TYPE,IS_GRANTABLE FROM information_schema.USER_PRIVILEGES WHERE GRANTEE=@monitor_grantee
UNION ALL SELECT 'schema',CONCAT(TABLE_SCHEMA,'.*'),PRIVILEGE_TYPE,IS_GRANTABLE FROM information_schema.SCHEMA_PRIVILEGES WHERE GRANTEE=@monitor_grantee
UNION ALL SELECT 'table',CONCAT(TABLE_SCHEMA,'.',TABLE_NAME),PRIVILEGE_TYPE,IS_GRANTABLE FROM information_schema.TABLE_PRIVILEGES WHERE GRANTEE=@monitor_grantee
UNION ALL SELECT 'column',CONCAT(TABLE_SCHEMA,'.',TABLE_NAME,'.',COLUMN_NAME),PRIVILEGE_TYPE,IS_GRANTABLE FROM information_schema.COLUMN_PRIVILEGES WHERE GRANTEE=@monitor_grantee
ORDER BY 1,2,3;")"
        printf 'STRICT monitor_privileges node=%s %s\n' "$node" "$(printf '%s' "$grants" | tr '\n' ';')"
        printf '%s\n' "$grants" | awk -F'\t' '
            $1=="global" && $2=="*.*" && ($3=="SLAVE MONITOR" || $3=="REPLICA MONITOR") && $4=="NO" { monitor++; next }
            $1=="table" && $2=="gruezi_ha.handoff_release" && $3=="SELECT" && $4=="NO" { receipt++; next }
            { invalid=1 }
            END { exit !(monitor==1 && receipt==1 && !invalid) }'
    done
    b4_eaccensevidence "$OWNER"
    adapter_portability_check "1-1-1,2-2-1" "1-1-2,2-2-3" "1-1-1,2-2-1"
    pass "initial read_only=1/1; origin-matched app VIP write; direct standby ERROR 1290; lab-root bypass accepted"
}

run_strict() {
    local i
    strict_smoke
    if [[ -n "$SCENARIO_FILTER" ]]; then
        if [[ "$SCENARIO_FILTER" != strict-smoke ]]; then "$SELECTED_HANDLER"; fi
        return
    fi
    for ((i=0; i<${#SCENARIO_IDS[@]}; i++)); do
        if [[ "${SCENARIO_PROFILES[i]}" == strict && "${SCENARIO_IDS[i]}" != strict-smoke ]]; then
            "${SCENARIO_FUNCTIONS[i]}"
        fi
    done
}

register_scenario strict strict-smoke strict_smoke
