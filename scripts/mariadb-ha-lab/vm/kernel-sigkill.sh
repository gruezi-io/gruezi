#!/usr/bin/env bash
# Called only after local guard authorization; never selects a process by name.
set -euo pipefail
command -v jq >/dev/null
[[ "$(systemctl show gruezi --property=Restart --value)" == no ]]
[[ "$(systemctl is-active gruezi)" == active ]]
pid="$(systemctl show gruezi --property=MainPID --value)"
[[ "$pid" =~ ^[1-9][0-9]*$ && "$pid" -gt 1 ]]
status="$(curl --max-time 5 -fsS http://127.0.0.1:9376/status)"
printf 'before=%s\n' "$status"
printf '%s\n' "$status" | jq -e '.ha | (.owned == true) and (.failure_policy == "strict")'
ip -o address show | awk -v vip="$1" '$4 == vip { found=1 } END { exit !found }'
# A watchdog could reset this host on SIGKILL. This row never authorizes reset.
[[ "$(systemctl show --property=RuntimeWatchdogUSec --value)" == 0 ]]
for state in /sys/class/watchdog/watchdog*/state; do
    [[ ! -e "$state" || "$(<"$state")" == inactive ]]
done
restore() { systemctl start gruezi; }
trap restore EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
kill -KILL "$pid"
printf 'kernel_after_SIGKILL:\n'
ip -o address show
printf 'expected_residual_vip=%s residual_writer_possible=yes\n' "$1"
