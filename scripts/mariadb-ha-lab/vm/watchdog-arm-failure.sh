#!/usr/bin/env bash
# Standalone required-watchdog refusal probe, never the deployed service config.
set -euo pipefail
namespace="gruezi-arm-$$"
directory="$(mktemp -d)"
owner='' created=0
cleanup() {
    if [[ -n "$owner" ]]; then
        kill -TERM "$owner" 2>/dev/null || true
        wait "$owner" || true
    fi
    ((created == 0)) || ip netns del "$namespace"
    rm -rf -- "$directory"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
command -v jq >/dev/null
[[ ! -e /dev/gruezi-missing-watchdog ]]
ip netns add "$namespace"
created=1
ip -n "$namespace" link set lo up
ip -n "$namespace" link add arm0 type dummy
ip -n "$namespace" link set arm0 up
cat >"$directory/config.yaml" <<'EOF'
mode: ha
node: {id: arm-probe}
ha:
  failure_policy: available
  peer_id: absent
  group_id: disposable-arm-probe
  bind: 127.0.0.1:9375
  peer: 127.0.0.2:9375
  interface: arm0
  hold_down_ms: 100
  jitter_ms: 0
  addresses: [192.0.2.254/32]
  auth: {key: disposable-arm-probe-key}
  watchdog: {mode: required, device: /dev/gruezi-missing-watchdog, timeout_ms: 15000, safety_margin_ms: 2000, nowayout: reject}
EOF
ip netns exec "$namespace" /usr/local/bin/gruezi start --config "$directory/config.yaml" >"$directory/log" 2>&1 &
owner=$!
deadline=$((SECONDS+30))
until ip netns exec "$namespace" curl --max-time 1 -fsS http://127.0.0.1:9376/status >"$directory/status" 2>/dev/null &&
    jq -e '.ha | (.owned == false) and (.watchdog.mode == "required") and (.watchdog.armed == false) and (.resource.last_error | strings | contains("watchdog open"))' "$directory/status"; do
    kill -0 "$owner"
    [[ "$SECONDS" -lt "$deadline" ]]
    sleep 0.1
done
cat "$directory/status"
cat "$directory/log"
[[ "$(ip -n "$namespace" -o address show dev arm0)" != *192.0.2.254/32* ]]
