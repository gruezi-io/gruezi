#!/usr/bin/env bash
# Run the complete existing adapter suite with host mawk and container gawk.
set -eEuo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
ROOT="$(cd "$HERE/../.." && pwd -P)"
IMAGE=localhost/gruezi-mariadb-lab:portability
CONTAINER=gruezi-mariadb-lab-awk-test
WORK="$(mktemp -d)"
cleanup() {
    local code=$?
    trap - EXIT
    if timeout 45 podman container exists "$CONTAINER"; then
        timeout 45 podman rm -f "$CONTAINER" || code=1
    fi
    if timeout 45 podman image exists "$IMAGE"; then
        timeout 45 podman image rm "$IMAGE" || code=1
    fi
    printf 'RECEIPT portability container=%s image=%s removed\n' "$CONTAINER" "$IMAGE"
    rm -rf "$WORK"
    exit "$code"
}
# Existing resources require explicit cleanup by their owner, never reuse.
if timeout 45 podman container exists "$CONTAINER" || timeout 45 podman image exists "$IMAGE"; then
    printf 'Portability resources exist; refusing to modify them\n' >&2
    rm -rf "$WORK"
    exit 1
fi
trap cleanup EXIT
cd "$ROOT"
timeout 600 cargo test --test mariadb_adapter --no-run 2>&1 | tee "$WORK/build.log"
binary="$(sed -n 's/^.*Executable .* (\(.*\))$/\1/p' "$WORK/build.log")"
[[ -n "$binary" && -x "$binary" ]]
case "$binary" in /*) ;; *) binary="$ROOT/$binary" ;; esac
printf 'PORTABILITY host: '; awk -W version 2>&1 | head -n 1
timeout 300 "$binary"
timeout 600 podman build -f "$HERE/Containerfile" -t "$IMAGE" "$ROOT"
timeout 300 podman run --name "$CONTAINER" --user 991:991 \
    --entrypoint /bin/bash \
    -v "$binary:/tmp/adapter-suite:ro,z" \
    -v "$ROOT/contrib/mariadb/gruezi-mariadb-adapter:$ROOT/contrib/mariadb/gruezi-mariadb-adapter:ro,z" \
    "$IMAGE" -c 'printf "PORTABILITY container: "; awk -W version | head -n 1; /tmp/adapter-suite; code=$?; exit "$code"'
printf 'PASS complete_adapter_suite_host_mawk_and_container_gawk\n'
