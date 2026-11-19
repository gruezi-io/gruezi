#!/usr/bin/env bash
# Destructive acceptance test for a throwaway Linux lab VM. The final phase
# deliberately resets the VM; run it again after reboot to inspect the log.
# Gruezi must be the only watchdog owner. systemd RuntimeWatchdogSec must NOT
# share this device.
set -euo pipefail

if [[ "${GRUEZI_ALLOW_WATCHDOG_RESET:-}" != 1 ]]; then
    printf '%s\n' 'Refusing: set GRUEZI_ALLOW_WATCHDOG_RESET=1 on the disposable lab VM only.' >&2
    exit 2
fi
if [[ "$(id -u)" != 0 || "$(uname -s)" != Linux ]]; then
    printf '%s\n' 'This test requires root on Linux.' >&2
    exit 2
fi
# sudo's secure_path omits /usr/local/bin on Rocky, so gruezi is resolved from
# an explicit path rather than PATH, and a missing piece is named.
binary="${GRUEZI_BIN:-/usr/local/bin/gruezi}"
for command in ip modprobe curl; do
    command -v "$command" >/dev/null || { printf 'Missing required command: %s\n' "$command" >&2; exit 2; }
done
[[ -x "$binary" ]] || { printf 'Missing gruezi executable %s; install it there or set GRUEZI_BIN.\n' "$binary" >&2; exit 2; }
if command -v systemctl >/dev/null; then
    runtime_watchdog="$(systemctl show --property=RuntimeWatchdogUSec --value)"
    if [[ -n "$runtime_watchdog" && "$runtime_watchdog" != 0 ]]; then
        printf 'Refusing systemd RuntimeWatchdogSec=%s; the device must be exclusive.\n' "$runtime_watchdog" >&2
        exit 2
    fi
fi
modprobe softdog nowayout=0 soft_noboot=0 soft_panic=0
if [[ -d /sys/module/softdog/parameters ]]; then
    for parameter in nowayout soft_noboot soft_panic; do
        value="$(<"/sys/module/softdog/parameters/$parameter")"
        if [[ "$value" != 0 && "$value" != N ]]; then
            printf 'Refusing softdog %s=%s\n' "$parameter" "$value" >&2
            exit 2
        fi
    done
else
    # Rocky 10 exports no softdog sysfs parameters; the kernel's load message
    # records the values the module was initialized with.
    load_message="$(dmesg | awk '/softdog: initialized\./ { line = $0 } END { print line }')"
    if [[ "$load_message" != *" soft_noboot=0 "* || "$load_message" != *" soft_panic=0 "* || "$load_message" != *"(nowayout=0)"* ]]; then
        printf 'Refusing softdog: load message does not show nowayout=0 soft_noboot=0 soft_panic=0: %s\n' "$load_message" >&2
        exit 2
    fi
fi
watchdog_sysfs=''
for candidate in /sys/class/watchdog/watchdog*; do
    if [[ "$(<"$candidate/identity")" == 'Software Watchdog' ]]; then
        watchdog_sysfs="$candidate"
        break
    fi
done
[[ -n "$watchdog_sysfs" ]]
[[ "$(<"$watchdog_sysfs/state")" == inactive ]]
device="/dev/${watchdog_sysfs##*/}"
namespace="gruezi-wd-$$"
directory="$(mktemp -d /var/tmp/gruezi-watchdog.XXXXXX)"
owner=''
cleanup() {
    if [[ -n "$owner" ]] && kill -0 "$owner" 2>/dev/null; then
        kill -CONT "$owner" 2>/dev/null || true
        kill -TERM "$owner" 2>/dev/null || true
        timeout 15 tail --pid="$owner" -f /dev/null || true
    fi
    ip netns del "$namespace" 2>/dev/null || true
    printf 'Watchdog acceptance log retained in %s\n' "$directory"
}
trap cleanup EXIT
ip netns add "$namespace"
ip -n "$namespace" link set lo up
ip -n "$namespace" link add wdtest0 type dummy
ip -n "$namespace" link set wdtest0 up
cat >"$directory/config.yaml.tmp" <<EOF
mode: ha
node: {id: watchdog-lab-owner}
ha:
  failure_policy: available
  peer_id: watchdog-lab-absent-peer
  group_id: watchdog-lab
  bind: 127.0.0.1:9375
  peer: 127.0.0.2:9375
  interface: wdtest0
  addresses: [192.0.2.254/32]
  hold_down_ms: 1000
  jitter_ms: 0
  auth: {key: disposable-watchdog-lab-key}
  watchdog: {mode: required, device: $device, timeout_ms: 15000, safety_margin_ms: 2000, nowayout: reject}
EOF
mv "$directory/config.yaml.tmp" "$directory/config.yaml"
start_owner() {
    ip netns exec "$namespace" "$binary" -v start --config "$directory/config.yaml" >>"$directory/owner.log" 2>&1 &
    owner=$!
    deadline=$((SECONDS + 30))
    until ip netns exec "$namespace" curl -fsS http://127.0.0.1:9376/status >"$directory/status.json" 2>/dev/null &&
        [[ "$(<"$directory/status.json")" == *'"owned":true'* ]]; do
        kill -0 "$owner"
        [[ "$SECONDS" -lt "$deadline" ]]
        sleep 0.1
    done
    [[ "$(ip -n "$namespace" -o address show dev wdtest0)" == *192.0.2.254/32* ]]
    [[ "$(<"$watchdog_sysfs/state")" == active ]]
    printf 'ARMED owner=%s status=%s\n' "$owner" "$(<"$directory/status.json")"
}
start_owner
effective_timeout="$(<"$watchdog_sysfs/timeout")"
deadline=$((SECONDS + effective_timeout + 2))
while [[ "$SECONDS" -lt "$deadline" ]]; do
    kill -0 "$owner"
    ip netns exec "$namespace" curl -fsS http://127.0.0.1:9376/status >"$directory/status.json"
    [[ "$(<"$directory/status.json")" == *'"owned":true'* ]]
    sleep 0.1
done
printf 'FEED verified: owner remained alive and owned beyond W=%ss\n' "$effective_timeout"
kill -TERM "$owner"
deadline=$((SECONDS + 15))
while kill -0 "$owner" 2>/dev/null; do
    [[ "$SECONDS" -lt "$deadline" ]]
    sleep 0.1
done
wait "$owner"
owner=''
[[ "$(ip -n "$namespace" -o address show dev wdtest0)" != *192.0.2.254/32* ]]
[[ "$(<"$watchdog_sysfs/state")" == inactive ]]
printf '%s\n' 'DISARM verified after confirmed VIP absence.'
start_owner
printf 'STALL owner=%s; expect an actual softdog VM reset within W=%ss. Log: %s\n' "$owner" "$effective_timeout" "$directory"
sync
kill -STOP "$owner"
deadline=$((SECONDS + effective_timeout + 10))
while [[ "$SECONDS" -lt "$deadline" ]]; do
    sleep 0.1
done
printf '%s\n' 'FAIL: softdog did not reset the stalled owner VM.' >&2
exit 1
