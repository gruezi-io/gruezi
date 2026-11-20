#!/usr/bin/env bash

# Reads the real adapter carrier, checks its canonical digest and live target,
# then independently repeats the origin-aware coverage and GTID wait.
strict_receipt_evidence() {
    local predecessor="$1" successor="$2" receipt digest actual state
    receipt="$(receipt_row_via_monitor "$predecessor")"
    [[ -n "$receipt" && "$receipt" != *$'\n'* ]]
    IFS=$'\t' read -r singleton transfer server instance generation incarnation pos current slave state nonce released digest <<<"$receipt"
    [[ "$singleton" == 1 && "$transfer" != 0 && "$generation" != 0 ]]
    actual="$(receipt_canonical_digest "$singleton" "$transfer" "$server" "$instance" "$generation" \
        "$incarnation" "$pos" "$current" "$slave" "$state" "$nonce" "$released")"
    [[ "$actual" == "$digest" ]]
    [[ "$(gtid_binlog_state_of "$predecessor")" == "$state" ]]
    [[ "$(read_only_of "$predecessor")" == 1 ]]
    [[ "$(replica_field "$predecessor" Slave_IO_Running)" == No && "$(replica_field "$predecessor" Slave_SQL_Running)" == No ]]
    [[ "$(sql "$predecessor" 'SELECT @@global.event_scheduler')" == OFF ]]
    [[ "$(sql "$predecessor" 'SELECT COUNT(*) FROM information_schema.INNODB_TRX')" == 0 ]]
    [[ -z "$(sql "$predecessor" 'XA RECOVER')" ]]
    [[ "$(sql "$predecessor" "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE ID<>CONNECTION_ID() AND USER NOT IN ('system user','event_scheduler','gruezi_monitor') AND COMMAND NOT IN ('Binlog Dump','Binlog Dump GTID','Slave_IO','Slave_SQL')")" == 0 ]]
    local frontier
    frontier="$(printf '%s' "$state" | tr ',' '\n' | awk -F- -v s="$server" '$2==s {printf "%s%s",sep,$0;sep=","}')"
    [[ -n "$frontier" ]]
    [[ "$(sql "$successor" "SELECT MASTER_GTID_WAIT('$frontier',3)")" == 0 ]]
    [[ "$(adapter_in_container "$successor" internal-gtid-covers state "$(gtid_binlog_state_of "$successor")" state "$state")" == covered=yes ]]
    report_field receipt_identity "instance=$instance;generation=$generation;transfer=$transfer;db=$incarnation;digest=$digest"
    report_field receipt_barrier_attestations "read_only=1;io=No;sql=No;write_sessions=0;transactions=0;xa=0;event_scheduler=OFF;digest=matched;live_target=matched"
    report_field receipt_target "$state"
    report_field coverage_result "MASTER_GTID_WAIT=0;origin_coverage=yes"
    printf 'RECEIPT instance=%s generation=%s transfer=%s target=%s MASTER_GTID_WAIT=0 origin_coverage=yes\n' "$instance" "$generation" "$transfer" "$state"
}

