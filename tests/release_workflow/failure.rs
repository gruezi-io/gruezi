//! Failure and recovery cases: everything that must stop a release before or
//! around publication, and the failed-job retry path that must not publish
//! twice or move a tag. The structural gates (needs graph, YAML parsing and
//! Bash syntax) live here too.

use super::runner::{Outcome, Runner};
use super::world::{CiRun, EXPECTED_SHA, World, ci_jobs, ci_jobs_with_coverage_failure};
use super::{VERSION, prepared, set_version};
use anyhow::Result;
use std::fs;

/// The guard refused at `step`, and no job below it ran or published.
async fn refused(runner: &mut Runner<'_>, world: &World, step: &str) -> Result<()> {
    let outcome = runner.run("guard").await?;
    assert!(outcome.failed(step), "guard outcome: {outcome:?}");
    for job in ["build", "crate", "github", "publish"] {
        assert_eq!(runner.run(job).await?, Outcome::Skipped, "{job}");
    }
    no_publication(world)
}

/// Fail when a call reached a publication boundary.
pub(super) fn no_publication(world: &World) -> Result<()> {
    let calls = world.calls();
    for call in &calls {
        for boundary in [
            "gh release ",
            "gh api -X DELETE",
            "cargo publish --locked --no-verify",
            "auth",
        ] {
            if call.starts_with(boundary) {
                return Err(anyhow::anyhow!("published early: {calls:?}"));
            }
        }
    }
    Ok(())
}

fn script_ci_for(world: &World, head: &str, ci_ok: &str) -> Result<()> {
    let run = CiRun::green(11, head);
    world.script_ci(std::slice::from_ref(&run), &[ci_jobs(&run, ci_ok)])
}

#[tokio::test]
async fn release_failure_guard() -> Result<()> {
    guard_dispatch_inputs().await?;
    guard_ancestry().await?;
    guard_trusted_ci().await?;
    Ok(())
}

/// The dispatch itself is wrong: another ref, another commit, a malformed
/// marker, or a run that is no longer the commit it names.
async fn guard_dispatch_inputs() -> Result<()> {
    // Wrong dispatch inputs.
    let world = World::new()?;
    let mut runner = Runner::new(&world)?;
    runner.ref_name = "refs/heads/main".to_owned();
    refused(&mut runner, &world, "check").await?;

    let world = World::new()?;
    let mut runner = Runner::new(&world)?;
    runner.expected_sha = "00112233445566778899aabbccddeeff00112233".to_owned();
    refused(&mut runner, &world, "check").await?;

    let world = World::new()?;
    let mut runner = Runner::new(&world)?;
    runner.expected_sha = "abc".to_owned();
    refused(&mut runner, &world, "check").await?;

    let world = World::new()?;
    let mut runner = Runner::new(&world)?;
    runner.dispatch_id = "marker;touch-x".to_owned();
    refused(&mut runner, &world, "check").await?;

    // develop moved on between resolving the commit and dispatching the run:
    // the run is on the new tip, expected_sha names the old commit.
    let world = World::new()?;
    let before = world.head()?;
    world.fixture.git(
        &world.fixture.other,
        &["commit", "--quiet", "--allow-empty", "-m", "Later work"],
    )?;
    world.fixture.git(
        &world.fixture.other,
        &["push", "--quiet", "origin", "develop"],
    )?;
    let moved = world
        .fixture
        .git(&world.fixture.other, &["rev-parse", "HEAD"])?;
    world
        .fixture
        .git(&world.fixture.work, &["fetch", "--quiet", "origin"])?;
    world.fixture.git(
        &world.fixture.work,
        &["switch", "--quiet", "--detach", &moved],
    )?;
    let mut runner = Runner::new(&world)?;
    runner.expected_sha = before;
    refused(&mut runner, &world, "check").await?;

    Ok(())
}

/// The dispatched commit no longer satisfies the branch contract.
async fn guard_ancestry() -> Result<()> {
    // main moved ahead of develop.
    let world = World::new()?;
    world.fixture.git(
        &world.fixture.other,
        &["commit", "--quiet", "--allow-empty", "-m", "Main work"],
    )?;
    world.fixture.git(
        &world.fixture.other,
        &["push", "--quiet", "origin", "develop:refs/heads/main"],
    )?;
    world
        .fixture
        .git(&world.fixture.work, &["fetch", "--quiet", "origin"])?;
    let mut runner = Runner::new(&world)?;
    refused(&mut runner, &world, "check").await?;

    Ok(())
}

