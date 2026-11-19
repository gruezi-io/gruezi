#!/usr/bin/env bash
# Scripted fake `mariadb` client for the gruezi-mariadb-adapter tests.
#
# Every invocation logs its complete argv and its stdin to the call log before
# doing anything else: the read-only and credential-hygiene assertions are
# proven from that log, never from prose. Responses are fixture files keyed by
# the option-file basename (`--defaults-extra-file=<path>` -> out.<basename>),
# so a test can hand the adapter exactly the output shape it wants to probe.
#
# Control files under GRUEZI_FAKE_MARIADB_FIXTURE:
#   mode          optional: "normal" (default), "hang" (never answer; the
#                 adapter's timeout must kill this process), "fail" (exit 1)
#   out.<base>    the client output for the option file with basename <base>
set -eu

log="${GRUEZI_FAKE_MARIADB_LOG:?}"
fix="${GRUEZI_FAKE_MARIADB_FIXTURE:?}"

interactive=no
for arg in "$@"; do [[ "$arg" != --unbuffered ]] || interactive=yes; done
if [[ "$interactive" == no ]]; then
    sql="$(cat)"
else
    sql=""
fi
{
    printf 'ARGV:'
    for arg in "$@"; do
        printf ' <%s>' "$arg"
    done
    printf '\n'
    printf 'STDIN:\n'
    printf '%s\n' "$sql"
    printf 'END\n'
} >>"$log"

option_file=""
for arg in "$@"; do
    case "$arg" in
        --defaults-extra-file=*) option_file="${arg#--defaults-extra-file=}" ;;
    esac
done
base="${option_file##*/}"

mode="normal"
if [[ -f "$fix/mode" ]]; then
    read -r mode <"$fix/mode" || true
fi
case "$mode" in
    hang)
        exec sleep 30
        ;;
    fail)
        printf 'fake mariadb: injected client failure\n' >&2
        exit 1
        ;;
esac

