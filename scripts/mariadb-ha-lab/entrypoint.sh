#!/usr/bin/env bash
set -euo pipefail
if [[ -n "${LAB_NODE:-}" ]]; then
    # /lab stays 0700 container-root-owned. Only root copies credentials across
    # this boundary; the lifecycle child never needs access to the bind mount.
    install -d -m 700 -o gruezi-actuator -g gruezi-actuator \
        /etc/gruezi/mariadb /var/lib/gruezi/mariadb-adapter
    for file in adapter.conf adapter-local.cnf adapter-peer.cnf; do
        install -m 600 -o gruezi-actuator -g gruezi-actuator \
            "/lab/strict-$LAB_NODE/$file" "/etc/gruezi/mariadb/$file"
    done
fi
exec /usr/local/bin/gruezi "$@"
