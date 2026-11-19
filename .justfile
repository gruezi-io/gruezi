set shell := ["zsh", "-uc"]

# Get the current user's UID and GID
uid := `id -u`
gid := `id -g`
root := justfile_directory()
net := "gruezi-net"
subnet := "172.31.21.0/24"
image := "localhost/gruezi:test"
node_a := "gruezi-ha-a"
node_b := "gruezi-ha-b"
api_a := "19376"
api_b := "29376"

default: test
  @just --list

setup-network:
  podman network inspect {{net}} >/dev/null 2>&1 || podman network create --subnet {{subnet}} {{net}}

build-image:
  cargo build
  podman build -t {{image}} -f {{root}}/Containerfile {{root}}

stop-ha:
  @for c in {{node_a}} {{node_b}}; do \
        podman stop $$c 2>/dev/null || true; \
        podman rm $$c 2>/dev/null || true; \
  done

test-ha: setup-network build-image stop-ha
  podman run --replace -d --name {{node_a}} \
    --cap-add NET_ADMIN \
    --network {{net}} --ip 172.31.21.11 \
    -p {{api_a}}:9376 \
    -v {{root}}/examples/ha-node-a.yaml:/etc/gruezi/gruezi.yaml:ro \
    {{image}} start --config /etc/gruezi/gruezi.yaml
  podman run --replace -d --name {{node_b}} \
    --cap-add NET_ADMIN \
    --network {{net}} --ip 172.31.21.12 \
    -p {{api_b}}:9376 \
    -v {{root}}/examples/ha-node-b.yaml:/etc/gruezi/gruezi.yaml:ro \
    {{image}} start --config /etc/gruezi/gruezi.yaml
  @echo "HA containers created."
  @echo "Query node A: cargo run -- status --node 127.0.0.1:{{api_a}}"
  @echo "Query node B: cargo run -- status --node 127.0.0.1:{{api_b}}"
  podman ps -a --filter name={{node_a}} --filter name={{node_b}}

logs-ha:
  podman logs {{node_a}}
  podman logs {{node_b}}

status-ha:
  cargo run -- status --node 127.0.0.1:{{api_a}}
  cargo run -- status --node 127.0.0.1:{{api_b}}

# Test suite
test: clippy fmt
  cargo test

# Linting
clippy:
  cargo clippy --all-targets --all-features

# Formatting check
fmt:
  cargo fmt --all -- --check

# Coverage report
coverage:
  CARGO_INCREMENTAL=0 RUSTFLAGS='-Cinstrument-coverage' LLVM_PROFILE_FILE='coverage-%p-%m.profraw' cargo test
  grcov . --binary-path ./target/debug/deps/ -s . -t html --branch --ignore-not-existing --ignore '../*' --ignore "/*" -o target/coverage/html
  firefox target/coverage/html/index.html
  rm -rf *.profraw

# Build a Debian package from the release binary and contrib assets
package-deb:
  cargo build --release --locked
  cargo deb --no-build

# Build an RPM package from the release binary and contrib assets
package-rpm:
  cargo build --release --locked
  cargo generate-rpm

# Update dependencies
update:
  cargo update

# Clean build artifacts
clean:
  cargo clean

# Get current version
version:
    @cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version'

# Bump the patch version on develop: refresh dependencies, verify, sign the version commit and push it
bump: (_bump "patch")

# Bump the minor version on develop (see bump)
bump-minor: (_bump "minor")

# Bump the major version on develop (see bump)
bump-major: (_bump "major")

# Shared bump implementation. Everything that can refuse (dirty tree, branch, stale or
# unreachable origin, existing tag, unusable signer) is checked before the first edit.
# Cargo writes the version and lockfile; a failure before the commit puts back only those
# two files and reports any other change it found. A rejected push keeps the signed commit
# and prints the retry command: never bump again to recover a push.
[private]
_bump kind:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{justfile_directory()}}"
    kind="{{kind}}"
    release_files=(Cargo.toml Cargo.lock)
    mutating=false
    committed=false

    die() { echo "bump: $*" >&2; exit 1; }
    current_version() { awk -F'"' '/^version = /{print $2; exit}' Cargo.toml; }
    status_lines() { git status --porcelain --untracked-files=all; }
    outside_release_files() {
        { git diff --name-only; git diff --cached --name-only; git ls-files --others --exclude-standard; } |
            sort -u | grep -vxE 'Cargo\.(toml|lock)' || true
    }
    require_only_release_files() {
        local stray
        stray="$(outside_release_files)"
        [[ -z "${stray}" ]] || die "verification changed files outside Cargo.toml and Cargo.lock (${stray//$'\n'/, }); not committing."
    }
    cleanup() {
        local rc=$?
        trap - EXIT
        if [[ "${rc}" -ne 0 && "${mutating}" == true && "${committed}" == false ]]; then
            git restore --staged --worktree --source=HEAD -- "${release_files[@]}"
            echo "bump: failed before committing; restored ${release_files[*]}." >&2
            if [[ -n "$(status_lines)" ]]; then
                echo "bump: left in place, not made by the version bump:" >&2
                status_lines >&2
            fi
        fi
        exit "${rc}"
    }

    [[ -z "$(status_lines)" ]] || die "the working tree must be clean, including untracked files."
    branch="$(git symbolic-ref --quiet --short HEAD || true)"
    [[ "${branch}" == develop ]] || die "bump runs on develop, not '${branch:-a detached HEAD}'."
    git fetch --quiet --tags origin || die "could not fetch origin; nothing was changed."
    git merge-base --is-ancestor refs/remotes/origin/develop HEAD ||
        die "develop is behind or has diverged from origin/develop; update it first."

    start="$(current_version)"
    [[ "${start}" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] ||
        die "Cargo.toml version '${start}' is not a canonical X.Y.Z version."
    major="${BASH_REMATCH[1]}" minor="${BASH_REMATCH[2]}" patch="${BASH_REMATCH[3]}"
    case "${kind}" in
        patch) expected="${major}.${minor}.$((patch + 1))" ;;
        minor) expected="${major}.$((minor + 1)).0" ;;
        major) expected="$((major + 1)).0.0" ;;
        *) die "unknown bump kind '${kind}'." ;;
    esac
    ! git rev-parse --quiet --verify "refs/tags/${expected}" >/dev/null || die "tag ${expected} already exists."
    # Signing a throwaway commit object proves the signer works before anything is edited.
    git commit-tree -S "HEAD^{tree}" -m "signing probe" >/dev/null ||
        die "cannot sign commits; check user.signingkey and gpg.format. Nothing was changed."

    trap cleanup EXIT
    trap 'exit 130' INT TERM
    mutating=true
    echo "Bumping ${start} to ${expected} (${kind})"
    cargo set-version --bump "${kind}"
    cargo update
    new="$(current_version)"
    [[ "${new}" == "${expected}" ]] || die "cargo set-version produced ${new}, expected ${expected}."
    "{{just_executable()}}" --justfile "{{justfile()}}" test

    require_only_release_files
    git add -- "${release_files[@]}"
    require_only_release_files
    git commit --quiet -S -m "Bump version to ${new}"
    committed=true
    if ! git push origin HEAD:refs/heads/develop; then
        echo "bump: version commit $(git rev-parse --short HEAD) is signed and committed locally, but the push was rejected." >&2
        echo "bump: fix the cause, then retry only: git push origin HEAD:refs/heads/develop" >&2
        exit 1
    fi
    echo "Bumped to ${new} and pushed develop."