/// Only this repository's successful `build.yml` push run of this commit on
/// develop counts as trusted CI.
async fn guard_trusted_ci() -> Result<()> {
    // A green run of the parent commit proves nothing about this one.
    let world = World::new()?;
    let parent = world.head()?;
    world.fixture.git(
        &world.fixture.other,
        &["commit", "--quiet", "--allow-empty", "-m", "Release work"],
    )?;
    world.fixture.git(
        &world.fixture.other,
        &["push", "--quiet", "origin", "develop"],
    )?;
    let head = world
        .fixture
        .git(&world.fixture.other, &["rev-parse", "HEAD"])?;
    world
        .fixture
        .git(&world.fixture.work, &["fetch", "--quiet", "origin"])?;
    world.fixture.git(
        &world.fixture.work,
        &["switch", "--quiet", "--detach", &head],
    )?;
    script_ci_for(&world, &parent, "success")?;
    refused(&mut Runner::new(&world)?, &world, "ci").await?;

    // A green run of another workflow, event, branch or repository.
    for (field, value) in [
        ("workflow", ".github/workflows/other.yml"),
        ("event", "pull_request"),
        ("branch", "feature"),
        ("repo", "someone/gruezi"),
    ] {
        let world = World::new()?;
        let mut run = CiRun::green(10, &world.head()?);
        match field {
            "workflow" => run.workflow = value.to_owned(),
            "event" => run.event = value.to_owned(),
            "branch" => run.branch = value.to_owned(),
            "repo" => run.repo = value.to_owned(),
            _ => return Err(anyhow::anyhow!("unknown CI field {field}")),
        }
        world.script_ci(std::slice::from_ref(&run), &[ci_jobs(&run, "success")])?;
        refused(&mut Runner::new(&world)?, &world, "ci").await?;
    }

    // A repository field that is null, missing, malformed, or a string must
    // fail closed instead of being treated as this repository.
    for repository_json in [
        "null",
        r#"{"name":"gruezi"}"#,
        r#""gruezi-io/gruezi""#,
        r#"{"full_name":null}"#,
    ] {
        let world = World::new()?;
        let mut run = CiRun::green(10, &world.head()?);
        run.repository_json = repository_json.to_owned();
        world.script_ci(std::slice::from_ref(&run), &[ci_jobs(&run, "success")])?;
        refused(&mut Runner::new(&world)?, &world, "ci").await?;
    }

    // The right run with a CI OK that did not pass.
    let world = World::new()?;
    script_ci_for(&world, &world.head()?, "failure")?;
    refused(&mut Runner::new(&world)?, &world, "ci").await?;

    // CI OK is the release contract; coverage may still be running or may
    // fail independently after CI OK has succeeded.
    let world = World::new()?;
    let mut run = CiRun::green(10, &world.head()?);
    run.status = "in_progress".to_owned();
    world.script_ci(
        std::slice::from_ref(&run),
        &[ci_jobs_with_coverage_failure(&run)],
    )?;
    assert!(
        Runner::new(&world)?.run("guard").await?.success(),
        "successful CI OK was blocked by unfinished coverage"
    );

    // A missing or incomplete CI OK still fails closed.
    for jobs in [
        r#"[{"id":102,"name":"Coverage","status":"completed","conclusion":"success","head_sha":"x"}]"#,
        r#"[{"id":101,"name":"CI OK","status":"in_progress","conclusion":null,"head_sha":"x"}]"#,
    ] {
        let world = World::new()?;
        let run = CiRun::green(10, &world.head()?);
        world.script_ci(std::slice::from_ref(&run), &[jobs.to_owned()])?;
        refused(&mut Runner::new(&world)?, &world, "ci").await?;
    }

    // The CI endpoints cannot be read.
    let world = World::new()?;
    world.gh_fail("get-actions_workflows_build.yml_runs", "503")?;
    refused(&mut Runner::new(&world)?, &world, "ci").await?;

    let world = World::new()?;
    world.gh_fail("get-actions_runs_10_jobs", "")?;
    refused(&mut Runner::new(&world)?, &world, "ci").await?;

    Ok(())
}

