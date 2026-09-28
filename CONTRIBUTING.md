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

A release follows one rule: **the commit that is tagged and put on `main` is exactly the
commit CI tested, and the published files are exactly the files CI built.** `just deploy`
promotes a commit only after every test, build and package passed on it, so a release
never needs a tag deleted or moved. Branch protection lets onto `main` only commits whose
**CI OK** check passed, and the Deploy workflow publishes a tag only with the successful
candidate run the signed tag names. The flow lives in `scripts/release`,
`.github/workflows/build.yml` (Test & Build) and `.github/workflows/release.yml`
(Deploy); it is the flow documented in full in the "Releasing" section of
[cron-when](https://github.com/nbari/cron-when#releasing), which is its template.

Work, including dependency updates (`just update`), lands on `sandbox`. When its
**Test & Build** run is green, merge it into `develop` and run `just deploy` from a clean
`develop`.

| Command | What it does |
|---|---|
| `just deploy` | Release a patch bump (`deploy-minor`, `deploy-major` for the others); when `develop`'s current version has no tag yet, it releases that version as is instead |
| `just deploy-current` | Release `develop`'s untagged version as is, explicitly |
| `just release-status` | Show `develop`, `main`, the staged candidate, its runs and the last tag's publish run |
| `just release-preflight` | Run only the checks; changes nothing apart from fetching |
| `just release-republish X.Y.Z` | Recovery: publish an existing tag again with `main`'s workflow |
| `just protect-branches` | Apply the branch protection the flow relies on |
| `just t-deploy` | Push a `t-*` test tag: tests, builds and packages, publishes nothing |

`just deploy` checks that `develop` is clean and equal to origin, then bumps the version
(`Cargo.toml` and `Cargo.lock` only) in a temporary worktree, runs `just test` there,
and pushes a signed commit "bump version to X" to the scratch `release` branch only. On
that exact commit, Test & Build runs, and so does a manual run of the Deploy workflow in
candidate mode: it tests, builds the archive, RPM and DEB for Linux x86_64 and aarch64
(musl) and the archives for macOS x86_64 and aarch64, packages and verifies the crate, and keeps all of it as artifacts with a
manifest of their SHA-256 sums, publishing nothing. When both pass, the script downloads
the manifest and every artifact and checks the commit, the version and every checksum,
then moves `develop`, `main` and the signed tag X together in one atomic push; the tag
message names the candidate run. The tag's Deploy run builds nothing: its guard checks
the tag (GitHub-verified signature, commit on `main`, version, Test & Build, the named
candidate run), the GitHub release gets exactly the manifest's files with notes GitHub
generates, and the crate goes
to crates.io, repackaged from the tag with the candidate's toolchain and uploaded only
when its checksum matches the manifest. The release is marked Latest only when its tag is
the highest promoted release at that moment.

If anything fails before the atomic push, nothing moved: no tag, `develop` and `main`
untouched. Re-run the failed jobs in GitHub (a waiting deploy picks the re-run up within
15 minutes), or fix it on `sandbox` and merge; then run `just deploy` again, which
resumes the same candidate and runs, replaces a stale one, or says there is nothing new
to release. A failed publish step in the tag's run is fixed with "Re-run failed jobs";
when the tagged workflow itself was wrong, fix it, release as usual, and run
`just release-republish X.Y.Z`. Recovery needs the candidate's artifacts, which GitHub
keeps for 90 days.

`RELEASE_POLL_SECONDS` (30), `RELEASE_CI_TIMEOUT` (3600, per attempt) and
`RELEASE_RERUN_WAIT` (900, after a failed attempt) are in seconds, and
`RELEASE_NO_WAIT=1` stops once the candidate is staged, or while CI still runs; a later
`just deploy` finishes. Releasing needs `just`, `jq`, `gh` (logged in), `cargo-edit`,
and a signing key that GitHub knows as a signing key, since commits and tags must be
signed.

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
