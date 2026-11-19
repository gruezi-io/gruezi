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

{
    printf 'ARGV:'
    for arg in "$@"; do
        printf ' <%s>' "$arg"
    done
    printf '\n'
    printf 'STDIN:\n'
    cat
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

output="$fix/out.$base"
if [[ ! -f "$output" ]]; then
    printf 'fake mariadb: no fixture output named out.%s\n' "$base" >&2
    exit 1
fi
cat "$output"
