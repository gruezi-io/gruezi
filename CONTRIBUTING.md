# Contributing

This repository is a Rust CLI crate for `gruezi`, with the current implementation focus on `mode: ha` and the longer-term direction covering `mode: kv`.

Start with these files:

* [README.md](README.md): product direction, roadmap, protocol notes, and operational docs
* [AGENTS.md](AGENTS.md): repository-specific engineering rules and testing expectations
* [ansible/README.md](ansible/README.md): HA lab deployment workflow
* [contrib/README.md](contrib/README.md): package-oriented `.deb` and `.rpm` assets

## Development setup

You need a working Rust toolchain and standard Cargo workflow.

Common commands:

```bash
cargo check
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets --all-features
```

There is also a `just`-based workflow:

```bash
just test
just coverage
just test-ha
```

## Repository layout

Key paths:

* `src/bin/gruezi.rs`: CLI entrypoint
* `src/lib.rs`: library entrypoint
* `src/cli/`: CLI parsing, dispatch, telemetry, and command actions
* `src/config.rs`: YAML config parsing and validation
* `src/gruezi/`: runtime code, including HA behavior
* `ansible/`: HA lab deployment workflow
* `contrib/`: package and service-management assets
* `.github/workflows/`: CI and release automation

## Coding expectations

Follow standard Rust style and keep changes small and coherent.

Expected conventions:

* use `snake_case` for functions and modules
* use `CamelCase` for types and enums
* keep imports compact and grouped when reasonable
* keep modules focused by responsibility
* prefer clear state-machine and protocol code over clever abstractions

This repository enforces strict lints:

* warnings are denied
* `clippy::pedantic` is enabled
* `unwrap`, `expect`, `panic!`, and unchecked indexing are not acceptable

If a lint is failing, refactor the code rather than adding local `#[allow(...)]` exceptions unless there is an explicit reason to do so.

## Testing requirements

Before opening a PR for a code change, run:

```bash
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets --all-features
```

If you change:

* CLI behavior: include example output or help text changes in the PR
* config parsing: cover both valid and invalid inputs
* HA logic: cover state transitions, packet handling, timer behavior, or shutdown/failure paths
* docs or examples: keep commands, ports, and config snippets aligned with the implementation

Tests should stay close to the code they verify. Prefer `#[cfg(test)]` unit tests in the relevant module or adjacent integration tests where that gives better coverage.

## HA work guidance

Current HA defaults and expectations are documented in [README.md](README.md).

When working on HA:

* preserve the `9375/udp` HA peer port and `9376/tcp` management API split unless the docs and code change together
* keep behavior observable through logs, hooks, and `gruezi status`
* make promotions, demotions, and VIP moves explainable from runtime output, not just inferable from packet captures
* favor deterministic and conservative failover behavior over aggressive promotion
* validate graceful shutdown, VIP cleanup, and failover timing when touching the HA runtime

For lab validation, use:

```bash
cd ansible
ansible-playbook deploy-ha-lab.yml
```

For local container-based HA testing, use:

```bash
just test-ha
```

## Packaging and deployment

The repository supports two operational paths today:

* direct HA lab deployment via Ansible in `ansible/`
* package-oriented assets in `contrib/` for future `.deb` and `.rpm` installs

Package build commands:

```bash
just package-deb
just package-rpm
```

These rely on:

* `cargo deb`
* `cargo generate-rpm`

The packaged systemd unit expects:

* `/etc/gruezi/gruezi.yaml`
* `/etc/gruezi/gruezi.env`

## Commits and pull requests

Keep commit subjects short, imperative, and scoped to one change.

Good examples:

* `fix ha shutdown cleanup`
* `add deb packaging assets`
* `document ansible lab workflow`

PRs should include:

* a short summary of what changed
* any linked issue or context, if applicable
* the verification commands you ran
* sample output when CLI behavior, status output, or logs changed

If a change is intentionally incomplete, say so clearly and describe the remaining risk or next step.

## Documentation

This repository uses the README as a living design and operations document.

If you change behavior, update the relevant docs at the same time:

* `README.md` for user-facing behavior, protocol notes, and roadmap status
* `ansible/README.md` for deployment changes
* `contrib/README.md` for packaging changes
