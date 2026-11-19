//! Behavioral tests for the local build and package actions.
//!
//! The real `run:` blocks of `crate-info`, `build-binary` and `linux-packages`
//! execute in order against fake compiler, packager and inspection tools (see
//! `release_packages/tools.rs`). Whether an output is acceptable is decided by the
//! actions' own shell. A failing step ends the run, so nothing after it (the
//! upload a workflow would do next) is reached; assertions read exit statuses and
//! the call logs, never printed prose.

#[path = "release_packages/assets.rs"]
mod assets;
mod release_actions;
mod release_support;
#[path = "release_packages/tools.rs"]
mod tools;

use anyhow::Result;
use release_actions::ActionRun;
use release_support::{ReleaseFixture, text, write_executable};
use std::fs;

const X86_64: &str = "x86_64-unknown-linux-musl";
const AARCH64: &str = "aarch64-unknown-linux-musl";

fn fixture_with_tools() -> Result<ReleaseFixture> {
    let fixture = ReleaseFixture::new()?;
    for (name, script) in [
        ("cargo", tools::FAKE_CARGO),
        ("strip", tools::FAKE_STRIP),
        ("dpkg-deb", tools::FAKE_DPKG_DEB),
        ("rpm", tools::FAKE_RPM),
        ("rpm2archive", tools::FAKE_RPM2ARCHIVE),
    ] {
        write_executable(&fixture.bin.join(name), script)?;
    }
    Ok(fixture)
}

/// Run one step, print its output, and report whether it succeeded.
async fn step(action: &mut ActionRun<'_>, id: &str) -> Result<bool> {
    let output = action.step(id).await?;
    println!("== {id}: {}\n{}", output.status, text(&output));
    Ok(output.status.success())
}