# Release the exact commit selected from origin/develop, then fast-forward main.
release:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{justfile_directory()}}"

    sha="$(git rev-parse origin/develop)"
    git fetch --quiet origin develop main
    sha="$(git rev-parse origin/develop)"
    dispatch_id="release-$(date -u +%Y%m%dT%H%M%SZ)-$$-${RANDOM}"
    repo="$(gh repo view --json nameWithOwner --jq '.nameWithOwner')"
    marker="Release ${sha} dispatch ${dispatch_id}"
    recovery() {
        echo "release: publication progress is preserved for ${sha} (run ${run:-unknown})." >&2
        echo "release: recover the same run with: gh run watch ${run:-<run-id>} --exit-status" >&2
        echo "release: after CI OK and main ancestry are verified, push: git push origin ${sha}:refs/heads/main" >&2
    }
    trap recovery ERR

    gh workflow run release.yml --ref develop \
        -f "expected_sha=${sha}" -f "dispatch_id=${dispatch_id}"
    run=""
    for _ in $(seq 30); do
        run="$(
            gh run list --workflow release.yml --branch develop --event workflow_dispatch --limit 100 \
                --json databaseId,name,workflowName,event,headSha,headBranch,url,repository \
                --jq '.[] | select(.name == "'"${marker}"'" and .workflowName == "Release" and .event == "workflow_dispatch" and .headBranch == "develop" and .headSha == "'"${sha}"'" and .repository.full_name == "'"${repo}"'") | .databaseId' |
                head -n 1
        )"
        [[ -n "${run}" ]] && break
        sleep 2
    done
    [[ -n "${run}" ]] || { echo "release: no run appeared for ${marker}" >&2; exit 1; }
    url="$(
        gh run view "${run}" --json databaseId,name,workflowName,event,headSha,headBranch,url,repository \
            --jq 'select(.name == "'"${marker}"'" and .workflowName == "Release" and .event == "workflow_dispatch" and .headBranch == "develop" and .headSha == "'"${sha}"'" and .repository.full_name == "'"${repo}"'") | .url'
    )"
    [[ -n "${url}" ]] || { echo "release: correlated run ${run} failed identity validation" >&2; exit 1; }
    trap - ERR
    echo "release: selected ${sha}; run ${run}: ${url}"
    gh run watch "${run}" --exit-status || { recovery; exit 1; }

    ci="$(
        gh run list --workflow build.yml --branch develop --event push --commit "${sha}" --limit 100 \
            --json databaseId,workflowName,event,headSha,headBranch,repository \
            --jq '.[] | select(.workflowName == "Test & Build" and .event == "push" and .headBranch == "develop" and .headSha == "'"${sha}"'" and .repository.full_name == "'"${repo}"'") | .databaseId' |
            head -n 1
    )"
    [[ -n "${ci}" ]] || { echo "release: no trusted Test & Build run for ${sha}" >&2; exit 1; }
    gh run watch "${ci}" --exit-status || { recovery; exit 1; }
    git merge-base --is-ancestor origin/main "${sha}" || { echo "release: main is not an ancestor of ${sha}" >&2; exit 1; }
    git push origin "${sha}:refs/heads/main" || { recovery; exit 1; }
    echo "release: promoted ${sha} to main."

# On-demand Jaeger for the devcontainer (compose "observability" profile)
obs-dev:
  {{root}}/scripts/obs-dev up

# Stop the on-demand devcontainer Jaeger
obs-dev-stop:
  {{root}}/scripts/obs-dev down

jaeger:
  podman run --rm -d --name jaeger \
    -e COLLECTOR_OTLP_ENABLED=true \
    -p 16686:16686 \
    -p 4317:4317 \
    -p 4318:4318 \
    jaegertracing/all-in-one:latest

stop-containers:
  @for c in jaeger; do \
        podman stop $c 2>/dev/null || true; \
  done