strict_cooperative_last_token() {
    local predecessor successor started observed_exit legacy_exit=0 window_begin client_begin client_end window_end token origin
    strict_record_start strict-cooperative-last-token cooperative
    wait_for "strict owner ready" strict_owner_activated
    predecessor="$OWNER"; successor="$(other "$OWNER")"
    [[ "$predecessor" == a && "$successor" == b ]]
    new_row last-acknowledged
    wait_for "last app token acknowledged" strict_writer_loop "$predecessor"
    handoff_arm_observers "$successor"
    started="$(handoff_monotonic_ms)"
    p kill --signal TERM "$NAME-gz-$predecessor"
    if [[ "${LAB_COMPARE_LEGACY_OBSERVATION:-0}" == 1 ]]; then
        wait_for "legacy sequential activation observation" strict_successor_activated "$successor" || legacy_exit=$?
        printf 'LEGACY_RESULT exit=%s injected_host_latency_ms=%s\n' "$legacy_exit" "${LAB_HOST_OBSERVATION_DELAY_MS:-0}"
    fi
    wait_for "prearmed successor observation completes" gx_file_nonempty "$successor" /tmp/handoff-activation.exit
    observed_exit="$(handoff_host_call observer_exit gx "$successor" cat /tmp/handoff-activation.exit)"
    handoff_collect_observers
    [[ "$observed_exit" == 0 ]]
    IFS=$'\t' read -r window_begin client_begin client_end window_end token origin <"$REPORT_DIR/activation.tsv"
    [[ "$token" == "$ROW-activated" && "$origin" == 2 ]]
    awk -F'\t' -v begin="$window_begin" -v end="$window_end" '
        $2<=begin { before=($3==0); before_end=$2 }
        $1<=end && $2>=begin && $3!=0 { overlap=1 }
        $1>=end && !after_seen { after_seen=1; after=($3==0); after_begin=$1 }
        $1>=begin && $1<=end { if (last && $1-last>gap) gap=$1-last; last=$2; middle++ }
        END {
            ok=before && after && !overlap && middle>0 && begin-before_end<=200 && after_begin-end<=200 && gap<=200
            printf "EXCLUSIVE_KERNEL window_begin_ms=%s window_end_ms=%s predecessor_absent_before=%d predecessor_absent_after=%d overlap=%d samples=%d max_gap_ms=%d result=%s\n",begin,end,before,after,overlap,middle,gap,(ok?"PASS":"FAIL")
            exit !ok
        }' "$REPORT_DIR/kernel-a.tsv"
    [[ "$(row_on "$successor")" == "$(row_on "$predecessor")" ]]
    strict_receipt_evidence "$predecessor" "$successor"
    report_field coverage_result "MASTER_GTID_WAIT=0;origin_coverage=yes;exclusive_kernel=yes;activation_token=$token;activation_window_ms=$window_begin..$window_end"
    report_field failover_ms "$(( client_end - started ))"
    report_field write_token "$ROW"
    report_field write_origin_server_id "$(sql "$successor" "SELECT origin FROM lab.writes WHERE token='$ROW'")"
    report_field write_via "$VIP"
    report_field rpo_record "conditional pending PO-1..PO-6 evidence; A1=verified_barrier;A2=assumed_no_privileged_writer;A3=log_bin_and_log_slave_updates;A4=no_filters;A5=gtid_strict_mode;A6=assumed_trust_boundary"
    stopped() { [[ "$(p inspect --format '{{.State.Running}}' "$NAME-gz-$1")" == false ]]; }
    wait_for "predecessor exits after handoff" stopped "$predecessor"
    wait_for "distinct later consent withdrawal" strict_consent_withdrawn "$successor"
    printf 'CONSENT_WITHDRAWAL monotonic_ms=%s kernel_owners=none read_only=1/1 successor_owned=false\n' "$(handoff_monotonic_ms)"
    p logs "$NAME-gz-$successor" >"$REPORT_DIR/successor-daemon.log"
    grep -q 'consent_expired' "$REPORT_DIR/successor-daemon.log"
    pass "prearmed exclusive successor activation and origin-2 app VIP write; preserved last token and exact receipt coverage; later consent expiry withdrew VIP; RPO conditional pending PO evidence under A1-A6"
    p start "$NAME-gz-$predecessor"
    wait_for "bilateral session restored" peer_healthy
}

strict_consent_withdrawn() {
    local successor="$1" json
    [[ "$(owners)" == none && "$(read_only_of a)" == 1 && "$(read_only_of b)" == 1 ]] || return 1
    json="$(status_json "$successor")" || return 1
    [[ "$(printf '%s' "$json" | json_get '$.ha.owned')" == false &&
       "$(printf '%s' "$json" | json_get '$.ha.authority.state')" == none ]]
}