/// Drive the three actions for `target` in workflow order. Returns the id of the
/// first failing step, or `None` when every step succeeded.
async fn run_actions(fixture: &ReleaseFixture, target: &str) -> Result<Option<&'static str>> {
    let mut info = ActionRun::load(fixture, "crate-info", &[])?;
    let mut build = ActionRun::load(
        fixture,
        "build-binary",
        &[("target", target), ("toolchain", "1.99.1")],
    )?;
    let mut packages = ActionRun::load(fixture, "linux-packages", &[("target", target)])?;
    if !step(&mut info, "read").await? {
        return Ok(Some("read"));
    }
    for name in ["name", "version"] {
        let value = info.output("read", name)?.to_owned();
        build.provide("info", name, &value);
        packages.provide("info", name, &value);
    }
    if !step(&mut build, "build").await? {
        return Ok(Some("build"));
    }
    if !step(&mut build, "smoke").await? {
        return Ok(Some("smoke"));
    }
    for id in ["tools", "package", "check"] {
        if !step(&mut packages, id).await? {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

fn files_in(fixture: &ReleaseFixture, dir: &str) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(fixture.work.join(dir))? {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

#[tokio::test]
async fn native_outputs() -> Result<()> {
    for (target, deb, rpm) in [
        (
            AARCH64,
            "fixture_1.2.3-1_arm64.deb",
            "fixture-1.2.3-1.aarch64.rpm",
        ),
        (
            X86_64,
            "fixture_1.2.3-1_amd64.deb",
            "fixture-1.2.3-1.x86_64.rpm",
        ),
    ] {
        let fixture = fixture_with_tools()?;
        if target == AARCH64 {
            write_executable(&fixture.bin.join("rpm2cpio"), tools::LEGACY_RPM2CPIO)?;
        }
        assert_eq!(run_actions(&fixture, target).await?, None, "{target}");
        assert_eq!(files_in(&fixture, "dist")?, [rpm, deb], "{target}");
        assert_eq!(fixture.calls("rpm2archive")?, ["-n -"], "{target}");
        let calls = fixture.calls("cargo")?;
        for expected in [
            "install cargo-generate-rpm --version 0.21.0 --locked".to_owned(),
            "install cargo-deb --version 3.8.0 --locked".to_owned(),
            format!("build --release --locked --target {target}"),
            format!("generate-rpm --target {target}"),
            format!("deb --no-build --target {target}"),
        ] {
            assert!(calls.contains(&expected), "{target}: missing `{expected}`");
        }
        assert_eq!(
            fixture.calls("strip")?,
            [format!("-s target/{target}/release/fixture")],
            "{target}"
        );
        assert!(fixture.calls("gh")?.is_empty(), "{target}");
    }
    Ok(())
}

/// One invalid output: the fake-tool controls and files to set up, the target,
/// and the step that must be the first to fail.
struct Case {
    name: &'static str,
    target: &'static str,
    controls: &'static [(&'static str, &'static str)],
    stale: &'static [&'static str],
    fails_at: &'static str,
}

const UNIT: &str = "/etc/systemd/system/fixture.service";
const ENV_FILE: &str = "/etc/fixture/fixture.env";
const BINARY: &str = "/usr/bin/fixture";

const fn case(
    name: &'static str,
    controls: &'static [(&'static str, &'static str)],
    fails_at: &'static str,
) -> Case {
    Case {
        name,
        target: X86_64,
        controls,
        stale: &[],
        fails_at,
    }
}

const CASES: &[Case] = &[
    case(
        "wrong binary version",
        &[("binary-output", "fixture 9.9.9 - x")],
        "smoke",
    ),
    case(
        "wrong binary name",
        &[("binary-output", "other 1.2.3 - x")],
        "smoke",
    ),
    case("build failure", &[("fail-build", "")], "build"),
    case("strip failure", &[("fail-strip", "")], "package"),
    case("rpm tool failure", &[("fail-generate-rpm", "")], "package"),
    case("deb tool failure", &[("fail-deb", "")], "package"),
    case("missing rpm", &[("no-rpm", "")], "package"),
    case("missing deb", &[("no-deb", "")], "package"),
    case("duplicate rpm", &[("extra-rpm", "")], "check"),
    case("duplicate deb", &[("extra-deb", "")], "check"),
    case("wrong deb version", &[("deb-version", "1.2.4-1")], "check"),
    case("wrong deb architecture", &[("deb-arch", "i386")], "check"),
    case("wrong rpm version", &[("rpm-version", "1.2.4")], "check"),
    case("wrong rpm architecture", &[("rpm-arch", "i686")], "check"),
    case("deb without binary", &[("drop-deb", BINARY)], "check"),
    case("deb without unit", &[("drop-deb", UNIT)], "check"),
    case("rpm without unit", &[("drop-rpm", UNIT)], "check"),
    case("rpm without config", &[("drop-rpm", ENV_FILE)], "check"),
    case("deb with different binary", &[("tamper-deb", "")], "check"),
    case("rpm with different binary", &[("tamper-rpm", "")], "check"),
    Case {
        name: "aarch64 target packaged as amd64",
        target: AARCH64,
        controls: &[("deb-arch", "amd64")],
        stale: &[],
        fails_at: "check",
    },
    Case {
        name: "aarch64 rpm archive producer failure",
        target: AARCH64,
        controls: &[("fail-rpm2archive", "")],
        stale: &[],
        fails_at: "check",
    },
    Case {
        name: "aarch64 rpm archive producer fails after complete payload",
        target: AARCH64,
        controls: &[("fail-rpm2archive-after-payload", "")],
        stale: &[],
        fails_at: "check",
    },
    Case {
        name: "stale deb must not stand in for a missing one",
        target: X86_64,
        controls: &[("no-deb", "")],
        stale: &["target/x86_64-unknown-linux-musl/debian/fixture_1.2.3-1_amd64.deb"],
        fails_at: "package",
    },
    Case {
        name: "stale package left in the output directory",
        target: X86_64,
        controls: &[],
        stale: &["dist/fixture-1.2.2-1.x86_64.rpm"],
        fails_at: "check",
    },
];

#[tokio::test]
async fn invalid_outputs() -> Result<()> {
    for case in CASES {
        println!("##### {}", case.name);
        let fixture = fixture_with_tools()?;
        for (control, value) in case.controls {
            fs::write(fixture.bin.join(control), value)?;
        }
        for stale in case.stale {
            let path = fixture.work.join(stale);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, "stale")?;
        }
        assert_eq!(
            run_actions(&fixture, case.target).await?,
            Some(case.fails_at),
            "{}",
            case.name
        );
        // The run stopped at the failing step, so no publication was reached.
        assert!(fixture.calls("gh")?.is_empty(), "{}", case.name);
        assert!(
            !fixture
                .calls("cargo")?
                .iter()
                .any(|call| call.starts_with("publish")),
            "{}",
            case.name
        );
    }
    Ok(())
}
