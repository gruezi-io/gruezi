# Releasing Gruezi

This runbook is for maintainers publishing a Gruezi version. It describes the
selected release contract. The documented flow and recovery cases are covered
by the release process fixture tests.

## Release contract

The version is prepared on `develop`, then one exact `develop` commit is
published. `just release` never changes the version, creates a commit, pushes
`develop`, or synchronizes another branch.

The release workflow:

1. Checks out the full SHA supplied by `just release`.
2. Requires `develop`, that exact SHA, a canonical `X.Y.Z` version, an
   untagged and numerically newer version, `main` ancestry, and the successful
   `CI OK` job from this repository's `build.yml` push run.
3. Resolves one exact Rust version and reuses it for every release job.
4. Builds and checks native Linux musl packages for x86_64 and aarch64.
5. Packages the crate and runs `cargo publish --dry-run --locked`.
6. Attests the release files and crate, creates the GitHub release, and
   verifies its tag, names, and downloaded bytes.
7. Verifies or publishes the same crate checksum on crates.io.

GitHub creates an unsigned release tag. Publication happens before the local
recipe advances `main` to the same SHA. This is not an atomic branch and tag
promotion. Existing signed commits and remote branch and tag protections remain
in force. The release workflow remains
`.github/workflows/release.yml` in `gruezi-io/gruezi`; crates.io uses its
existing Trusted Publishing OIDC action, `id-token: write`, and no stored
registry secret or environment binding.

## Prerequisites

Use a clean checkout on `develop`. Install and authenticate:

* Rust and Cargo, including the repository's `cargo set-version` command.
* `just`, `git`, `gh`, `jq`, and `sha256sum`.
* A Git signing key configured for signed commits.
* `gh auth status` showing access to `gruezi-io/gruezi` and workflow dispatch.

Do not change branch protection, tag protection, OIDC settings, or registry
settings as part of a release. Do not run these commands against the production
repository as a test.

## Normal flow

Work normally, review the result, and push the work to `develop`. The version
bump is an explicit final change on `develop`:

```sh
just bump
just bump-minor
just bump-major
```

Each command requires `develop`, a clean worktree, a usable signer, and a
nondivergent `origin/develop`. It refreshes dependencies with `cargo update`,
runs `just test`, signs a commit containing only `Cargo.toml` and `Cargo.lock`,
and pushes that commit to `develop`. Cargo writes the version and lockfile.

The three commands mean patch, minor, and major release respectively. A bump
failure before the commit restores only the version files changed by that
invocation. A signed commit remains local if its push is rejected. Fix the
cause and retry the printed push command. Never run another bump to recover a
rejected push.

After the pushed commit has its ordinary `build.yml` run and `CI OK`, run:

```sh
just release
```

The recipe fetches `origin/develop` and `origin/main`, records the full selected
SHA, creates a unique dispatch marker, dispatches `release.yml`, finds only that
invocation, and watches it. It then pushes exactly that SHA to `main` after
publication and the final checks pass.
It does not promise that ordinary CI and release binaries are byte-identical.
The release run resolves and reuses its own exact Rust compiler.

## Downloads and verification

For version `V`, GitHub has exactly these seven files:

```text
gruezi-V-x86_64-unknown-linux-musl.tar.gz
gruezi-V-aarch64-unknown-linux-musl.tar.gz
gruezi_V-1_amd64.deb
gruezi_V-1_arm64.deb
gruezi-V-1.x86_64.rpm
gruezi-V-1.aarch64.rpm
SHA256SUMS
```

The crate, `gruezi-V.crate`, is verified separately and is not a GitHub release
asset. Download the GitHub files and verify them from the directory containing
`SHA256SUMS`:

```sh
sha256sum --check --ignore-missing SHA256SUMS
gh attestation verify <file> \
  --repo gruezi-io/gruezi \
  --signer-workflow gruezi-io/gruezi/.github/workflows/release.yml
```

Run attestation verification for each downloaded payload. A checksum mismatch
is a failure. Do not replace, delete, or move a public tag or public bytes.
Provenance covers the six GitHub payloads and the separately verified crate.
Release artifacts are retained for 90 days.

## Recovery

Record the release version, full SHA, workflow run ID, run URL, and dispatch
marker as soon as `just release` prints them. Recovery always uses that same
run and SHA.

### Bump failed before a commit

Read the command's error and inspect the owned files. The recipe restores only
its own `Cargo.toml` and `Cargo.lock` edits. Preserve unrelated changes, fix
the reported cause, and start a fresh bump only after the worktree is clean.
Do not use a reset or restore command that could remove another person's work.

### Signed bump commit exists but push failed

Keep the signed local commit. Fix authentication, divergence, or protection and
retry exactly:

```sh
git push origin HEAD:refs/heads/develop
```

Do not bump again. The version commit is already the intended input.

### Workflow fails before publication

Use GitHub's **Re-run failed jobs** for the recorded run. Successful guard,
build, package, and inventory outputs remain the run's inputs. A full workflow
rerun after tagging is unsupported. If source changes are needed, make them on
`develop`, bump to a new version, and start a new release.

### GitHub is public but crates.io fails

The GitHub tag, release assets, checksums, and provenance are permanent
progress. Re-run the failed jobs of the same recorded workflow run. Do not
retag, delete the release, replace assets, or dispatch a new release for the
same version.

### crates.io accepted the crate but final verification is delayed

Keep the recorded run and SHA. Re-run the failed registry job. The workflow
accepts an existing registry version only when its checksum matches the
verified crate. A different checksum fails and is never overwritten.

### The local watcher is interrupted

Use the recorded run ID and URL to watch that same run. The recipe's recovery
output has this exact form, with the recorded values substituted:

```sh
gh run watch <RUN_ID> --exit-status
git push origin <FULL_SHA>:refs/heads/main
```

Run the push only after the same run succeeds, the trusted `CI OK` run for the
same SHA succeeds, and `main` is still an ancestor. Do not run `just release`
again. If the release workflow failed, rerun its failed jobs first and then
watch the same run.

### The final push to main is rejected

Publication remains valid and the signed bump commit remains unchanged. Fix the
fast-forward or protection issue, confirm the recorded SHA is still the
intended tip, then retry:

```sh
git fetch origin develop main
git push origin <FULL_SHA>:refs/heads/main
```

Never force-push or push a different SHA. If `main` advanced, stop and obtain
the owner decision before changing its history.

### An older run is used after a newer publication

Stop. The guard and publication checks must reject stale version or tag state.
Do not delete or move the newer tag, reuse its version, or replace public
bytes. Prepare a higher version on `develop` and use a new run.

## Removed paths and limits

The old deploy, candidate, manifest, dry-run, republish, status, and branch
protection commands are not release interfaces. There is no scratch release
branch, automatic sandbox synchronization, main-based republish, signed
release tag, atomic branch and tag promotion, or recovery service.

Failed-job reruns reuse successful outputs from the retained run. Once the run
or its artifacts are no longer recoverable, source fixes require a new version.
Published tags, releases, crate versions, and bytes are permanent. Do not
claim a release is complete until the recorded run, checksums, provenance, and
final `main` push have each been verified.
