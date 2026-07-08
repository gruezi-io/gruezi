# gruezi DevPod / Dev Containers workspace

A **portable contributor setup** based on [DevPod](https://devpod.sh) and the
[Dev Containers](https://containers.dev) spec. The default `scripts/dev-up` path is
optimized for a local rootless podman/Docker host, with Linux Atomic
(e.g. fedora-atomic) as the main target. A portable config is also provided for
Docker/remote providers that should not load local-only Podman and 1Password mounts.

## Design: one self-contained dev container

The devcontainer is **Docker Compose-based**. A single compose project
(`compose.yaml`) defines the **app** dev container. gruezi's core workflow
(`just test` = clippy + fmt + cargo test) needs **no external service**, so a plain
`devpod up` brings up just `app`.

```
   host: podman (or docker) + devpod + docker compose v2
     │  devpod up .
     ▼
 ┌──────────────── compose project "gruezi" ────────────────┐
 │  app  (service "app", user vscode, PRIVILEGED)           │
 │   ├─ rust + mise (just, slick, cargo-edit, grcov, ...)   │
 │   └─ nested rootful podman   ← for `just test-ha`        │
 │        └─ gruezi-ha-a / gruezi-ha-b on a bridge net      │
 │                                                          │
 │  jaeger  (OTLP, "observability" profile, on-demand)      │
 └──────────────────────────────────────────────────────────┘
```

### Why nested podman (podman-in-podman)

gruezi's `just test-ha` target builds a local image (`Containerfile`) and runs
multiple nodes via `podman`, on a user-defined bridge with **static IPs** and
`--cap-add NET_ADMIN`, bind-mounting `examples/*.yaml` by their in-repo path
(`{{root}}/examples/...`).

To make that work **inside** the dev container, `app` runs its **own** podman
rather than forwarding the host's socket. This matters because:

- **Paths just work.** The inner podman shares the app container's filesystem, so
  `{{root}}/examples/ha-node-a.yaml` resolves natively. A forwarded host socket
  would look for those files on the *host* path and fail.
- **Self-contained.** No host path matching, no `DOCKER_HOST`/`CONTAINER_HOST`
  juggling — identical on Linux, macOS, and Atomic.

The inner podman runs **rootful**. Nested *rootless* podman is impossible on a
rootless host: the chain host-podman → app → inner-podman nests user namespaces
three deep, and the innermost `newuidmap` cannot write a uid_map for a range that
was never delegated (`Operation not permitted`). Since `app` is **privileged**,
rootful podman inside it works reliably — rootful containers don't nest another
userns. `postcreate.sh` installs a tiny `podman → sudo podman` PATH shim so the
justfile's bare `podman` calls transparently run rootful.

The tradeoff: `app` is a **privileged** container (see `compose.yaml`). This is a
**local/dev only** trust boundary — never reuse the stack outside a disposable dev
machine.

## Files

| File | Purpose |
| --- | --- |
| `compose.yaml` | The stack: `app` (nested podman) + `jaeger` (observability profile). |
| `compose.podman.yaml` | Local override: `userns_mode: keep-id` (editable workspace) + host 1Password agent. |
| `devcontainer.json` | Local compose-based devcontainer (`compose.yaml` + podman override). |
| `devcontainer.portable.json` | Portable compose-based devcontainer (`compose.yaml` only). |
| `postcreate.sh` | One-time provisioning: system deps, podman stack, rustup components, `mise install`, dotfiles. |
| `post-start.sh` | Every start: re-apply git identity, print readiness hint. |
| `configure-git.sh` | Apply forwarded git identity / SSH commit signing. |
| `../scripts/dev-up` | Host helper: `devpod up` with the right flags. |
| `../scripts/dev-ssh` | Host helper: shell/run a command in `/workspaces/gruezi`. |
| `../scripts/obs-dev` | Host helper: start/stop the on-demand Jaeger sibling. |

The toolchain is declared in [`../mise.toml`](../mise.toml) (just, the
[slick](https://github.com/nbari/slick) prompt, cargo-edit, cargo-watch, grcov,
cargo-deb, cargo-generate-rpm, tree-sitter). Neovim is installed via a devcontainer
feature, and `postcreate.sh` applies your dotfiles with
[chezmoi](https://chezmoi.io) (repo from `DEVPOD_DOTFILES`, default
`https://github.com/nbari/dotfiles-devpod.git`). `zsh` is the default shell — the
`.justfile` requires it (`shell := ["zsh", "-uc"]`).

## Usage

### Local (Linux / macOS / fedora-atomic)

```bash
scripts/dev-up               # build + start the app container, exec-ready
scripts/dev-ssh              # shell in as vscode, in /workspaces/gruezi
# inside the container:
just test                    # clippy + fmt + cargo test
just test-ha                 # nested podman: build the image + run 2 HA nodes
```

`scripts/dev-up` runs `devpod up . --ide none --id gruezi --ssh-config
"$HOME/.ssh/devpod"`, forwards your git identity and optional `DEVPOD_DOTFILES`,
and uses the host 1Password SSH agent when present. Plain `devpod up .` works too
for the local config.

If you manage identity in `.envrc`, load it before starting DevPod:

```bash
export GIT_USER_NAME="nbari"
export GIT_USER_EMAIL="nbari@tequila.io"
export GIT_SIGNING_KEY="ssh-ed25519 ..."
scripts/dev-up
```

Or in VS Code: **Dev Containers: Reopen in Container**.

#### SSH config (`--ssh-config`)

DevPod writes its managed SSH host entries to a **dedicated file** (`~/.ssh/devpod`)
instead of editing your main `~/.ssh/config`. Add this line **once** to
`~/.ssh/config` so `ssh` and VS Code Remote-SSH can resolve the DevPod hosts:

```
Include ~/.ssh/devpod
```

Override the path with `DEVPOD_SSH_CONFIG=/path/to/file scripts/dev-up`. Use
`scripts/dev-ssh` or `devpod ssh gruezi --workdir /workspaces/gruezi` to enter.

> **Local-focused.** This config targets a local container runtime (rootless
> podman / Docker Desktop). `compose.podman.yaml` applies `userns_mode: keep-id`
> and binds the host 1Password agent, which are local-only. Use
> `devcontainer.portable.json` for a remote (docker) provider.

### Portable / remote provider

```bash
devpod up . --devcontainer-path .devcontainer/devcontainer.portable.json \
  --ide none --id gruezi
devpod ssh gruezi --workdir /workspaces/gruezi
```

The portable config omits `compose.podman.yaml` (keep-id + 1Password). Note the
nested-podman `just test-ha` path relies on the app container being **privileged**
with `/dev/fuse`; a remote provider must permit that for HA tests to run.

## Running the HA lab (`just test-ha`)

Inside the container, the existing justfile targets work unchanged — they now run
against the **nested** podman:

```bash
just test-ha        # setup-network + build-image + run gruezi-ha-a / gruezi-ha-b
just status-ha      # cargo run -- status --node 127.0.0.1:19376 (and :29376)
just logs-ha        # podman logs for both nodes
just stop-ha        # tear the nodes down
```

The HA node APIs are published on the app container at `19376` / `29376`, forwarded
to the host by DevPod, so `just status-ha` works both inside the container and from
the host.

### If nested podman misbehaves

The nested (rootful) podman needs the privileged app container from `compose.yaml`
and the `podman → sudo podman` shim installed by `postcreate.sh`. If `just test-ha`
fails, verify the environment from inside the container:

```bash
which podman            # -> /usr/local/bin/podman (the sudo shim)
podman info | grep -Ei 'graphDriverName'   # expect overlay
sudo podman run --rm docker.io/library/busybox echo ok   # nested run smoke test
```

If a fresh workspace ever fails `cargo build` under `just test-ha` with a
permission error on `~/.cargo/registry`, a named-volume mount point was created
root-owned; re-run the ownership fix: `sudo chown -R vscode:vscode ~/.cargo`.

As a fallback you can always run `just test-ha` on the **host** (outside the
container); the target is host/podman-native.

## Tracing (Jaeger) — on-demand

gruezi enables OpenTelemetry tracing only when `OTEL_EXPORTER_OTLP_ENDPOINT` is set
(gRPC/OTLP). Jaeger is defined in `compose.yaml` behind the **`observability`
profile**, so a plain `devpod up` stays lean — it is **not** started by default.

```bash
# on the host: bring up Jaeger (joins the devcontainer network)
just obs-dev             # -> Jaeger UI http://localhost:16686
just obs-dev-stop        # stop it when done

# inside the container: point gruezi at the collector and run it
export OTEL_EXPORTER_OTLP_ENDPOINT=http://jaeger:4317
cargo run -- start --config examples/ha-node-a.yaml
```

Jaeger scrapes nothing — gruezi **pushes** OTLP spans to `jaeger:4317` over the
shared compose network (no host-gateway or `127.0.0.1` tricks), so it behaves
identically on Linux, macOS, and fedora-atomic.

## Notes

- **Multi-arch:** the base images (`devcontainers/rust`, `jaegertracing/all-in-one`)
  are multi-arch, so Apple Silicon (arm64) and Linux (amd64/arm64) work natively.
- **cargo target dir:** kept at the default (`target/` inside the workspace) on
  purpose — gruezi's `Containerfile` does `COPY target/debug/gruezi`, so redirecting
  `CARGO_TARGET_DIR` would break `just build-image` / `just test-ha`.
