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

# Check if working directory is clean
check-clean:
    #!/usr/bin/env bash
    if [[ -n $(git status --porcelain) ]]; then
        echo "❌ Working directory is not clean. Commit or stash your changes first."
        git status --short
        exit 1
    fi
    echo "✅ Working directory is clean"

# Check if on develop branch
check-develop:
    #!/usr/bin/env bash
    current_branch=$(git branch --show-current)
    if [[ "$current_branch" != "develop" ]]; then
        echo "❌ Not on develop branch (currently on: $current_branch)"
        echo "Switch to develop branch first: git checkout develop"
        exit 1
    fi
    echo "✅ On develop branch"

# Check if tag already exists for a given version
check-tag-not-exists version:
    #!/usr/bin/env bash
    set -euo pipefail
    version="{{version}}"

    git fetch --tags --quiet

    if git rev-parse -q --verify "refs/tags/${version}" >/dev/null 2>&1; then
        echo "❌ Tag ${version} already exists!"
        exit 1
    fi

    echo "✅ No tag exists for version ${version}"

# Releases run scripts/release: the version bump is staged on the scratch `release`
# branch, Test & Build and a candidate run of release.yml (every test, build and package,
# published nowhere) test that exact commit, and only then do develop, main and the
# signed tag move together in one atomic push; the tag's run publishes exactly what the
# candidate run built. Every deploy recipe is idempotent: rerunning it resumes the staged
# candidate, or says there is nothing left to release. See README "Releasing".

# Deploy: stage a patch bump, test and package it, then release it
deploy:
    @scripts/release deploy patch

# Deploy with minor version bump
deploy-minor:
    @scripts/release deploy minor

# Deploy with major version bump
deploy-major:
    @scripts/release deploy major

# Release develop's version as is when it has no tag yet (no new bump)
deploy-current:
    @scripts/release deploy current

# Show where a release stands: develop, main, the staged candidate and its runs
release-status:
    @scripts/release status

# Check everything a release needs; changes nothing apart from fetching
release-preflight:
    @scripts/release preflight

# Publish an existing release tag again if its own run cannot (recovery run on main)
release-republish version:
    @scripts/release republish {{version}}

# Apply the branch protection the release flow relies on (main requires "CI OK")
protect-branches:
    @scripts/release protect

# Create & push a test tag like t-YYYYMMDD-HHMMSS (tests, builds and packages; publishes nothing)
# Usage:
#   just t-deploy
#   just t-deploy "optional tag message"
t-deploy message="CI test": check-develop check-clean test
    #!/usr/bin/env bash
    set -euo pipefail

    message="{{message}}"
    ts="$(date -u +%Y%m%d-%H%M%S)"
    tag="t-${ts}"

    echo "🏷️  Creating signed test tag: ${tag}"
    git fetch --tags --quiet

    if git rev-parse -q --verify "refs/tags/${tag}" >/dev/null; then
        echo "❌ Tag ${tag} already exists. Aborting." >&2
        exit 1
    fi

    git tag -s "${tag}" -m "${message}"
    git push origin "${tag}"

    echo "✅ Pushed ${tag}"
    echo "🧹 To remove it:"
    echo "   git push origin :refs/tags/${tag} && git tag -d ${tag}"


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
