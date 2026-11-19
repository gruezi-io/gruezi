//! Publication failure and recovery cases: what must stop a release around
//! the moment it becomes public, and the failed-job retries that must never
//! publish twice, replace a tag or change bytes.

use super::failure::no_publication;
use super::runner::Outcome;
use super::world::{EXPECTED_SHA, World};
use super::{prepared, release};
use anyhow::Result;
use std::fs;

#[tokio::test]
async fn release_failure_stale_run() -> Result<()> {
    // A newer version published while this run was building: the recheck stops
    // it before GitHub creates the tag, and the newer release keeps Latest.
    let world = World::new()?;
    let mut runner = prepared(&world).await?;
    let head = world.head()?;
    world.add_tag("1.3.0", false, EXPECTED_SHA)?;
    world.add_release(&release(20, "1.3.0", EXPECTED_SHA, false, true, &[]))?;
    assert_eq!(
        runner.run("github").await?,
        Outcome::Failed("gate".to_owned())
    );
    no_publication(&world)?;
    assert!(!world.state.join("gh-state/tagrefs/1.2.3").exists());
    assert_eq!(
        fs::read_to_string(world.state.join("gh-state/latest"))?,
        "1.3.0"
    );
    assert_eq!(head, world.head()?);
    Ok(())
}

#[tokio::test]
async fn release_failure_existing_release() -> Result<()> {
    let world = World::new()?;
    let mut runner = prepared(&world).await?;
    let head = world.head()?;
    let good = [
        ("SHA256SUMS", b"sums".to_vec()),
        ("fixture-1.2.3-1.aarch64.rpm", b"rpm".to_vec()),
    ];
    // Wrong tag target, then wrong asset sets and bytes.
    world.add_tag("1.2.3", false, "00112233445566778899aabbccddeeff00112233")?;
    world.add_release(&release(30, "1.2.3", &head, false, false, &good))?;
    assert_eq!(
        runner.run("github").await?,
        Outcome::Failed("verify".to_owned())
    );

    let world = World::new()?;
    let mut runner = prepared(&world).await?;
    world.add_tag("1.2.3", true, "00112233445566778899aabbccddeeff00112233")?;
    world.add_release(&release(31, "1.2.3", &world.head()?, false, false, &good))?;
    assert_eq!(
        runner.run("github").await?,
        Outcome::Failed("verify".to_owned())
    );

    for (mutate, assets) in [
        ("extra", vec![("stray.bin", b"x".to_vec())]),
        ("missing", vec![]),
        ("bytes", vec![("SHA256SUMS", b"tampered".to_vec())]),
    ] {
        let world = World::new()?;
        let mut runner = prepared(&world).await?;
        world.add_tag("1.2.3", false, &world.head()?)?;
        let mut files = good.to_vec();
        match mutate {
            "extra" => files.extend(assets),
            "missing" => files.truncate(1),
            _ => {
                let Some(tampered) = assets.first() else {
                    continue;
                };
                files.retain(|(name, _)| name != &tampered.0);
                files.push(tampered.clone());
            }
        }
        world.add_release(&release(32, "1.2.3", &world.head()?, false, false, &files))?;
        assert_eq!(
            runner.run("github").await?,
            Outcome::Failed("verify".to_owned())
        );
        no_publication(&world)?;
    }

    // A draft that is not ours to replace is left alone.
    let world = World::new()?;
    let mut runner = prepared(&world).await?;
    world.add_release(&release(
        33,
        "1.2.3",
        "00112233445566778899aabbccddeeff00112233",
        true,
        false,
        &good,
    ))?;
    assert_eq!(
        runner.run("github").await?,
        Outcome::Failed("draft".to_owned())
    );
    no_publication(&world)?;
    Ok(())
}

