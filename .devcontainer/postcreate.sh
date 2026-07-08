#!/usr/bin/env bash
set -euo pipefail

# One-time provisioning for the gruezi dev container (DevPod postCreateCommand):
# system deps, a nested rootless podman (so `just test-ha` works inside the
# container), Rust components, and the mise-managed toolchain (just, slick, etc.).
#
# gruezi's core `just test` (clippy + fmt + cargo test) needs nothing external. The
# HA integration target `just test-ha` calls `podman` directly to build a local
# image and run multiple nodes on a bridge network with static IPs; the podman
# stack installed here provides that inside the (privileged) app container.

export PATH="$HOME/.local/bin:$HOME/.local/share/mise/shims:$PATH"

# Absolute repo root, independent of the caller's CWD (postcreate.sh lives in
# .devcontainer/). Used to invoke repo scripts directly instead of via `mise run`,
# which can resolve paths from an unexpected CWD under MISE_CONFIG_FILE.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Apply optional git identity/signing forwarded from the host by scripts/dev-up.
sh "$REPO_ROOT/.devcontainer/configure-git.sh"

# 1. Named-volume mount points are frequently created root-owned by the runtime
#    (especially under the local keep-id flow), and a recursive chown of the parent
#    does not reliably descend into each volume's own mount point. Chown every
#    mounted path *explicitly* so `cargo`/`mise`/nvim can write into them as vscode.
#    Not best-effort: if these are wrong, `cargo build` (and thus `just test-ha`)
#    fails with EACCES, so surface a clear error instead of silently continuing.
sudo mkdir -p \
    "$HOME/.local/bin" "$HOME/.local/share" "$HOME/.cache" "$HOME/.config"
for vol in \
    /home/vscode/.cargo/registry /home/vscode/.cargo/git \
    /home/vscode/.rustup \
    "$HOME/.local/share/mise" "$HOME/.cache/mise" \
    "$HOME/.local/share/nvim" "$HOME/.local/state/nvim" "$HOME/.cache/nvim"; do
    sudo mkdir -p "$vol"
    sudo chown "$(id -u):$(id -g)" "$vol"
done
# Best-effort recursive pass for anything already written into the trees.
sudo chown -R "$(id -u):$(id -g)" \
    "$HOME/.local" "$HOME/.cache" "$HOME/.config" \
    /home/vscode/.cargo /home/vscode/.rustup 2>/dev/null || true

# 2. System dependencies. rustls build (no OpenSSL needed) + the podman stack for
#    nested container workloads (`just test-ha`). zsh is required because gruezi's
#    .justfile sets `shell := ["zsh", "-uc"]`; make it the default login shell too.
sudo apt-get update
sudo apt-get install -y \
    build-essential ca-certificates curl delta dnsutils fd-find fzf git gnupg iputils-ping jq \
    libbz2-dev libcap2-bin libffi-dev liblzma-dev libnss3-tools libreadline-dev libsqlite3-dev \
    libssl-dev luarocks make netcat-openbsd openssh-client pkg-config rsync \
    tmux unzip wget xz-utils yq zip zlib1g-dev zsh \
    podman crun fuse-overlayfs uidmap slirp4netns passt netavark aardvark-dns \
    containernetworking-plugins iptables iproute2 iputils-arping ndisc6

command -v zsh >/dev/null 2>&1 && sudo chsh -s "$(command -v zsh)" vscode || true

# 3. Nested podman for `just test-ha`. Rootless-in-rootless is not possible here:
#    on a rootless host the chain host-podman -> app -> inner-podman nests user
#    namespaces three deep, and the innermost `newuidmap` cannot write uid_map for
#    a range that was never delegated ("Operation not permitted"). The app
#    container is `privileged` (see compose.yaml), though, so ROOTFUL podman inside
#    it works reliably: rootful containers don't nest another userns, and overlay +
#    netavark bridge with static IPs all function.
#
#    gruezi's .justfile calls bare `podman`, so route it through rootful podman with
#    a tiny PATH wrapper. Passwordless sudo is provided by the common-utils feature.
sudo tee /usr/local/bin/podman >/dev/null <<'EOF'
#!/bin/sh
# gruezi devcontainer shim: run podman rootful (nested ROOTLESS podman can't work
# under the host-rootless -> app -> inner triple userns nesting). The app container
# is privileged, so rootful podman is the reliable path for `just test-ha`.
exec sudo -n /usr/bin/podman "$@"
EOF
sudo chmod +x /usr/local/bin/podman

# Persist the rootful podman image/container store on the mounted volume (compose
# mounts `containers-storage` at /var/lib/containers) so `just build-image` /
# `just test-ha` don't re-pull base layers on every container recreate.
sudo mkdir -p /var/lib/containers
sudo chown root:root /var/lib/containers

