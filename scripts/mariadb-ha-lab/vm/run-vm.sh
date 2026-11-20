#!/usr/bin/env bash
# No discovery, deployment, arbitrary commands, or implicit scenario selection.
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
source "$root/scripts/mariadb-ha-lab/vm/guard.sh"
rows=(kernel-sigkill watchdog-reset watchdog-arm-failure firewall-block asymmetric-loss)
inventory='' row='' target='' output='' mode=run
while (($#)); do
    case "$1" in
        --list|--dry-run)
            [[ "$mode" == run ]] || deny 'conflicting modes'
            mode="${1#--}"; shift ;;
        --inventory|--row|--target|--output)
            (($# >= 2)) || deny 'missing option value'
            case "$1" in
                --inventory) [[ -z "$inventory" ]] || deny 'duplicate inventory'; inventory="$2" ;;
                --row) [[ -z "$row" ]] || deny 'duplicate row'; row="$2" ;;
                --target) [[ -z "$target" ]] || deny 'duplicate target'; target="$2" ;;
                --output) [[ -z "$output" ]] || deny 'duplicate output'; output="$2" ;;
            esac
            shift 2 ;;
        *) deny "unsupported option: $1" ;;
    esac
done
# Env gates precede even inventory parsing for actual operations.
if [[ "$mode" == run ]]; then
    [[ "${GRUEZI_ALLOW_VM_FAULTS:-}" == 1 ]] || deny 'GRUEZI_ALLOW_VM_FAULTS=1 required'
fi
[[ -n "$inventory" ]] || deny '--inventory is required'
read_inventory "$inventory"
read_allowlist
case "$row" in
    ''|kernel-sigkill|watchdog-reset|watchdog-arm-failure|firewall-block|asymmetric-loss) ;;
    *) deny 'unknown row' ;;
esac
if [[ -n "$target" && "$target" != "${names[0]}" && "$target" != "${names[1]}" ]]; then
    deny 'target must name one inventory host'
fi
if [[ "$mode" == run ]]; then
    [[ -n "$row" && -n "$target" ]] || deny '--row and --target required; no default fault'
    [[ "$output" =~ ^/[a-zA-Z0-9_./-]+$ && ! -e "$output" ]] ||
        deny '--output requires a new absolute directory (letters, digits, slash, dot, underscore, hyphen)'
    if [[ "$row" == watchdog-reset ]]; then
        [[ "${GRUEZI_ALLOW_WATCHDOG_RESET:-}" == 1 ]] || deny 'GRUEZI_ALLOW_WATCHDOG_RESET=1 required'
    fi
fi
for planned in "${rows[@]}"; do
    gate='GRUEZI_ALLOW_VM_FAULTS=1'
    [[ "$planned" != watchdog-reset ]] || gate+=' GRUEZI_ALLOW_WATCHDOG_RESET=1'
    printf '%s\tNOT RUN\trequires=%s reason=%s\n' "$planned" "$gate" \
        "$([[ "$mode" == run && "$row" == "$planned" ]] && printf pending || printf 'not selected or planning only')"
done
[[ "$mode" == run ]] || exit 0
# All commands below this point operate on parsed allowlisted values only.
source "$root/scripts/mariadb-ha-lab/vm/operations.sh"
