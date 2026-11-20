#!/usr/bin/env bash
# This transport consumes Ansible's module bytes as data. It NEVER executes them
# or invokes SSH, iptables, tc, sudo, or a remote interpreter.
set -euo pipefail
directory="$(cd -- "$(dirname -- "$0")" && pwd)"
payload="$(cat)"
kind="$(<"$directory/kind")"
scenario="$(<"$directory/scenario")"
operation=diagnostic
if [[ "$payload" == *-D* || "$payload" == *'\"del\"'* || "$payload" == *'"del"'* ]]; then
    operation=delete
elif [[ "$payload" == *-S* || "$payload" == *qdisc* ]]; then
    operation=query
fi
code=0 failed=false stderr='' lines='[]'
if [[ "$operation" == delete ]]; then
    if [[ "$scenario" == delete-error || "$scenario" == known-absent ]]; then
        code=42 failed=true stderr='FAKE deletion rc=42'
    else
        touch "$directory/deleted"
    fi
fi
if [[ "$operation" == query &&
    ( "$scenario" == observation-error ||
      ( "$scenario" == after-observation-error && -f "$directory/deleted" ) ) ]]; then
    code=43 failed=true stderr='FAKE observation rc=43'
fi
if [[ "$scenario" == malformed ||
    ( "$scenario" == after-malformed && -f "$directory/deleted" ) ]]; then
    stdout='FAKE unparseable observation'
    lines='["FAKE unparseable observation"]'
elif [[ "$scenario" == shape-error ||
    ( "$scenario" == after-shape-error && -f "$directory/deleted" ) ]]; then
    if [[ "$kind" == drop ]]; then
        stdout='-P OUTPUT ACCEPT\nFAKE incomplete rule'
        lines='["-P OUTPUT ACCEPT","FAKE incomplete rule"]'
    else
        stdout='[{\"kind\":\"noqueue\"}]'
    fi
elif [[ "$kind" == drop ]]; then
    stdout='-P OUTPUT ACCEPT'
    lines='["-P OUTPUT ACCEPT"]'
    if [[ "$scenario" != known-absent &&
        ( ! -f "$directory/deleted" || "$scenario" == persistent ) ]]; then
        stdout+='\n-A OUTPUT -d 10.88.0.11/32 -p udp -m udp --dport 9375 -m comment --comment gruezi-ha-drop -j DROP'
        lines='["-P OUTPUT ACCEPT","-A OUTPUT -d 10.88.0.11/32 -p udp -m udp --dport 9375 -m comment --comment gruezi-ha-drop -j DROP"]'
    fi
else
    stdout='[{\"kind\":\"noqueue\",\"handle\":\"0:\",\"root\":true}]'
    if [[ "$scenario" != known-absent &&
        ( ! -f "$directory/deleted" || "$scenario" == persistent ) ]]; then
        stdout='[{\"kind\":\"prio\",\"handle\":\"1:\",\"root\":true},{\"kind\":\"netem\",\"handle\":\"30:\",\"parent\":\"1:3\"}]'
    fi
fi
printf '%s rc=%s\n' "$operation" "$code" >>"$directory/transport.log"
printf '{"changed":false,"rc":%s,"stdout":"%s","stdout_lines":%s,"stderr":"%s","failed":%s}\n' \
    "$code" "$stdout" "$lines" "$stderr" "$failed"