# 4. mise: installs the toolchain from mise.toml (just, slick, cargo tools, etc.).
#    Be resilient: a single optional tool must not brick the whole workspace. Try
#    the full install, retry once, then fall back to the essentials so `just test`
#    always works.
if ! command -v mise >/dev/null 2>&1; then
    curl -fsSL https://mise.run | sh
fi
mise trust --yes
if ! mise install; then
    echo "mise install failed; retrying once..." >&2
    if ! mise install; then
        echo "mise install still failing; installing essential tools individually." >&2
        # `just` is required for the whole workflow; the rest are best-effort.
        mise install just || true
        mise install || true
    fi
fi
# Remove tools no longer in mise.toml (notably a previously mise-managed `rust`),
# so the interactive shell's cargo/rustc resolve to the image toolchain below
# rather than stale mise shims.
mise prune --yes || true
mise reshim || true

# Ensure the essential tool is actually present (the rest are best-effort).
export PATH="$HOME/.local/bin:$HOME/.local/share/mise/shims:$PATH"
if ! command -v just >/dev/null 2>&1; then
    echo "ERROR: 'just' is not available after mise install; cannot continue." >&2
    exit 1
fi

# 5. Rust is provided by the base image (devcontainers/rust), not mise. Add the
#    components the project needs (clippy/rustfmt/rust-analyzer) to the image
#    toolchain. Use the image's rustup explicitly so this never hits a mise shim.
IMAGE_RUSTUP="$(command -v rustup || echo /usr/local/cargo/bin/rustup)"
case "$IMAGE_RUSTUP" in
*/.local/share/mise/*) IMAGE_RUSTUP=/usr/local/cargo/bin/rustup ;;
esac
"$IMAGE_RUSTUP" component add rustfmt clippy rust-analyzer

# Make the mise shims available to login/non-login shells.
sudo tee /etc/profile.d/mise.sh >/dev/null <<'EOF'
export PATH="$HOME/.local/bin:$HOME/.local/share/mise/shims:$PATH"
EOF
grep -qxF 'export PATH="$HOME/.local/bin:$HOME/.local/share/mise/shims:$PATH"' ~/.bashrc 2>/dev/null ||
    echo 'export PATH="$HOME/.local/bin:$HOME/.local/share/mise/shims:$PATH"' >>~/.bashrc
grep -qxF 'export PATH="$HOME/.local/bin:$HOME/.local/share/mise/shims:$PATH"' ~/.zshenv 2>/dev/null ||
    echo 'export PATH="$HOME/.local/bin:$HOME/.local/share/mise/shims:$PATH"' >>~/.zshenv

# 6. Warm the cargo cache.
cargo fetch || true

# 7. Dotfiles (chezmoi). Opt-in via DEVPOD_DOTFILES (forwarded by scripts/dev-up);
#    defaults to the personal devpod dotfiles repo. Brings in shell config, the
#    slick prompt wiring, zinit, mise activation, nvim config, etc.
#
#    This whole step is best-effort: it must NEVER abort postCreate (which would
#    leave a half-provisioned workspace). We run it in a subshell with `set +e` and
#    retry the chezmoi install, so a transient network hiccup doesn't skip dotfiles.
apply_dotfiles() {
    set +e
    dotfiles_repo="${DEVPOD_DOTFILES:-https://github.com/nbari/dotfiles-devpod.git}"
    [ "$dotfiles_repo" != "" ] || {
        echo "No dotfiles repo configured; skipping."
        return 0
    }

    if ! command -v chezmoi >/dev/null 2>&1 && [ ! -x "$HOME/.local/bin/chezmoi" ]; then
        for attempt in 1 2 3; do
            sh -c "$(curl -fsSL get.chezmoi.io)" -- -b "$HOME/.local/bin" && break
            echo "chezmoi install attempt ${attempt} failed; retrying..." >&2
            sleep 3
        done
    fi

    chezmoi_bin="$(command -v chezmoi || echo "$HOME/.local/bin/chezmoi")"
    if [ ! -x "$chezmoi_bin" ]; then
        echo "chezmoi not available; skipping dotfiles (continuing)." >&2
        return 0
    fi

    "$chezmoi_bin" init --apply --force "$dotfiles_repo" ||
        echo "chezmoi dotfiles apply failed (continuing). Re-run later: chezmoi init --apply --force ${dotfiles_repo}" >&2
}
# Run in a subshell so the `set +e` above stays contained to this best-effort step.
(apply_dotfiles)

# Dotfiles may write git config after the first setup pass. Re-apply the forwarded
# identity/signing config last so commit signing stays stable.
sh "$REPO_ROOT/.devcontainer/configure-git.sh"

echo "✓ postCreate complete: toolchain ready (just, rustfmt, clippy, podman, slick)."
echo "  Core:  just test        (clippy + fmt + cargo test)"
echo "  HA:    just test-ha      (nested podman: builds the image + runs 2 nodes)"
echo "  Trace: just obs-dev      (start Jaeger; UI at http://localhost:16686)"
