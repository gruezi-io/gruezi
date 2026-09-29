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

For Linux HA failure-injection coverage:

```bash
cargo build --release --locked
sudo ./scripts/test-ha-netem.sh all
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

For Linux namespace and `tc netem` impairment testing, use:

```bash
sudo ./scripts/test-ha-netem.sh all
```

This covers delay, asymmetric loss, full partition, and heal-after-partition behavior.

When changing HA election or failover behavior, also compare the resulting operator behavior to the keepalived guidance in [README.md](README.md):

* match priority and advertisement timing as closely as practical
* verify `preempt: false` against keepalived `nopreempt`
* compare takeover and failback outcomes, not packet formats
* document intentional differences, especially around asymmetric loss handling

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

## Releasing

A release is one command, `just deploy`, and follows one rule: **the commit that is
tagged and put on `main` is exactly the commit CI tested, and everything published is
exactly what CI built.** The version bump is staged on the scratch `release` branch,
Test & Build and a candidate run of the Release workflow (`.github/workflows/release.yml`)
test and package that exact commit, and only then do `develop`, `main` and the signed
tag move together in one atomic push. The tag's run builds nothing: it publishes the
candidate's files and crate. A failure before the tag leaves nothing to clean up, and a
release never needs a tag deleted or moved. The flow comes from
[cron-when](https://github.com/nbari/cron-when), whose
[RELEASING.md](https://github.com/nbari/cron-when/blob/main/RELEASING.md) explains it
in full: the diagrams, every failure and what to do, the guarantees and limits, and how
to verify a download.

Work, including dependency updates (`just update`), lands on `sandbox`. When its
**Test & Build** run is green, merge it into `develop` and run `just deploy` from a clean
`develop`; you can keep working on `sandbox` meanwhile.

| Command | What it does |
|---|---|
| `just deploy` | Release a patch version (`deploy-minor`, `deploy-major` for the others); when `develop` already carries an untagged version, that version is released as is |
| `just deploy-current` | Release `develop`'s untagged version as is, explicitly |
| `just release-status` | Show `develop`, `main`, `sandbox`, the staged candidate and its runs |
| `just release-preflight` | Run only the checks; changes nothing that lasts |
| `just release-dry-run` | Build and package the current branch exactly like a candidate, releasing nothing: no bump, no tag |
| `just release-republish X.Y.Z` | Recovery: publish an existing tag again with `main`'s workflow |
| `just protect-branches` | Apply the branch protection (**CI OK** on `main`, signed commits, linear history and resolved review conversations) and the rule that release tags are never moved or deleted |

The candidate run builds the archive, RPM and DEB for Linux x86_64 and aarch64 (musl) and the archives for macOS x86_64 and aarch64, packages and verifies the crate, keeps everything
with a manifest of SHA-256 sums, and attests the build provenance of every file. The tag
run publishes those files to the GitHub release with notes made from the commit
subjects since the previous release, marks it Latest only while it is the highest
release, and uploads the crate with crates.io
[Trusted Publishing](https://crates.io/docs/trusted-publishing) (no stored token),
only when the repackaged crate has the tested checksum. To check a download:

```sh
sha256sum --check --ignore-missing SHA256SUMS
gh attestation verify <file> --repo gruezi-io/gruezi --signer-workflow gruezi-io/gruezi/.github/workflows/release.yml
```

If anything fails before promotion, nothing moved: re-run the failed jobs in GitHub, or
fix it on `sandbox` and merge, then run `just deploy` again; it resumes the same
candidate. A failed publish step is finished by "Re-run failed jobs" on the tag run,
within GitHub's 30-day window, or later with `just release-republish X.Y.Z` while the
candidate's artifacts are kept (90 days). Releasing needs `just`, `jq`, `gh` (logged
in), git 2.31 or later, `cargo-edit`, and a signing key that GitHub knows as a signing
key, since commits and tags must be signed.
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
