//! Command-boundary fakes for the release workflow: `gh`, `curl`, `cargo`,
//! `rustup`, `rustc` and `sleep`. Each appends its call to `order.log` (one line
//! per call across all tools, so boundary order is observable) and to its own
//! log. State lives beside the scripts and is scripted or tampered with by the
//! tests; what a workflow step decides is never decided here.

/// Scripted GitHub. `api` serves static routes, a tag store and a release store;
/// `release create|edit|download` and the DELETE endpoint change that store the
/// way the service would. A file in `gh-state/fail/<op>` makes that operation
/// answer with its content as an HTTP status (500 when empty).
pub const FAKE_GH: &str = r#"#!/bin/bash
set -eu
ctl="$(cd "$(dirname "$0")" && pwd -P)"
st="$ctl/gh-state"
printf '%s\n' "$*" >> "$ctl/gh.log"
printf 'gh %s\n' "$*" >> "$ctl/order.log"
include=0

refuse() {
    [[ -f "$st/fail/$1" ]] || return 0
    local code=500
    if [[ -s "$st/fail/$1" ]]; then code="$(cat "$st/fail/$1")"; fi
    if [[ "$include" == 1 ]]; then printf 'HTTP/2.0 %s Injected\r\n\r\n' "$code"; fi
    echo "gh: injected failure (HTTP $code)" >&2
    exit 1
}

absent() {
    if [[ "$include" == 1 ]]; then printf 'HTTP/2.0 404 Not Found\r\n\r\n'; fi
    echo "gh: Not Found (HTTP 404)" >&2
    exit 1
}