#[tokio::test]
async fn release_failure_version_gate() -> Result<()> {
    // The target tag already exists.
    for annotated in [false, true] {
        let world = World::new()?;
        world.add_tag(VERSION, annotated, EXPECTED_SHA)?;
        refused(&mut Runner::new(&world)?, &world, "tag").await?;
    }

    // A higher version is already released.
    let world = World::new()?;
    world.add_tag("1.10.0", false, EXPECTED_SHA)?;
    refused(&mut Runner::new(&world)?, &world, "tag").await?;

    // A noncanonical version is not a release.
    let world = World::new()?;
    set_version(&world, "1.2.3-rc.1")?;
    refused(&mut Runner::new(&world)?, &world, "check").await?;

    let world = World::new()?;
    set_version(&world, "01.2.3")?;
    refused(&mut Runner::new(&world)?, &world, "check").await?;

    // Only a definite 404 is an absence: every other API answer stops the run.
    let world = World::new()?;
    world.gh_fail("get-git_ref_tags_1.2.3", "")?;
    refused(&mut Runner::new(&world)?, &world, "tag").await?;

    let world = World::new()?;
    world.gh_fail("get-git_ref_tags_1.2.3", "403")?;
    refused(&mut Runner::new(&world)?, &world, "tag").await?;

    let world = World::new()?;
    world.gh_fail("get-tags", "503")?;
    refused(&mut Runner::new(&world)?, &world, "tag").await?;

    Ok(())
}

#[tokio::test]
async fn release_failure_prerequisites() -> Result<()> {
    // A build or package step that fails stops the run before any publication.
    for (action, target, step) in [
        (
            "build-binary",
            "x86_64-unknown-linux-musl",
            "Build the binary",
        ),
        (
            "linux-packages",
            "aarch64-unknown-linux-musl",
            "Build and check the Linux packages",
        ),
    ] {
        let world = World::new()?;
        let mut runner = Runner::new(&world)?;
        runner
            .failed_uses
            .push((action.to_owned(), target.to_owned()));
        assert_eq!(runner.run("guard").await?, Outcome::Success);
        assert_eq!(runner.run("build").await?, Outcome::Failed(step.to_owned()));
        // The crate job does not wait for the builds; only publication does.
        assert_eq!(runner.run("crate").await?, Outcome::Success);
        assert_eq!(runner.run("github").await?, Outcome::Skipped);
        assert_eq!(runner.run("publish").await?, Outcome::Skipped);
        no_publication(&world)?;
    }

    // The crate job's own checks also gate the publication.
    for (control, step) in [("fail-package", "package"), ("fail-dry-run", "package")] {
        let world = World::new()?;
        world.inject(control)?;
        let mut runner = Runner::new(&world)?;
        assert_eq!(runner.run("guard").await?, Outcome::Success);
        assert_eq!(runner.run("build").await?, Outcome::Success);
        assert_eq!(runner.run("crate").await?, Outcome::Failed(step.to_owned()));
        assert_eq!(runner.run("github").await?, Outcome::Skipped);
        assert_eq!(runner.run("publish").await?, Outcome::Skipped);
        no_publication(&world)?;
    }
    Ok(())
}

#[tokio::test]
async fn release_failure_inventory() -> Result<()> {
    // A missing or stray file is not an inventory this workflow may publish.
    for (mutate, step) in [
        ("missing", "inventory"),
        ("extra", "inventory"),
        ("all", "Download the release files"),
    ] {
        let world = World::new()?;
        let mut runner = prepared(&world).await?;
        let artifact = world.state.join("artifacts/dist-x86_64-unknown-linux-musl");
        match mutate {
            "missing" => fs::remove_file(artifact.join("fixture_1.2.3-1_amd64.deb"))?,
            "extra" => fs::write(artifact.join("stray.txt"), "not a release file\n")?,
            _ => fs::remove_dir_all(world.state.join("artifacts"))?,
        }
        assert_eq!(
            runner.run("github").await?,
            Outcome::Failed(step.to_owned())
        );
        no_publication(&world)?;
    }

    // The crate job's checksum must match what the release downloads.
    let world = World::new()?;
    let mut runner = prepared(&world).await?;
    fs::write(
        world.state.join("artifacts/crate/fixture-1.2.3.crate"),
        b"different crate",
    )?;
    assert_eq!(
        runner.run("github").await?,
        Outcome::Failed("crate".to_owned())
    );
    no_publication(&world)?;
    Ok(())
}
