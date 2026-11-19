#!/usr/bin/env bash
set -euo pipefail

# Recommended application write-fencing pattern: removing a VIP alone is NOT
# write fencing. This notification hook cannot fence a crashed/frozen process,
# privileged DB users, or two owners under the available failure policy.
# Set read_only=1 before starting Gruezi; on_backup also covers initial standby.
: "${GRUEZI_EVENT:?Gruezi must supply GRUEZI_EVENT}"
: "${GRUEZI_NODE_ID:?Gruezi must supply GRUEZI_NODE_ID}"
: "${GRUEZI_GROUP_ID:?Gruezi must supply GRUEZI_GROUP_ID}"
case "$GRUEZI_EVENT" in
    promote) value=0 ;;
    demote|backup|fault) value=1 ;;
    *) printf 'Unsupported GRUEZI_EVENT: %s\n' "$GRUEZI_EVENT" >&2; exit 2 ;;
esac
# A protected defaults file keeps credentials out of process arguments. In the
# pod lab the local server shares the network namespace with this hook.
timeout --signal=KILL 3 mariadb \
    --defaults-extra-file="${MARIADB_HOOK_DEFAULTS:-/lab/db-client.cnf}" \
    --protocol=tcp --host=127.0.0.1 --connect-timeout=1 \
    --execute="SET GLOBAL read_only=$value"
printf 'node=%s group=%s event=%s read_only=%s\n' \
    "$GRUEZI_NODE_ID" "$GRUEZI_GROUP_ID" "$GRUEZI_EVENT" "$value"
