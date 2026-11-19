//! Behavioral tests for `.github/workflows/release.yml`.
//!
//! The workflow's own shell runs against a scripted GitHub, crates.io and
//! command boundary (see `world`/`fakes`), one job after another like the runner
//! would. Outcomes are read from the recorded call order and the service state,
//! never from printed prose. The three entry points are `release_success`,
//! `release_failure` and `compiler_reuse`; `release_failure` also holds the
//! structural gates (needs graph, YAML parsing, Bash syntax).

#[path = "release_workflow/artifacts.rs"]
mod artifacts;
#[path = "release_workflow/boundary.rs"]
mod boundary;
#[path = "release_workflow/fakes.rs"]
mod fakes;
#[path = "release_workflow/model.rs"]
mod model;
#[path = "release_actions/mod.rs"]
mod release_actions;
#[path = "release_support/mod.rs"]
mod release_support;
#[path = "release_workflow/runner.rs"]
mod runner;
#[path = "release_workflow/world.rs"]
mod world;

#[path = "release_workflow/contract.rs"]
mod contract;
#[path = "release_workflow/failure.rs"]
mod failure;
#[path = "release_workflow/publication.rs"]
mod publication;
#[path = "release_workflow/success.rs"]
mod success;

use anyhow::Result;
use runner::{Outcome, Runner};
use std::fs;
use world::{Release, World};

/// The version the fixture repository carries.
const VERSION: &str = "1.2.3";

/// A release that exists in the scripted service.
fn release(
    id: u64,
    tag: &str,
    target: &str,
    draft: bool,
    latest: bool,
    assets: &[(&str, Vec<u8>)],
) -> Release {
    Release {
        id,
        tag: tag.to_owned(),
        target: target.to_owned(),
        draft,
        latest,
        assets: assets
            .iter()
            .map(|(name, bytes)| ((*name).to_owned(), bytes.clone()))
            .collect(),
    }
}

/// Change the fixture's package version and make CI green at the new commit.
fn set_version(world: &World, version: &str) -> Result<()> {
    let manifest = world.fixture.work.join("Cargo.toml");
    let text = fs::read_to_string(&manifest)?.replace(VERSION, version);
    fs::write(&manifest, text)?;
    world.fixture.git(
        &world.fixture.work,
        &[
            "commit",
            "--quiet",
            "-am",
            &format!("Bump version to {version}"),
        ],
    )?;
    world.ci_green()
}

/// Run guard, the two build rows and the crate job, as preparation for a
/// publication case.
async fn prepared(world: &World) -> Result<Runner<'_>> {
    let mut runner = Runner::new(world)?;
    for job in ["guard", "build", "crate"] {
        let outcome = runner.run(job).await?;
        assert!(outcome.success(), "{job} did not succeed: {outcome:?}");
    }
    Ok(runner)
}

/// Position of the first recorded call with this prefix, or `usize::MAX`.
fn first(calls: &[String], prefix: &str) -> usize {
    calls
        .iter()
        .position(|call| call.starts_with(prefix))
        .unwrap_or(usize::MAX)
}

fn count(calls: &[String], prefix: &str) -> usize {
    calls.iter().filter(|call| call.starts_with(prefix)).count()
}

#[tokio::test]
async fn release_success() -> Result<()> {
    success::first_release().await?;
    success::numeric_version_order().await?;
    success::existing_release_is_left_alone().await?;
    success::existing_draft_is_replaced().await?;
    success::existing_registry_checksum().await?;
    Ok(())
}

/// The compiler is resolved once and reused by every later job, even when the
/// channel moves and only failed work is re-run.
#[tokio::test]
async fn compiler_reuse() -> Result<()> {
    let world = World::new()?;
    let mut runner = prepared(&world).await?;
    let crate_sha = runner
        .output("crate", "sha256")
        .ok_or_else(|| anyhow::anyhow!("the crate job reported no checksum"))?
        .to_owned();

    // The stable channel moves between the guard and the re-run.
    fs::write(world.state.join("stable-version"), "2.0.0\n")?;
    let mut probe = Runner::new(&world)?;
    assert_eq!(probe.run("guard").await?, Outcome::Success);
    assert_eq!(
        probe.output("guard", "rust"),
        Some("2.0.0"),
        "the channel did not move"
    );

    // Only the publication is re-run, and it keeps the guard's exact version.
    assert_eq!(runner.run("github").await?, Outcome::Success);
    assert_eq!(runner.run("publish").await?, Outcome::Success);
    let cargo = world.tool_calls("cargo")?;
    assert!(!cargo.is_empty(), "no cargo call was recorded");
    for call in &cargo {
        assert!(
            call.starts_with("[1.99.1]"),
            "cargo used another compiler: {call}"
        );
    }
    assert!(
        world
            .calls()
            .iter()
            .any(|call| call == "rustup toolchain install 1.99.1 --profile minimal"),
        "the exact toolchain was not installed"
    );
    assert_eq!(
        fs::read_to_string(world.state.join("curl-state/crate-fixture-1.2.3"))?,
        crate_sha,
        "the crate bytes changed with the channel"
    );
    Ok(())
}
