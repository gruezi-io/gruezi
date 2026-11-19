//! The release that must succeed: the whole pipeline in boundary order, the
//! numeric version order, and the re-runs that verify without publishing again.

use super::runner::{Outcome, Runner};
use super::world::{EXPECTED_SHA, World};
use super::{count, first, prepared, release, set_version};
use anyhow::Result;
use std::fs;

/// The whole pipeline, from the dispatched commit to a published release and a
/// registry entry, with the boundary order the plan requires.
pub(super) async fn first_release() -> Result<()> {
    let world = World::new()?;
    for (tag, annotated) in [
        ("1.2.2", true),
        ("0.9.9", false),
        ("v1.0.0", false),
        ("1.2.3-rc.1", false),
    ] {
        world.add_tag(tag, annotated, EXPECTED_SHA)?;
    }
    let origin = world.fixture.git(
        &world.fixture.origin,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    )?;
    let mut runner = Runner::new(&world)?;

    for job in ["guard", "build", "crate", "github", "publish"] {
        let outcome = runner.run(job).await?;
        assert_eq!(outcome, Outcome::Success, "job {job}");
    }

    let calls = world.calls();
    let guard_tag = first(
        &calls,
        "gh api -i repos/gruezi-io/gruezi/git/ref/tags/1.2.3",
    );
    let create = first(&calls, "gh release create 1.2.3");
    let edit = first(&calls, "gh release edit 1.2.3");
    let registry_publish = first(&calls, "cargo publish --locked --no-verify");
    let dry_run = first(&calls, "cargo publish --dry-run --locked");
    let attest = first(&calls, "attest ");
    assert!(
        guard_tag < create,
        "published before the tag gate: {calls:?}"
    );
    assert!(create < edit, "published without a draft: {calls:?}");
    assert!(
        dry_run < create,
        "published before the crate checks: {calls:?}"
    );
    assert!(attest < create, "published before attestation: {calls:?}");
    assert!(edit < registry_publish, "registry before GitHub: {calls:?}");
    assert!(
        count(
            &calls,
            "gh api -i repos/gruezi-io/gruezi/git/ref/tags/1.2.3"
        ) >= 2,
        "the target tag was not rechecked before publishing: {calls:?}"
    );
    // Nothing else was tagged, moved or pushed.
    assert_eq!(
        world.fixture.git(
            &world.fixture.origin,
            &["for-each-ref", "--format=%(refname) %(objectname)"]
        )?,
        origin
    );
    // The release is public, latest, at the dispatched commit, with its files.
    let releases = fs::read_to_string(world.state.join("gh-state/releases.json"))?;
    assert!(releases.contains(r#""tag_name":"1.2.3""#), "{releases}");
    assert!(releases.contains(r#""draft":false"#), "{releases}");
    assert_eq!(
        fs::read_to_string(world.state.join("gh-state/latest"))?,
        "1.2.3"
    );
    let tag = fs::read_to_string(world.state.join("gh-state/tagrefs/1.2.3"))?;
    assert!(tag.contains(&world.head()?), "{tag}");
    let files = world.published("1.2.3")?;
    let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "SHA256SUMS",
            "fixture-1.2.3-1.aarch64.rpm",
            "fixture-1.2.3-1.x86_64.rpm",
            "fixture-1.2.3-aarch64-unknown-linux-musl.tar.gz",
            "fixture-1.2.3-x86_64-unknown-linux-musl.tar.gz",
            "fixture_1.2.3-1_amd64.deb",
            "fixture_1.2.3-1_arm64.deb",
        ]
    );
    // The registry reports exactly the verified crate.
    let crate_sha = runner
        .output("crate", "sha256")
        .ok_or_else(|| anyhow::anyhow!("the crate job reported no checksum"))?;
    assert_eq!(
        fs::read_to_string(world.state.join("curl-state/crate-fixture-1.2.3"))?,
        crate_sha
    );
    Ok(())
}

/// 0.10.0 is a release above 0.9.9, and a noncanonical tag is not in the way.
pub(super) async fn numeric_version_order() -> Result<()> {
    let world = World::new()?;
    set_version(&world, "0.10.0")?;
    world.add_tag("0.9.9", false, EXPECTED_SHA)?;
    world.add_tag("0.10.0-rc.1", false, EXPECTED_SHA)?;
    let mut runner = Runner::new(&world)?;
    for job in ["guard", "build", "crate", "github", "publish"] {
        assert_eq!(runner.run(job).await?, Outcome::Success, "{job}");
    }
    assert_eq!(
        fs::read_to_string(world.state.join("gh-state/latest"))?,
        "0.10.0"
    );
    Ok(())
}

/// A published release that already carries the right bytes is verified and
/// left alone: no create, no edit and no Latest change.
pub(super) async fn existing_release_is_left_alone() -> Result<()> {
    let world = World::new()?;
    let mut runner = prepared(&world).await?;
    assert_eq!(runner.run("github").await?, Outcome::Success);
    let files = world.published("1.2.3")?;
    // A re-run may find the target tag annotated as well as lightweight.
    world.add_tag("1.2.3", true, &world.head()?)?;
    let calls_after_first = world.calls().len();

    assert_eq!(runner.run("github").await?, Outcome::Success);
    let calls = world.calls();
    assert_eq!(
        count(&calls, "gh release create 1.2.3"),
        1,
        "the published release was recreated: {calls:?}"
    );
    assert_eq!(
        count(&calls, "gh release edit 1.2.3"),
        1,
        "the published release was edited: {calls:?}"
    );
    assert_eq!(count(&calls, "gh api -X DELETE"), 0);
    assert!(
        calls.len() > calls_after_first,
        "the re-run verified nothing"
    );
    assert_eq!(
        fs::read_to_string(world.state.join("gh-state/latest"))?,
        "1.2.3"
    );
    assert_eq!(world.published("1.2.3")?, files);
    Ok(())
}

/// A draft of this run's own failed attempt is replaced, then published.
pub(super) async fn existing_draft_is_replaced() -> Result<()> {
    let world = World::new()?;
    let mut runner = prepared(&world).await?;
    let head = world.head()?;
    world.add_release(&release(
        50,
        "1.2.3",
        &head,
        true,
        false,
        &[("fixture-1.2.3-1.x86_64.rpm", b"stale".to_vec())],
    ))?;
    assert_eq!(runner.run("github").await?, Outcome::Success);
    let calls = world.calls();
    assert_eq!(
        count(
            &calls,
            "gh api -X DELETE repos/gruezi-io/gruezi/releases/50"
        ),
        1
    );
    assert_eq!(world.published("1.2.3")?.len(), 7);
    Ok(())
}

/// A registry version that already carries the verified crate needs no token
/// and no publish.
pub(super) async fn existing_registry_checksum() -> Result<()> {
    let world = World::new()?;
    let mut runner = prepared(&world).await?;
    assert_eq!(runner.run("github").await?, Outcome::Success);
    assert_eq!(runner.run("publish").await?, Outcome::Success);
    let checksum = fs::read_to_string(world.state.join("curl-state/crate-fixture-1.2.3"))?;
    let calls_after_first = world.calls().len();

    assert_eq!(runner.run("publish").await?, Outcome::Success);
    let calls = world.calls();
    assert_eq!(
        count(&calls, "cargo publish --locked --no-verify"),
        1,
        "an accepted version was published again: {calls:?}"
    );
    assert!(
        calls.len() > calls_after_first,
        "the re-run checked nothing"
    );
    assert_eq!(
        fs::read_to_string(world.state.join("curl-state/crate-fixture-1.2.3"))?,
        checksum
    );
    Ok(())
}
