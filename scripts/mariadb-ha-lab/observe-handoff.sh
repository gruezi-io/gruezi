#!/usr/bin/env bash
# Disposable, prearmed observation only. No authority or server state changes.
set -euo pipefail
mode="$1" vip="$2"
mono() { awk '{printf "%.0f\n",$1*1000}' /proc/uptime; }
kernel() {
    timeout --signal=KILL 2 ip -o addr show dev eth0 \
        | awk -v vip="$vip/24" '$3=="inet" && $4==vip {found=1} END {print found+0}'
}
deadline=$(( $(mono) + 65000 ))
if [[ "$mode" == kernel ]]; then
    printf '%s\n' "$$" >/tmp/handoff-kernel.pid
    : >/tmp/handoff-kernel.tsv
    printf 'ready\n' >/tmp/handoff-kernel.ready
    while (( $(mono) < deadline )); do
        begin="$(mono)"
        value="$(kernel)"
        printf '%s\t%s\t%s\n' "$begin" "$(mono)" "$value" >>/tmp/handoff-kernel.tsv
        sleep 0.02
    done
    exit 0
fi
[[ "$mode" == activation ]] || exit 2
token="$3" old_token="$4" server="$5"
exec >/tmp/handoff-activation.log 2>&1
trap 'code=$?; printf "%s\n" "$code" >/tmp/handoff-activation.exit' EXIT
printf 'ready\n' >/tmp/handoff-activation.ready
field() { awk -F'\t' -v path="$1" '$1==path {print $2}' <<<"$paths"; }
status() {
    local begin code=0
    begin="$(mono)"
    json="$(timeout --signal=KILL 2 curl --max-time 1 -fsS http://127.0.0.1:9376/status)" || code=$?
    paths="$(printf '%s' "$json" | awk -f /tmp/handoff-status.awk)"
    owned="$(field '$.ha.owned')"
    authority="$(field '$.ha.authority.state')"
    resource="$(field '$.ha.resource.state')"
    printf 'LOCAL_STATUS begin_ms=%s end_ms=%s exit=%s owned=%s authority=%s resource=%s\n' \
        "$begin" "$(mono)" "$code" "$owned" "$authority" "$resource"
    [[ "$code" == 0 ]]
}
read_only() {
    local begin code=0
    begin="$(mono)"
    ro="$(timeout --signal=KILL 2 mariadb --defaults-extra-file=/lab/app.cnf \
        --protocol=tcp --host=127.0.0.1 --connect-timeout=1 --batch --skip-column-names \
        --execute='SELECT @@global.read_only' 2>&1)" || code=$?
    printf 'LOCAL_SQL begin_ms=%s end_ms=%s exit=%s read_only=%s\n' "$begin" "$(mono)" "$code" "$ro"
    [[ "$code" == 0 ]]
}
while (( $(mono) < deadline )); do
    window_begin="$(mono)"
    status
    present="$(kernel)"
    printf 'LOCAL_KERNEL begin_ms=%s end_ms=%s present=%s\n' "$window_begin" "$(mono)" "$present"
    read_only
    if [[ "$owned" != true || "$authority" != valid || "$resource" != present || "$present" != 1 || "$ro" != 0 ]]; then
        printf 'LOCAL_PREDICATE eligible=no app=not_attempted\n'
        sleep 0.02
        continue
    fi
    printf 'STATUS_BEFORE %s\n' "$json"
    client_begin="$(mono)"
    client_code=0
    result="$(timeout --signal=KILL 2 mariadb --defaults-extra-file=/lab/app.cnf \
        --protocol=tcp --host="$vip" --connect-timeout=1 --batch --skip-column-names \
        --execute="INSERT INTO lab.writes(token,origin) VALUES('$token',@@server_id);
SELECT token,origin FROM lab.writes WHERE token IN ('$token','$old_token') ORDER BY token;" 2>&1)" || client_code=$?
    client_end="$(mono)"
    printf 'LOCAL_APP begin_ms=%s end_ms=%s exit=%s result=%s\n' "$client_begin" "$client_end" "$client_code" "$result"
    # There is exactly one INSERT attempt. An error is a terminal observation,
    # never an idempotent retry that could hide the failed application call.
    [[ "$client_code" == 0 ]]
    printf '%s\n' "$result" | awk -F'\t' -v new="$token" -v old="$old_token" -v server="$server" '
        $1==new && $2==server { fresh++; next }
        $1==old && $2==1 { acknowledged++; next }
        { invalid=1 }
        END {exit !(fresh==1 && acknowledged==1 && !invalid)}'
    status
    present="$(kernel)"
    read_only
    printf 'STATUS_AFTER %s\n' "$json"
    [[ "$owned" == true && "$authority" == valid && "$resource" == present && "$present" == 1 && "$ro" == 0 ]]
    window_end="$(mono)"
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$window_begin" "$client_begin" "$client_end" "$window_end" "$token" "$server" \
        >/tmp/handoff-activation.tsv
    printf 'LOCAL_ACTIVATION_PASS window_begin_ms=%s client_begin_ms=%s client_end_ms=%s window_end_ms=%s token=%s origin=%s\n' \
        "$window_begin" "$client_begin" "$client_end" "$window_end" "$token" "$server"
    while (( $(mono) < deadline )); do
        withdrawal_begin="$(mono)"
        status
        present="$(kernel)"
        read_only
        if [[ "$owned" == false && "$authority" == none && "$present" == 0 && "$ro" == 1 ]]; then
            printf 'LOCAL_WITHDRAWAL begin_ms=%s end_ms=%s owned=false authority=none kernel_present=0 read_only=1\n' \
                "$withdrawal_begin" "$(mono)"
            printf 'STATUS_WITHDRAWN %s\n' "$json"
            exit 0
        fi
        sleep 0.02
    done
    printf 'LOCAL_WITHDRAWAL_TIMEOUT activation_was_observed=yes\n'
    exit 124
done
printf 'LOCAL_ACTIVATION_TIMEOUT deadline_ms=%s app=not_attempted\n' "$deadline"
exit 124