handoff_arm_observers() {
    local successor="$1" node
    mkdir -p "$REPORT_DIR"
    printf '%s\n' "$JSON_PATHS_AWK" | atomic "$WORK/handoff-status.awk"
    for node in a b; do
        p cp "$HERE/observe-handoff.sh" "$NAME-db-$node:/tmp/observe-handoff.sh"
        p exec -d "$NAME-db-$node" timeout --signal=KILL 70 bash /tmp/observe-handoff.sh kernel "$VIP"
        wait_for "kernel observer $node armed" db_observer_ready "$node"
    done
    p cp "$HERE/observe-handoff.sh" "$NAME-gz-$successor:/tmp/observe-handoff.sh"
    p cp "$WORK/handoff-status.awk" "$NAME-gz-$successor:/tmp/handoff-status.awk"
    p exec -d "$NAME-gz-$successor" timeout --signal=KILL 70 bash /tmp/observe-handoff.sh \
        activation "$VIP" "$ROW-activated" "$ROW" 2
    wait_for "successor observer armed" gx_file_nonempty "$successor" /tmp/handoff-activation.ready
    HANDOFF_OBSERVERS_ARMED=1
    printf 'OBSERVERS_ARMED monotonic_ms=%s successor=%s token=%s\n' "$(handoff_monotonic_ms)" "$successor" "$ROW-activated"
}

db_observer_ready() { p exec "$NAME-db-$1" test -s /tmp/handoff-kernel.ready; }

handoff_collect_observers() {
    local node suffix
    [[ "${HANDOFF_OBSERVERS_ARMED:-0}" == 1 ]] || return 0
    for node in a b; do
        # Podman cp archives a fixed file size. Stop and reap the append-only
        # writer first; copying its live log can fail with "write too long".
        p exec "$NAME-db-$node" timeout --signal=KILL 3 sh -c '
            pid="$(cat /tmp/handoff-kernel.pid)"
            if kill -0 "$pid" 2>/dev/null; then
                kill "$pid"
                while kill -0 "$pid" 2>/dev/null; do sleep 0.01; done
            fi'
        p cp "$NAME-db-$node:/tmp/handoff-kernel.tsv" "$REPORT_DIR/kernel-$node.tsv"
    done
    for suffix in log exit tsv; do
        if gx b test -f "/tmp/handoff-activation.$suffix"; then
            p cp "$NAME-gz-b:/tmp/handoff-activation.$suffix" "$REPORT_DIR/activation.$suffix"
        fi
    done
    if [[ -f "$REPORT_DIR/activation.log" ]]; then
        printf 'PREARMED_OBSERVER_EVIDENCE file=%s/activation.log\n' "$REPORT_DIR"
        cat "$REPORT_DIR/activation.log"
    fi
    HANDOFF_OBSERVERS_ARMED=0
}

strict_successor_activated() {
    local successor="$1" json observed ROW="${ROW}-legacy"
    observed="$(handoff_host_call kernel owners)" || return 1
    [[ "$observed" == "$successor" ]] || {
        printf 'HOST_PREDICATE failed=kernel skipped=sql,status,app\n' >&2; return 1;
    }
    observed="$(handoff_host_call sql read_only_of "$successor")" || return 1
    [[ "$observed" == 0 ]] || {
        printf 'HOST_PREDICATE failed=read_only skipped=status,app\n' >&2; return 1;
    }
    json="$(handoff_host_call status status_json "$successor")" || return 1
    [[ "$(printf '%s' "$json" | json_get '$.ha.owned')" == true ]] || {
        printf 'HOST_PREDICATE failed=owned skipped=app\n' >&2; return 1;
    }
    observed="$(handoff_host_call token row_on "$successor")" || return 1
    if [[ -z "$observed" ]]; then
        handoff_host_call app strict_vip_write "$successor" || return 1
        observed="$(handoff_host_call token_readback row_on "$successor")" || return 1
    fi
    [[ "$observed" == "$ROW:server=2" ]] || return 1
    printf 'STRICT_ACTIVATION successor=%s owned=true read_only=0 vip_write=%s origin=%s\n' \
        "$successor" "$ROW" "$observed"
}

