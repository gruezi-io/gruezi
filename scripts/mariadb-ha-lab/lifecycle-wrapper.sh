#!/usr/bin/env bash
# Lab-only shim: the Gruezi executor spawns lifecycle commands with no argv,
# while the adapter selects its operation from argv. Forward GRUEZI_EVENT
# unchanged; the adapter re-validates it against its own environment contract.
set -euo pipefail
exec /usr/libexec/gruezi/gruezi-mariadb-adapter "${GRUEZI_EVENT:?GRUEZI_EVENT required}"