api() {
    local method=GET path="" jq="" rel body
    while [[ $# -gt 0 ]]; do
        case "$1" in
            -i|--include) include=1 ;;
            --paginate) ;;
            -X|--method) method="$2"; shift ;;
            --jq) jq="$2"; shift ;;
            -*) echo "unscripted gh api flag $1" >&2; exit 99 ;;
            *) path="$1" ;;
        esac
        shift
    done
    path="${path%%\?*}"
    rel="${path#repos/*/*/}"
    if [[ "$method" == DELETE ]]; then
        refuse delete
        local id="${rel#releases/}"
        jq -c --argjson id "$id" 'map(select(.id != $id))' "$st/releases.json" > "$st/releases.new"
        mv "$st/releases.new" "$st/releases.json"
        rm -rf "$st/assets/$id"
        exit 0
    fi
    [[ "$method" == GET ]] || exit 99
    refuse "get-${rel//\//_}"
    case "$rel" in
        releases) body="$(cat "$st/releases.json")" ;;
        releases/tags/*)
            body="$(jq -c --arg t "${rel#releases/tags/}" '[.[] | select(.tag_name == $t and .draft == false)][0] // empty' "$st/releases.json")"
            [[ -n "$body" ]] || absent ;;
        releases/[0-9]*)
            body="$(jq -c --argjson id "${rel#releases/}" '.[] | select(.id == $id)' "$st/releases.json")"
            [[ -n "$body" ]] || absent ;;
        tags)
            body="$(for f in "$st"/tagrefs/*; do if [[ -e "$f" ]]; then basename "$f"; fi; done | jq -R '{name: .}' | jq -sc .)" ;;
        git/ref/tags/*)
            [[ -f "$st/tagrefs/${rel#git/ref/tags/}" ]] || absent
            body="$(cat "$st/tagrefs/${rel#git/ref/tags/}")" ;;
        *)
            [[ -f "$st/routes/${rel//\//_}" ]] || { echo "unscripted gh api route $rel" >&2; exit 99; }
            body="$(cat "$st/routes/${rel//\//_}")" ;;
    esac
    if [[ "$include" == 1 ]]; then printf 'HTTP/2.0 200 OK\r\n\r\n'; fi
    if [[ -n "$jq" ]]; then printf '%s' "$body" | jq -r "$jq"; else printf '%s\n' "$body"; fi
}

release() {
    local op="$1" tag="$2"
    shift 2
    local files=() target="" draft=0 latest=0 dir="" id
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --repo|--title|--notes-file) shift ;;
            --target) target="$2"; shift ;;
            --dir) dir="$2"; shift ;;
            --draft|--draft=true) draft=1 ;;
            --draft=false) draft=0 ;;
            --latest) latest=1 ;;
            -*) echo "unscripted gh release flag $1" >&2; exit 99 ;;
            *) files+=("$1") ;;
        esac
        shift
    done
    case "$op" in
        create)
            refuse release-create
            [[ "$draft" == 1 ]] || { echo "release created without --draft" >&2; exit 99; }
            id="$(cat "$st/next-id")"
            echo $((id + 1)) > "$st/next-id"
            mkdir -p "$st/assets/$id"
            cp -- "${files[@]}" "$st/assets/$id/"
            local assets
            assets="$(for f in "${files[@]}"; do basename "$f"; done | jq -R '{name: .}' | jq -sc .)"
            jq -c --argjson id "$id" --arg tag "$tag" --arg target "$target" --argjson assets "$assets" \
                '. + [{id: $id, tag_name: $tag, draft: true, target_commitish: $target, assets: $assets}]' \
                "$st/releases.json" > "$st/releases.new"
            mv "$st/releases.new" "$st/releases.json" ;;
        edit)
            refuse release-edit
            id="$(jq -r --arg t "$tag" '[.[] | select(.tag_name == $t and .draft == true)][0].id // empty' "$st/releases.json")"
            [[ -n "$id" ]] || { echo "gh: no draft release for $tag" >&2; exit 1; }
            target="$(jq -r --argjson id "$id" '.[] | select(.id == $id) | .target_commitish' "$st/releases.json")"
            jq -c --argjson id "$id" 'map(if .id == $id then .draft = false else . end)' "$st/releases.json" > "$st/releases.new"
            mv "$st/releases.new" "$st/releases.json"
            # Publishing creates the tag at the release target unless it exists.
            if [[ ! -f "$st/tagrefs/$tag" ]]; then
                printf '{"object":{"type":"commit","sha":"%s"}}' "$target" > "$st/tagrefs/$tag"
            fi
            mkdir -p "$st/published/$tag"
            cp -- "$st/assets/$id/"* "$st/published/$tag/"
            if [[ "$latest" == 1 ]]; then printf '%s' "$tag" > "$st/latest"; fi ;;
        download)
            refuse release-download
            id="$(jq -r --arg t "$tag" '[.[] | select(.tag_name == $t and .draft == false)][0].id // empty' "$st/releases.json")"
            [[ -n "$id" ]] || { echo "gh: release $tag not found" >&2; exit 1; }
            mkdir -p "$dir"
            cp -- "$st/assets/$id/"* "$dir/" ;;
        *) echo "unscripted gh release $op" >&2; exit 99 ;;
    esac
}

case "${1:-}" in
    api) shift; api "$@" ;;
    release) shift; release "$@" ;;
    *) echo "unscripted gh ${1:-}" >&2; exit 99 ;;
esac
"#;

/// Scripted crates.io API. `curl-state/crate-<name>-<version>` holds the checksum
/// the registry reports; `delay-<name>-<version>` is how many answers stay 404;
/// `outage` fails the connection and `status` forces an HTTP status.
pub const FAKE_CURL: &str = r#"#!/bin/bash
set -eu
ctl="$(cd "$(dirname "$0")" && pwd -P)"
st="$ctl/curl-state"
printf '%s\n' "$*" >> "$ctl/curl.log"
printf 'curl %s\n' "$*" >> "$ctl/order.log"
out="" fmt=""
while [[ $# -gt 1 ]]; do
    case "$1" in
        -A) shift ;;
        -o) out="$2"; shift ;;
        -w) fmt="$2"; shift ;;
        -s|-S|-sS) ;;
        *) echo "unscripted curl flag $1" >&2; exit 99 ;;
    esac
    shift
done
url="$1"
[[ "$fmt" == '%{http_code}' && -n "$out" ]] || { echo "unscripted curl call" >&2; exit 99; }
if [[ -f "$st/outage" ]]; then echo "curl: (6) Could not resolve host" >&2; exit 6; fi
if [[ -f "$st/status" ]]; then echo '{}' > "$out"; cat "$st/status"; exit 0; fi
key="$(basename "$(dirname "$url")")-$(basename "$url")"
found=0
if [[ -f "$st/delay-$key" && "$(cat "$st/delay-$key")" -gt 0 ]]; then
    echo $(( $(cat "$st/delay-$key") - 1 )) > "$st/delay-$key"
elif [[ -f "$st/crate-$key" ]]; then
    found=1
fi
if [[ "$found" == 1 ]]; then
    printf '{"version":{"checksum":"%s"}}' "$(cat "$st/crate-$key")" > "$out"
    printf 200
else
    echo '{"errors":[{"detail":"Not Found"}]}' > "$out"
    printf 404
fi
"#;

/// Fake `cargo`. A crate's bytes name the toolchain that packaged it, so a
/// different compiler is a different checksum. Control files beside the script:
/// `fail-package`, `fail-dry-run`, `fail-publish`, `reproduce-differs`,
/// `registry-corrupt`, `registry-delay` and `crash-after-accept`.
pub const FAKE_CARGO: &str = r#"#!/bin/bash
set -eu
ctl="$(cd "$(dirname "$0")" && pwd -P)"
printf '[%s] %s\n' "${RUSTUP_TOOLCHAIN:-}" "$*" >> "$ctl/cargo.log"
printf 'cargo %s\n' "$*" >> "$ctl/order.log"
name="$(awk -F'"' '/^name = /{print $2; exit}' Cargo.toml)"
version="$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml)"
crate="target/package/$name-$version.crate"
refuse() { if [[ -f "$ctl/$1" ]]; then echo "injected $1" >&2; exit 1; fi; }
write_crate() {
    mkdir -p target/package
    printf 'crate %s %s\nrustc=%s\n' "$name" "$version" "${RUSTUP_TOOLCHAIN:-none}" > "$crate"
}
case "${1:-}" in
    package)
        refuse fail-package
        write_crate
        if [[ " $* " == *" --no-verify "* && -f "$ctl/reproduce-differs" ]]; then echo drift >> "$crate"; fi ;;
    publish)
        if [[ " $* " == *" --dry-run "* ]]; then
            refuse fail-dry-run
            write_crate
        else
            refuse fail-publish
            [[ -n "${CARGO_REGISTRY_TOKEN:-}" ]] || { echo "no registry token" >&2; exit 1; }
            write_crate
            sum="$(sha256sum "$crate" | awk '{print $1}')"
            if [[ -f "$ctl/registry-corrupt" ]]; then sum="$(printf tampered | sha256sum | awk '{print $1}')"; fi
            printf '%s' "$sum" > "$ctl/curl-state/crate-$name-$version"
            if [[ -f "$ctl/registry-delay" ]]; then cp "$ctl/registry-delay" "$ctl/curl-state/delay-$name-$version"; fi
            refuse crash-after-accept
        fi ;;
    *) echo "unscripted cargo ${1:-}" >&2; exit 99 ;;
esac
"#;

/// Fake `rustup`: only records the call.
pub const FAKE_RUSTUP: &str = r#"#!/bin/bash
ctl="$(cd "$(dirname "$0")" && pwd -P)"
printf 'rustup %s\n' "$*" >> "$ctl/order.log"
"#;

/// Fake `rustc`: `+stable` answers with the scripted channel version, anything
/// else with the toolchain selected through `RUSTUP_TOOLCHAIN`.
pub const FAKE_RUSTC: &str = r#"#!/bin/bash
ctl="$(cd "$(dirname "$0")" && pwd -P)"
printf 'rustc %s\n' "$*" >> "$ctl/order.log"
if [[ "${1:-}" == +stable ]]; then
    version="$(cat "$ctl/stable-version")"
else
    version="${RUSTUP_TOOLCHAIN:-$(cat "$ctl/stable-version")}"
fi
printf 'rustc %s (abcdef123 2026-01-01)\n' "$version"
"#;

/// Fake `sleep`: time is a fake, so waits return at once and are only counted.
pub const FAKE_SLEEP: &str = r#"#!/bin/bash
ctl="$(cd "$(dirname "$0")" && pwd -P)"
printf 'sleep %s\n' "$*" >> "$ctl/order.log"
"#;
