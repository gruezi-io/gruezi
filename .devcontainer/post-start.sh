#!/usr/bin/env bash
set -uo pipefail

# Runs on every container start (DevPod postStartCommand). Best-effort: it must not
# fail `devpod up`. Re-applies forwarded git identity and prints a short readiness
# hint. gruezi needs no external service for `just test`, so there is nothing to
# seed here — the podman stack for `just test-ha` is provisioned once in postcreate.

export PATH="$HOME/.local/bin:$HOME/.local/share/mise/shims:$PATH"
cd /workspaces/gruezi 2>/dev/null || exit 0

# Re-apply optional git identity/signing on every start so updates to forwarded
# DevPod workspace env are reflected without rebuilding the container.
sh .devcontainer/configure-git.sh || true

echo "✓ Workspace ready."
echo "  Run: just test        (clippy + fmt + cargo test)"
echo "       just test-ha     (nested podman HA lab: 2 nodes)"

exit 0
