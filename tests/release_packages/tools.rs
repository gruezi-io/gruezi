//! Command-boundary fakes for the build and packaging tools.
//!
//! Fake packages are tar archives holding `META` (`Version=`, `Release=`,
//! `Architecture=`) and `root.tar` (the installed tree). The fake `dpkg-deb`,
//! `rpm` and `rpm2archive` read that format, so the action's own checks
//! decide whether a package is acceptable. Control files in the fake `bin`
//! directory steer the fakes: `fail-<tool or cargo subcommand>`, and files whose
//! content is the value to use: `binary-output`, `deb-version`, `deb-arch`,
//! `rpm-version`, `rpm-arch`, `drop-deb`, `drop-rpm` (an installed path to
//! omit), `tamper-deb`, `tamper-rpm`, `no-deb`, `no-rpm`, `extra-deb`, `extra-rpm`.
//! `fail-rpm2archive-after-payload` fails after emitting the complete installed tree.

/// Fake `cargo` for the `fixture` 1.2.3 crate: logs calls, builds a binary that
/// prints a version line, and writes the packages where the real packagers do.
pub const FAKE_CARGO: &str = r#"#!/bin/bash
set -eu
ctl="$(cd "$(dirname "$0")" && pwd -P)"
printf '%s\n' "$*" >> "$ctl/cargo.log"
sub="${1:-}"
if [[ -f "$ctl/fail-$sub" ]]; then
    echo "injected cargo $sub failure" >&2
    exit 1
fi
want() { if [[ -f "$ctl/$1" ]]; then cat "$ctl/$1"; else printf '%s' "$2"; fi; }
target=""
while [[ $# -gt 0 ]]; do
    if [[ "$1" == --target ]]; then target="${2:-}"; fi
    shift
done
case "$target" in
    x86_64-*) rpm_arch=x86_64; deb_arch=amd64 ;;
    aarch64-*) rpm_arch=aarch64; deb_arch=arm64 ;;
    *) rpm_arch=unknown; deb_arch=unknown ;;
esac
pack() {
    local kind="$1" out="$2" meta="$3" tmp
    tmp="$(mktemp -d)"
    mkdir -p "$tmp/root/usr/bin" "$tmp/root/etc/systemd/system" "$tmp/root/etc/fixture"
    cp "target/$target/release/fixture" "$tmp/root/usr/bin/fixture"
    : > "$tmp/root/etc/systemd/system/fixture.service"
    : > "$tmp/root/etc/fixture/fixture.env"
    if [[ -f "$ctl/tamper-$kind" ]]; then echo tampered >> "$tmp/root/usr/bin/fixture"; fi
    if [[ -f "$ctl/drop-$kind" ]]; then rm -f "$tmp/root$(cat "$ctl/drop-$kind")"; fi
    tar -C "$tmp/root" -cf "$tmp/root.tar" .
    printf '%s\n' "$meta" > "$tmp/META"
    mkdir -p "$(dirname "$out")"
    tar -C "$tmp" -cf "$out" META root.tar
    rm -rf "$tmp"
}
case "$sub" in
build)
    mkdir -p "target/$target/release"
    printf '#!/bin/sh\necho "%s"\n' "$(want binary-output 'fixture 1.2.3 - 0123abc')" > "target/$target/release/fixture"
    chmod 755 "target/$target/release/fixture"
    ;;
generate-rpm)
    dir="target/$target/generate-rpm"
    meta="Version=$(want rpm-version 1.2.3)
Release=1
Architecture=$(want rpm-arch "$rpm_arch")"
    if [[ ! -f "$ctl/no-rpm" ]]; then pack rpm "$dir/fixture-1.2.3-1.$rpm_arch.rpm" "$meta"; fi
    if [[ -f "$ctl/extra-rpm" ]]; then pack rpm "$dir/fixture-1.2.3-2.$rpm_arch.rpm" "$meta"; fi
    ;;
deb)
    dir="target/$target/debian"
    meta="Version=$(want deb-version 1.2.3-1)
Architecture=$(want deb-arch "$deb_arch")"
    if [[ ! -f "$ctl/no-deb" ]]; then pack deb "$dir/fixture_1.2.3-1_$deb_arch.deb" "$meta"; fi
    if [[ -f "$ctl/extra-deb" ]]; then pack deb "$dir/fixture_1.2.3-2_$deb_arch.deb" "$meta"; fi
    ;;
esac
"#;

/// Fake `strip`: logs the call and fails on request, leaving the file alone.
pub const FAKE_STRIP: &str = r#"#!/bin/bash
set -eu
ctl="$(cd "$(dirname "$0")" && pwd -P)"
printf '%s\n' "$*" >> "$ctl/strip.log"
if [[ -f "$ctl/fail-strip" ]]; then
    echo "injected strip failure" >&2
    exit 1
fi
"#;

/// Fake `dpkg-deb`: `--field <deb> <name>` and `-x <deb> <dir>`.
pub const FAKE_DPKG_DEB: &str = r#"#!/bin/bash
set -eu
case "${1:-}" in
    --field) tar -xOf "$2" META | sed -n "s/^$3=//p" ;;
    -x) mkdir -p "$3"; tar -xOf "$2" root.tar | tar -xf - -C "$3" ;;
    *) exit 2 ;;
esac
"#;

/// Fake `rpm`: `-qp --queryformat <format> <rpm>`, answering `%{VERSION}-%{RELEASE}`
/// and `%{ARCH}`.
pub const FAKE_RPM: &str = r#"#!/bin/bash
set -eu
file="${4:-}"
field() { tar -xOf "$file" META | sed -n "s/^$1=//p"; }
case "${3:-}" in
    *VERSION*) printf '%s-%s' "$(field Version)" "$(field Release)" ;;
    *ARCH*) printf '%s' "$(field Architecture)" ;;
    *) exit 2 ;;
esac
"#;

/// Fake `rpm2archive`: requires uncompressed tar on stdout from the RPM on stdin.
pub const FAKE_RPM2ARCHIVE: &str = r#"#!/bin/bash
set -euo pipefail
ctl="$(cd "$(dirname "$0")" && pwd -P)"
printf '%s\n' "$*" >> "$ctl/rpm2archive.log"
[[ $# -eq 2 && "$1" == -n && "$2" == - ]] || exit 2
if [[ -f "$ctl/fail-rpm2archive" ]]; then exit 1; fi
tar -xOf - root.tar
if [[ -f "$ctl/fail-rpm2archive-after-payload" ]]; then exit 1; fi
"#;

/// RPM 4.18 regression: emit a full cpio payload, then silently return failure.
/// Only the legacy extraction path invokes this; the action uses `rpm2archive`.
pub const LEGACY_RPM2CPIO: &str = r#"#!/bin/bash
set -euo pipefail
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
tar -xOf "$1" root.tar | tar -xf - -C "$work"
cd "$work"
find . -type f | cpio --quiet -o -H newc
exit 1
"#;