handoff_monotonic_ms() {
    awk '{printf "%.0f\n", $1 * 1000}' /proc/uptime
}

handoff_host_call() {
    local label="$1" begin end result code=0 delay="${LAB_HOST_OBSERVATION_DELAY_MS:-0}"
    shift
    [[ "$delay" =~ ^[0-9]+$ && "$delay" -le 2000 ]] || return 2
    begin="$(handoff_monotonic_ms)"
    if [[ "$delay" != 0 ]]; then sleep "$(awk -v ms="$delay" 'BEGIN {printf "%.3f",ms/1000}')"; fi
    result="$("$@" 2>&1)" || code=$?
    end="$(handoff_monotonic_ms)"
    printf 'HOST_OBSERVATION predicate=%s begin_ms=%s end_ms=%s exit=%s value=%s\n' \
        "$label" "$begin" "$end" "$code" "$result" >&2
    printf '%s' "$result"
    return "$code"
}

strict_late_queued_replication() {
    local predecessor successor target before after observed_owners
    strict_record_start strict-late-queued-replication cooperative
    wait_for "strict owner ready" strict_owner_activated
    predecessor="$OWNER"; successor="$(other "$OWNER")"
    # Keep an event in the predecessor's relay log. A privileged fixture write
    # on the standby is declared explicitly; the app never has this privilege.
    sql "$predecessor" 'STOP SLAVE SQL_THREAD;'
    new_row queued-replication
    sql "$successor" "INSERT INTO lab.writes(token,origin) VALUES('$ROW',@@server_id);"
    target="$(sql "$successor" 'SELECT @@global.gtid_binlog_pos')"
    queued_received() {
        [[ "$(replica_field "$predecessor" Gtid_IO_Pos)" == "$target" && -z "$(row_on "$predecessor")" ]]
    }
    wait_for "queued event received but not applied" queued_received
    before="$(gtid_binlog_state_of "$predecessor")"
    p kill --signal TERM "$NAME-gz-$predecessor"
    wait_for "release carrier published" receipt_published "$predecessor"
    # A release stops BOTH threads. A queued commit cannot be released by the
    # replication IO stream after capture: the applier is stopped structurally.
    sql "$predecessor" 'START SLAVE IO_THREAD;'
    wait_for "IO resumed for queued release attempt" io_started "$predecessor"
    [[ "$(replica_field "$predecessor" Slave_SQL_Running)" == No ]]
    [[ -z "$(row_on "$predecessor")" ]]
    after="$(gtid_binlog_state_of "$predecessor")"
    [[ "$before" == "$after" ]]
    [[ "$(read_only_of "$predecessor")" == 1 ]]
    observed_owners="$(owners)"
    [[ "$observed_owners" == none || "$observed_owners" == "$successor" ]]
    sql "$predecessor" 'STOP REPLICA;'
    report_field receipt_identity "$(receipt_row_via_monitor "$predecessor")"
    report_field receipt_target "$after"
    report_field receipt_barrier_attestations "queued_relay_event=received;post_capture_io_release=attempted;sql_applier=No;queued_token=absent;target=unchanged"
    report_field coverage_result "barrier blocks queued commit"
    report_field write_token "$ROW"
    report_field write_origin_server_id "$(sql "$successor" "SELECT origin FROM lab.writes WHERE token='$ROW'")"
    report_field write_via "direct lab-root replication fixture"
    pass "queued replication event remains unapplied after capture and IO release; release barrier blocks late commit"
}

receipt_published() { [[ -n "$(receipt_row_via_monitor "$1")" ]]; }
io_started() { [[ "$(replica_field "$1" Slave_IO_Running)" == Yes ]]; }

register_scenario strict strict-cooperative-last-token strict_cooperative_last_token
register_scenario strict strict-late-queued-replication strict_late_queued_replication