if [[ -f "$fix/lifecycle" ]]; then
    control() {
        if [[ -f "$fix/$1" ]]; then cat "$fix/$1"; else printf '%s' "${2:-}"; fi
    }
    ro() { control read_only 1; }
    state="${GRUEZI_FAKE_STATE_DIR:?}"
    if [[ "$interactive" == yes ]]; then
        while IFS= read -r line; do
            if [[ "$line" != 'SET GLOBAL read_only=0;' ]]; then printf '%s\n' "$line" >>"$log"; fi
            case "$line" in
                'SELECT CONNECTION_ID();') printf '77\n' ;;
                'SET GLOBAL read_only=0;')
                    [[ -s "$state/operation.record" ]] || exit 1
                    compgen -G "$state/dirty.*.record" >/dev/null || exit 1
                    {
                        printf 'EPOCH_BEFORE_ENABLE\n'
                        cat "$state/operation.record"
                        if [[ -f "$state/authority.consumed" ]]; then printf 'OVERRIDE_CONSUMED_BEFORE_ENABLE\n'; fi
                        if [[ -f "$state/consumed.keys" ]]; then printf 'RECEIPT_CONSUMED_BEFORE_ENABLE\n'; fi
                        printf '%s\n' "$line"
                    } >>"$log"
                    if [[ ! -f "$fix/readback_mismatch" ]]; then printf '0\n' >"$fix/read_only"; fi
                    ;;
                "SELECT 'enabled';") printf 'enabled\n' ;;
            esac
        done
        exit 0
    fi
    if [[ "$base" == peer.cnf && -f "$fix/peer_unreachable" ]]; then
        printf 'ERROR 2003 (HY000): Cannot connect to peer\n' >&2
        exit 1
    fi
    if [[ -f "$fix/deny_sql" && "$sql" == *"$(control deny_sql)"* ]]; then
        printf 'ERROR 1227 (42000): Access denied\n' >&2
        exit 1
    fi
    case "$sql" in
        'SELECT @@global.server_id,@@global.gtid_domain_id,@@global.read_only;')
            if [[ -f "$fix/peer_writable" ]]; then printf '2\t1\t0\n'; else printf '2\t1\t1\n'; fi
            ;;
        'SELECT @@global.server_id,@@global.gtid_domain_id,@@global.read_only,@@global.pid_file;')
            scalar="$(head -n 1 "$fix/out.local.cnf")"
            printf '%s\t%s\t%s\t%s\n' "$(cut -f1 <<<"$scalar")" "$(cut -f2 <<<"$scalar")" "$(ro)" "$(cut -f8 <<<"$scalar")"
            ;;
        'SELECT @@global.read_only;') ro; printf '\n' ;;
        'SET GLOBAL read_only=1;') printf '1\n' >"$fix/read_only" ;;
        'STOP REPLICA;') printf 'No\n' >"$fix/threads" ;;
        'START REPLICA;') printf 'Yes\n' >"$fix/threads" ;;
        'SELECT ID FROM information_schema.PROCESSLIST'*)
            if [[ "$sql" == *CURRENT_USER* ]]; then
                [[ -f "$fix/killed.41" ]] || printf '41\n'
            else
                [[ -f "$fix/killed.42" ]] || printf '42\n'
                if [[ -f "$fix/monitor_session" && "$sql" != *"USER<>'monitor'"* ]]; then printf '43\n'; fi
            fi
            ;;
        'KILL CONNECTION '*) id="${sql#KILL CONNECTION }"; touch "$fix/killed.${id%;}" ;;
        'SELECT COUNT(*) FROM information_schema.PROCESSLIST'*)
            if [[ -f "$fix/monitor_session" && "$sql" != *"USER<>'monitor'"* && "$sql" != *CURRENT_USER* ]]; then
                printf '1\n'
            else
                control sessions_remaining 0; printf '\n'
            fi
            ;;
        'SELECT COUNT(*) FROM information_schema.INNODB_TRX;'*) control transactions 0; printf '\n' ;;
        'XA RECOVER;') control xa ;;
        'SELECT MASTER_GTID_WAIT('*)
            control wait_result 0; printf '\n'
            if [[ -f "$fix/after_wait.local" ]]; then cp "$fix/after_wait.local" "$fix/out.local.cnf"; fi
            ;;
        'SELECT COUNT(*) FROM gruezi_ha.handoff_release;')
            if [[ -n "$(control "receipt.$base")" ]]; then printf '1\n'; else printf '0\n'; fi
            ;;
        'SELECT singleton_id,'*)
            if [[ "$base" == local.cnf && -f "$fix/receipt_readback_corrupt" ]]; then
                printf 'corrupt\n'
            else
                control "receipt.$base"; printf '\n'
            fi
            ;;
        'SET SESSION sql_log_bin=0;'*)
            if [[ "$sql" == *'DELETE FROM'* ]]; then rm -f "$fix/receipt.$base"; exit 0; fi
            # All carrier values have the validated data-only alphabet, so
            # quoted SQL values have no escaping ambiguity in this fake.
            values="${sql##*VALUES (}"
            values="${values%);}"
            awk -F"'" '{ for(i=2;i<=NF;i+=2) printf "%s%s", (i==2?"":"\t"),$i;printf "\n" }' <<<"$values" >"$fix/receipt.$base"
            ;;
        *information_schema.GLOBAL_STATUS*)
            if [[ "$base" == local.cnf ]]; then
                printf '1\t0\t%s\t%s\t100\tON\tON\tON\tROW\tOFF\n' "$(ro)" "$(control event_scheduler OFF)"
                head -n 4 "$fix/out.local.cnf" | tail -n 3 | head -n 1
                head -n 4 "$fix/out.local.cnf" | tail -n 3 | head -n 1
                head -n 4 "$fix/out.local.cnf" | tail -n 1
                head -n 3 "$fix/out.local.cnf" | tail -n 1
                threads="$(control threads Yes)"
            else
                peer_ro=1
                [[ ! -f "$fix/peer_writable" ]] || peer_ro=0
                printf '2\t1\t%s\tOFF\t%s\tON\tON\tON\tROW\tOFF\n' "$peer_ro" "$(control peer_uptime 100)"
                head -n 2 "$fix/out.peer.cnf" | tail -n 1
                head -n 2 "$fix/out.peer.cnf" | tail -n 1
                head -n 4 "$fix/out.peer.cnf" | tail -n 1
                head -n 3 "$fix/out.peer.cnf" | tail -n 1
                threads="$(control peer_threads No)"
                if [[ -f "$fix/late_peer" ]]; then
                    printf '2\t1\n0-1-10,1-2-21\n0-1-1-10,1-2-1-21\n0-1-10,1-2-21\n' >"$fix/out.peer.cnf"
                fi
            fi
            printf 'Connection_name: \nSlave_IO_Running: %s\nSlave_SQL_Running: %s\n' "$threads" "$threads"
            printf 'Last_IO_Errno: 0\nLast_SQL_Errno: 0\n'
            printf 'Using_Gtid: Slave_Pos\nGtid_IO_Pos: 0-1-10,1-2-20\n'
            ;;
        *)
            cat "$fix/out.$base"
            ;;
    esac
    exit 0
fi

output="$fix/out.$base"
if [[ ! -f "$output" ]]; then
    printf 'fake mariadb: no fixture output named out.%s\n' "$base" >&2
    exit 1
fi
cat "$output"