#[tokio::test]
async fn release_failure_api_outage() -> Result<()> {
    for (operation, step, tagged) in [
        ("get-releases", "existing", false),
        ("release-create", "create", false),
        ("release-edit", "publish", false),
        ("release-download", "verify", true),
    ] {
        let world = World::new()?;
        let mut runner = prepared(&world).await?;
        world.gh_fail(operation, "500")?;
        assert_eq!(
            runner.run("github").await?,
            Outcome::Failed(step.to_owned())
        );
        // The only publication that may already have happened is the one the
        // outage interrupted: at most one release of this version, and the tag
        // only once GitHub accepted the publication.
        let calls = world.calls();
        assert!(
            super::count(&calls, "gh release create 1.2.3") <= 1,
            "published twice despite the outage: {calls:?}"
        );
        assert_eq!(
            world.state.join("gh-state/tagrefs/1.2.3").exists(),
            tagged,
            "unexpected tag state after the outage: {calls:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn release_failure_crate_and_registry() -> Result<()> {
    // The downloaded crate is not the verified crate.
    let world = World::new()?;
    let mut runner = prepared(&world).await?;
    assert_eq!(runner.run("github").await?, Outcome::Success);
    fs::write(
        world.state.join("artifacts/crate/fixture-1.2.3.crate"),
        b"different crate",
    )?;
    assert_eq!(
        runner.run("publish").await?,
        Outcome::Failed("downloaded".to_owned())
    );

    // The exact compiler must reproduce the verified bytes.
    let world = World::new()?;
    let mut runner = prepared(&world).await?;
    assert_eq!(runner.run("github").await?, Outcome::Success);
    world.inject("reproduce-differs")?;
    assert_eq!(
        runner.run("publish").await?,
        Outcome::Failed("repackage".to_owned())
    );

    // The registry cannot be asked: only a definite 404 is an absence.
    for (control, step) in [("outage", "registry"), ("status", "registry")] {
        let world = World::new()?;
        let mut runner = prepared(&world).await?;
        assert_eq!(runner.run("github").await?, Outcome::Success);
        match control {
            "outage" => world.registry_outage()?,
            _ => world.registry_status("500")?,
        }
        assert_eq!(
            runner.run("publish").await?,
            Outcome::Failed(step.to_owned())
        );
    }

    // An existing version with different bytes must not be overwritten.
    let world = World::new()?;
    let mut runner = prepared(&world).await?;
    assert_eq!(runner.run("github").await?, Outcome::Success);
    world.registry_set(
        "fixture",
        "1.2.3",
        "0000000000000000000000000000000000000000000000000000000000000000",
    )?;
    assert_eq!(
        runner.run("publish").await?,
        Outcome::Failed("registry".to_owned())
    );
    assert_eq!(
        super::count(&world.calls(), "cargo publish --locked --no-verify"),
        0
    );

    // A checksum the registry reports after accepting differs from the verified
    // crate: fail loudly, do not wait forever or hide it.
    let world = World::new()?;
    let mut runner = prepared(&world).await?;
    assert_eq!(runner.run("github").await?, Outcome::Success);
    world.inject("registry-corrupt")?;
    assert_eq!(
        runner.run("publish").await?,
        Outcome::Failed("publish".to_owned())
    );
    assert_eq!(super::count(&world.calls(), "sleep 10"), 0);
    Ok(())
}

#[tokio::test]
async fn release_failure_registry_wait() -> Result<()> {
    // The bounded wait is driven by the fake clock: no real delay, and a
    // registry that never catches up fails with the loop exhausted.
    let world = World::new()?;
    let mut runner = prepared(&world).await?;
    assert_eq!(runner.run("github").await?, Outcome::Success);
    world.inject("registry-delay")?;
    fs::write(world.state.join("registry-delay"), "999\n")?;
    assert_eq!(
        runner.run("publish").await?,
        Outcome::Failed("publish".to_owned())
    );
    assert_eq!(super::count(&world.calls(), "sleep 10"), 30);

    // A registry that catches up after a few answers succeeds.
    let world = World::new()?;
    let mut runner = prepared(&world).await?;
    assert_eq!(runner.run("github").await?, Outcome::Success);
    fs::write(world.state.join("registry-delay"), "2\n")?;
    assert_eq!(runner.run("publish").await?, Outcome::Success);
    assert_eq!(super::count(&world.calls(), "sleep 10"), 2);
    Ok(())
}

#[tokio::test]
async fn release_failure_retry_after_publication() -> Result<()> {
    // A run that dies right after GitHub accepted the release is recovered by
    // re-running its failed jobs: no second release, no replaced tag, same bytes.
    let world = World::new()?;
    let mut runner = prepared(&world).await?;
    runner.stop_after = Some(("github".to_owned(), "publish".to_owned()));
    assert_eq!(runner.run("github").await?, Outcome::Success);
    let files = world.published("1.2.3")?;
    runner.stop_after = None;
    assert_eq!(runner.run("github").await?, Outcome::Success);
    let calls = world.calls();
    assert_eq!(super::count(&calls, "gh release create 1.2.3"), 1);
    assert_eq!(super::count(&calls, "gh release edit 1.2.3"), 1);
    assert_eq!(super::count(&calls, "gh api -X DELETE"), 0);
    assert_eq!(world.published("1.2.3")?, files);
    assert_eq!(runner.run("publish").await?, Outcome::Success);

    // A run that dies after the registry accepted the crate is recovered the
    // same way: the accepted version is recognized, not published again.
    let world = World::new()?;
    let mut runner = prepared(&world).await?;
    assert_eq!(runner.run("github").await?, Outcome::Success);
    world.inject("crash-after-accept")?;
    assert_eq!(
        runner.run("publish").await?,
        Outcome::Failed("publish".to_owned())
    );
    let checksum = fs::read_to_string(world.state.join("curl-state/crate-fixture-1.2.3"))?;
    world.clear("crash-after-accept")?;
    assert_eq!(runner.run("publish").await?, Outcome::Success);
    let calls = world.calls();
    assert_eq!(
        super::count(&calls, "cargo publish --locked --no-verify"),
        1
    );
    assert_eq!(
        fs::read_to_string(world.state.join("curl-state/crate-fixture-1.2.3"))?,
        checksum
    );
    Ok(())
}
