use anyhow::Result;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

/// Stateful command fixture for real-process tests (never use a paused clock).
///
/// `state` contains `IF ADDR` rows. `flags` contains `IF ADDR FLAGS` rows.
/// `fail-add` / `fail-del` contain `always` or the invocation number to fail;
/// the corresponding `count-*` file counts attempts, including failed ones.
/// A `gate-add` / `gate-del` FIFO blocks that operation before it reads state.
/// The command prints `GATED OP` once its FIFO is open; write a line to release
/// it. Reads expire after ten seconds. Mutations are intended to be serialized
/// by the caller, just like the resource worker.
pub struct FakeIp {
    pub dir: PathBuf,
}

impl FakeIp {
    /// Create commands and an empty address table in a private temporary directory.
    ///
    /// # Errors
    ///
    /// Returns filesystem errors while creating the fixture.
    pub fn new() -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "gruezi-fake-ip-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir)?;
        let fixture = Self { dir };
        fs::write(fixture.dir.join("state"), "")?;
        fs::write(fixture.dir.join("flags"), "")?;
        for name in ["ip", "arping", "ndsend"] {
            let path = fixture.command(name);
            fs::write(&path, SCRIPT)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
        }
        Ok(fixture)
    }

    pub fn command(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for FakeIp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

const SCRIPT: &str = r#"#!/bin/bash
set -eu
root="$(cd "$(dirname "$0")" && pwd -P)"
name="${0##*/}"
printf '%s %s\n' "$name" "$*" >> "$root/commands"
if [[ "$name" != ip ]]; then
    exit 0
fi

if [[ "$#" == 5 && "$1" == -o && ( "$2" == address || "$2" == addr ) && "$3" == show && "$4" == dev ]]; then
    while read -r interface address; do
        [[ "$interface" == "$5" ]] || continue
        family=inet
        [[ "$address" != *:* ]] || family=inet6
        flags=""
        while read -r flag_if flag_addr value; do
            if [[ "$flag_if" == "$interface" && "$flag_addr" == "$address" ]]; then
                flags="$value"
            fi
        done < "$root/flags"
        printf '2: %s    %s %s scope global %s %s\n' "$interface" "$family" "$address" "$flags" "$interface"
    done < "$root/state"
    exit 0
fi

if [[ "$#" != 5 || "$1" != address || ( "$2" != add && "$2" != del ) || "$4" != dev ]]; then
    echo "unsupported fake ip arguments: $*" >&2
    exit 2
fi
operation="$2"
address="$3"
interface="$5"
count=0
[[ ! -f "$root/count-$operation" ]] || read -r count < "$root/count-$operation"
count=$((count + 1))
printf '%s\n' "$count" > "$root/count-$operation"
if [[ -p "$root/gate-$operation" ]]; then
    exec 3<> "$root/gate-$operation"
    printf 'GATED %s\n' "$operation"
    if ! read -r -t 10 token <&3; then
        echo "fake ip gate timed out: $operation" >&2
        exit 1
    fi
    exec 3>&-
fi
if [[ -f "$root/fail-$operation" ]]; then
    read -r failure < "$root/fail-$operation"
    if [[ "$failure" == always || "$failure" == "$count" ]]; then
        echo "RTNETLINK answers: injected $operation failure" >&2
        exit 1
    fi
fi
found=false
while read -r current_if current_addr; do
    if [[ "$current_if" == "$interface" && "$current_addr" == "$address" ]]; then
        found=true
    fi
done < "$root/state"
if [[ "$operation" == add ]]; then
    if "$found"; then
        echo "RTNETLINK answers: File exists" >&2
        exit 2
    fi
    printf '%s %s\n' "$interface" "$address" >> "$root/state"
else
    if ! "$found"; then
        echo "RTNETLINK answers: Cannot assign requested address" >&2
        exit 2
    fi
    while read -r current_if current_addr; do
        if [[ "$current_if" != "$interface" || "$current_addr" != "$address" ]]; then
            printf '%s %s\n' "$current_if" "$current_addr"
        fi
    done < "$root/state" > "$root/state.next"
    mv "$root/state.next" "$root/state"
fi
"#;
