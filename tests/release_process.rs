//! Behavioral tests for the release recipes in `.justfile`.
//!
//! The real recipes run against disposable repositories (see
//! `release_support`). Outcomes are read from refs, commits, files and the
//! fake-tool call logs, never from the recipes' printed prose.

mod release_support;

#[path = "release_support/gh_contract.rs"]
mod gh_contract;

use anyhow::{Result, anyhow};
use release_support::{ReleaseFixture, text, write_executable};
use std::{fs, process::Output};

const REFUSAL_MARKER: &str = "bump: ";
const RETRY_PUSH: &str = "git push origin HEAD:refs/heads/develop";

fn configure_release_gh(fixture: &ReleaseFixture, mode: &str) -> Result<()> {
    let contract = include_str!("release_support/gh_contract.sh");
    let script = format!(
        r#"{contract}
case "${{1:-}} ${{2:-}}" in
  "repo view")
    [[ "{mode}" != api-error ]] || {{ echo "fixture API outage" >&2; exit 1; }}
    if [[ "{mode}" == moved ]]; then
      git -C "$ctl/../other" push --quiet origin moving:develop
    fi
    printf 'fixture/repo\n'
    ;;
  "workflow run")
    for arg in "$@"; do
      case "$arg" in
        dispatch_id=*) printf '%s' "${{arg#dispatch_id=}}" > "$ctl/dispatch-id" ;;
      esac
    done
    if [[ "{mode}" == moved ]]; then
      git --git-dir="$ctl/../origin.git" rev-parse refs/heads/develop > "$ctl/selected-sha"
    fi
    ;;
  "run list")
    sha="$(cat "$ctl/selected-sha")"
    marker="$(cat "$ctl/dispatch-id")"
    [[ "{mode}" != wrong-identity && "{mode}" != missing-run ]] || exit 1
    if [[ "$*" == *"build.yml"* ]]; then
      printf '8\n'
    elif [[ "{mode}" == competing ]]; then
      if [[ "$*" == *".name == "* ]]; then
        printf '7\n'
      else
        printf '6\n'
      fi
    elif [[ "{mode}" == missing-run ]]; then
      printf '\n'
    elif [[ "{mode}" == wrong-identity ]]; then
      printf '\n'
    else
      printf '7\n'
    fi
    ;;
  "run view")
    sha="$(cat "$ctl/selected-sha")"
    marker="$(cat "$ctl/dispatch-id")"
    printf 'https://fixture/runs/7\n'
    ;;
  "run watch")
    [[ "{mode}" != failed-watch ]] || exit 1
    [[ "{mode}" != hanging-watch ]] || sleep 30
    if [[ "{mode}" == recover-after-first && ! -f "$ctl/watched" ]]; then
      touch "$ctl/watched"
      exit 1
    fi
    ;;
  *) echo "unknown gh command: $*" >&2; exit 2 ;;
esac
"#
    );
    write_executable(&fixture.bin.join("gh"), &script)?;
    fs::write(
        fixture.bin.join("selected-sha"),
        fixture.git(&fixture.work, &["rev-parse", "origin/develop"])?,
    )?;
    Ok(())
}

/// Recipe-test helpers over the shared fixture. They live here, not in
/// `release_support`, because only the recipe tests use them.
trait RecipeFixture {
    /// Run `just <recipe>` in the working clone.
    async fn just(&self, recipe: &str) -> Result<Output>;
    /// Every ref on the fixture remote with its object id.
    fn origin_refs(&self) -> Result<String>;
    /// Remote refs other than `refs/heads/develop`.
    fn origin_refs_except_develop(&self) -> Result<String>;
    /// Switch on a fake-tool control file (see the fixture docs).
    fn inject(&self, control: &str) -> Result<()>;
    /// Make the remote refuse every push, like a protected branch would.
    fn reject_pushes(&self) -> Result<()>;
    /// Contents of a file in the working clone.
    fn read(&self, name: &str) -> Result<String>;
}

impl RecipeFixture for ReleaseFixture {
    async fn just(&self, recipe: &str) -> Result<Output> {
        self.run("just", &[recipe], &[]).await
    }

    fn origin_refs(&self) -> Result<String> {
        self.git(
            &self.origin,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        )
    }

    fn origin_refs_except_develop(&self) -> Result<String> {
        Ok(self
            .origin_refs()?
            .lines()
            .filter(|line| !line.starts_with("refs/heads/develop "))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn inject(&self, control: &str) -> Result<()> {
        Ok(fs::write(self.bin.join(control), "")?)
    }

    fn reject_pushes(&self) -> Result<()> {
        write_executable(
            &self.origin.join("hooks").join("pre-receive"),
            "#!/bin/sh\necho 'rejected by the fixture remote' >&2\nexit 1\n",
        )
    }

    fn read(&self, name: &str) -> Result<String> {
        Ok(fs::read_to_string(self.work.join(name))?)
    }
}

/// State a refused bump must not disturb.
struct Snapshot {
    head: String,
    origin: String,
    status: String,
}

fn snapshot(fixture: &ReleaseFixture) -> Result<Snapshot> {
    Ok(Snapshot {
        head: fixture.git(&fixture.work, &["rev-parse", "HEAD"])?,
        origin: fixture.origin_refs()?,
        status: fixture.git(
            &fixture.work,
            &["status", "--porcelain", "--untracked-files=all"],
        )?,
    })
}

/// Run `just bump`, expect a failure, and return the recipe output.
async fn refused(fixture: &ReleaseFixture) -> Result<String> {
    let output = fixture.just("bump").await?;
    let log = text(&output);
    println!("== just bump (expected failure)\n{log}");
    assert!(!output.status.success(), "bump unexpectedly succeeded");
    // Every refusal path of the recipe prefixes its report; a missing recipe or an
    // unrelated crash would fail too, but never with this marker.
    assert!(
        log.contains(REFUSAL_MARKER),
        "failure did not come from the bump recipe"
    );
    Ok(log)
}

/// The bump stopped before touching anything: nothing moved, no tool ran.
async fn assert_refused_untouched(fixture: &ReleaseFixture) -> Result<()> {
    let before = snapshot(fixture)?;
    refused(fixture).await?;
    let after = snapshot(fixture)?;
    assert_eq!(before.head, after.head);
    assert_eq!(before.origin, after.origin);
    assert_eq!(before.status, after.status);
    assert!(
        !fixture
            .calls("cargo")?
            .iter()
            .any(|call| call.starts_with("set-version")),
        "version tool ran despite the refusal"
    );
    Ok(())
}

/// The bump started editing, failed, and put back its own edits: no commit,
/// no push, version files identical to HEAD. Returns the post-failure status
/// and the recipe output.
async fn assert_refused_restored(fixture: &ReleaseFixture) -> Result<(String, String)> {
    let before = snapshot(fixture)?;
    let log = refused(fixture).await?;
    let after = snapshot(fixture)?;
    assert_eq!(before.head, after.head);
    assert_eq!(before.origin, after.origin);
    for diff in [&["diff"][..], &["diff", "--cached"][..]] {
        let mut args = diff.to_vec();
        args.extend(["--name-only", "--", "Cargo.toml", "Cargo.lock"]);
        assert_eq!(fixture.git(&fixture.work, &args)?, "");
    }
    assert!(fixture.read("Cargo.toml")?.contains("version = \"1.2.3\""));
    assert!(
        fixture
            .calls("cargo")?
            .iter()
            .any(|call| call.starts_with("set-version")),
        "the failure happened before the bump started"
    );
    Ok((after.status, log))
}

#[tokio::test]
async fn bump_success() -> Result<()> {
    let cases = [
        ("bump", "patch", "1.2.4"),
        ("bump-minor", "minor", "1.3.0"),
        ("bump-major", "major", "2.0.0"),
    ];
    for (recipe, kind, version) in cases {
        let fixture = ReleaseFixture::new()?;
        let base = fixture.git(&fixture.work, &["rev-parse", "HEAD"])?;
        let other_refs = fixture.origin_refs_except_develop()?;

        let output = fixture.just(recipe).await?;
        println!("== just {recipe}\n{}", text(&output));
        assert!(output.status.success());

        let work = &fixture.work;
        let head = fixture.git(work, &["rev-parse", "HEAD"])?;
        assert_eq!(fixture.git(work, &["rev-parse", "HEAD^"])?, base);
        assert_eq!(
            fixture.git(&fixture.origin, &["rev-parse", "refs/heads/develop"])?,
            head
        );
        assert_eq!(
            fixture.git(
                work,
                &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"]
            )?,
            "Cargo.lock\nCargo.toml"
        );
        assert_eq!(fixture.git(work, &["log", "-1", "--format=%G?"])?, "G");
        assert!(
            fixture
                .read("Cargo.toml")?
                .contains(&format!("version = \"{version}\""))
        );
        let lock = fixture.read("Cargo.lock")?;
        assert!(lock.contains(&format!("name = \"fixture\"\nversion = \"{version}\"")));
        assert!(lock.contains("# refreshed 1"));
        assert_eq!(fixture.origin_refs_except_develop()?, other_refs);
        assert_eq!(fixture.git(work, &["tag", "--list"])?, "1.2.2");
        assert_eq!(fixture.git(work, &["status", "--porcelain"])?, "");
        assert_eq!(
            fixture.calls("cargo")?,
            [
                format!("set-version --bump {kind}").as_str(),
                "update",
                "clippy --all-targets --all-features",
                "fmt --all -- --check",
                "test",
            ]
        );
        assert_eq!(fixture.calls("gh")?, Vec::<String>::new());
    }
    Ok(())
}

#[tokio::test]
async fn bump_failure_dirty_worktree() -> Result<()> {
    for dirt in ["staged", "unstaged", "untracked"] {
        let fixture = ReleaseFixture::new()?;
        match dirt {
            "staged" => {
                fs::write(fixture.work.join("README.md"), "staged\n")?;
                fixture.git(&fixture.work, &["add", "README.md"])?;
            }
            "unstaged" => fs::write(fixture.work.join("README.md"), "unstaged\n")?,
            _ => fs::write(fixture.work.join("notes.txt"), "untracked\n")?,
        }
        assert_refused_untouched(&fixture).await?;
    }
    Ok(())
}

#[tokio::test]
async fn bump_failure_wrong_branch() -> Result<()> {
    let fixture = ReleaseFixture::new()?;
    fixture.git(&fixture.work, &["switch", "--quiet", "-c", "feature"])?;
    assert_refused_untouched(&fixture).await?;

    let fixture = ReleaseFixture::new()?;
    fixture.git(&fixture.work, &["switch", "--quiet", "--detach"])?;
    assert_refused_untouched(&fixture).await
}

#[tokio::test]
async fn bump_failure_stale_origin() -> Result<()> {
    // Origin moved on: local develop is behind.
    let fixture = ReleaseFixture::new()?;
    fixture.git(
        &fixture.other,
        &["commit", "--quiet", "--allow-empty", "-m", "Remote work"],
    )?;
    fixture.git(&fixture.other, &["push", "--quiet", "origin", "develop"])?;
    assert_refused_untouched(&fixture).await?;

    // Both sides moved: diverged.
    let fixture = ReleaseFixture::new()?;
    fixture.git(
        &fixture.other,
        &["commit", "--quiet", "--allow-empty", "-m", "Remote work"],
    )?;
    fixture.git(&fixture.other, &["push", "--quiet", "origin", "develop"])?;
    fixture.git(
        &fixture.work,
        &["commit", "--quiet", "--allow-empty", "-m", "Local work"],
    )?;
    assert_refused_untouched(&fixture).await?;

    // Origin cannot be fetched: refuse before any mutation.
    let fixture = ReleaseFixture::new()?;
    let missing = fixture.work.join("missing.git");
    fixture.git(
        &fixture.work,
        &["remote", "set-url", "origin", &missing.to_string_lossy()],
    )?;
    assert_refused_untouched(&fixture).await
}

#[tokio::test]
async fn bump_failure_existing_tag() -> Result<()> {
    // The target tag exists on the remote only.
    let fixture = ReleaseFixture::new()?;
    fixture.git(&fixture.other, &["tag", "1.2.4"])?;
    fixture.git(&fixture.other, &["push", "--quiet", "origin", "1.2.4"])?;
    assert_refused_untouched(&fixture).await?;

    // The target tag exists locally only.
    let fixture = ReleaseFixture::new()?;
    fixture.git(&fixture.work, &["tag", "1.2.4"])?;
    assert_refused_untouched(&fixture).await
}

#[tokio::test]
async fn bump_failure_invalid_version() -> Result<()> {
    let fixture = ReleaseFixture::new()?;
    let manifest = fixture.read("Cargo.toml")?.replace("1.2.3", "1.2.3-rc.1");
    fs::write(fixture.work.join("Cargo.toml"), manifest)?;
    fixture.git(
        &fixture.work,
        &["commit", "--quiet", "-am", "Use a prerelease"],
    )?;
    fixture.git(&fixture.work, &["push", "--quiet", "origin", "develop"])?;
    assert_refused_untouched(&fixture).await
}

#[tokio::test]
async fn bump_failure_tool_results() -> Result<()> {
    for control in ["wrong-bump", "fail-update", "fail-clippy", "fail-test"] {
        let fixture = ReleaseFixture::new()?;
        fixture.inject(control)?;
        let (status, _) = assert_refused_restored(&fixture).await?;
        assert_eq!(status, "", "{control} left the worktree dirty");
    }
    Ok(())
}

#[tokio::test]
async fn bump_failure_verification_dirt() -> Result<()> {
    let fixture = ReleaseFixture::new()?;
    fixture.inject("verify-modify-unrelated")?;
    let (status, log) = assert_refused_restored(&fixture).await?;
    assert_eq!(status, "M README.md");
    assert!(log.contains("README.md"), "unexpected edit not reported");

    let fixture = ReleaseFixture::new()?;
    fixture.inject("verify-stage-unrelated")?;
    let (status, log) = assert_refused_restored(&fixture).await?;
    assert_eq!(status, "A  staged.txt");
    assert!(log.contains("staged.txt"), "unexpected edit not reported");
    Ok(())
}

#[tokio::test]
async fn bump_failure_signing() -> Result<()> {
    let fixture = ReleaseFixture::new()?;
    let missing = fixture.work.join("no-such-key");
    fixture.git(
        &fixture.work,
        &[
            "config",
            "--global",
            "user.signingkey",
            &missing.to_string_lossy(),
        ],
    )?;
    assert_refused_untouched(&fixture).await
}

#[tokio::test]
async fn bump_failure_rejected_push() -> Result<()> {
    let fixture = ReleaseFixture::new()?;
    fixture.reject_pushes()?;
    let base = fixture.git(&fixture.work, &["rev-parse", "HEAD"])?;
    let origin = fixture.origin_refs()?;

    let log = refused(&fixture).await?;

    let work = &fixture.work;
    assert_eq!(fixture.origin_refs()?, origin);
    assert_eq!(
        fixture.git(work, &["rev-list", "--count", &format!("{base}..HEAD")])?,
        "1"
    );
    assert_eq!(fixture.git(work, &["log", "-1", "--format=%G?"])?, "G");
    assert_eq!(
        fixture.git(
            work,
            &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"]
        )?,
        "Cargo.lock\nCargo.toml"
    );
    assert_eq!(fixture.git(work, &["status", "--porcelain"])?, "");
    assert_eq!(fixture.calls("cargo")?.len(), 5);
    assert!(log.contains(RETRY_PUSH), "retry command not printed");
    Ok(())
}

async fn release_success_case() -> Result<()> {
    let fixture = ReleaseFixture::new()?;
    configure_release_gh(&fixture, "success")?;
    let selected = fixture.git(&fixture.work, &["rev-parse", "origin/develop"])?;
    let output = fixture.run("just", &["release"], &[]).await?;
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(
        fixture.git(&fixture.origin, &["rev-parse", "refs/heads/main"])?,
        selected
    );
    assert_eq!(
        fixture.git(&fixture.origin, &["rev-parse", "refs/heads/develop"])?,
        selected
    );
    assert_eq!(fixture.git(&fixture.work, &["tag", "--list"])?, "1.2.2");
    assert!(
        fixture
            .calls("gh")?
            .iter()
            .any(|call| call.contains("workflow run release.yml"))
    );
    Ok(())
}

async fn release_failure_case() -> Result<()> {
    let fixture = ReleaseFixture::new()?;
    configure_release_gh(&fixture, "failed-watch")?;
    let selected = fixture.git(&fixture.work, &["rev-parse", "origin/develop"])?;
    let output = fixture.run("just", &["release"], &[]).await?;
    assert!(!output.status.success());
    assert_eq!(
        fixture.git(&fixture.origin, &["rev-parse", "refs/heads/main"])?,
        selected
    );
    assert!(text(&output).contains("git push origin"));
    Ok(())
}

#[tokio::test]
async fn release_selects_remote_develop_when_local_head_differs() -> Result<()> {
    let fixture = ReleaseFixture::new()?;
    fixture.git(
        &fixture.work,
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "Local-only work",
        ],
    )?;
    configure_release_gh(&fixture, "success")?;
    let selected = fixture.git(&fixture.work, &["rev-parse", "origin/develop"])?;

    let output = fixture.run("just", &["release"], &[]).await?;

    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(
        fixture.git(&fixture.origin, &["rev-parse", "refs/heads/main"])?,
        selected
    );
    assert!(
        fixture
            .calls("gh")?
            .iter()
            .any(|call| call.contains(&format!("expected_sha={selected}")))
    );
    Ok(())
}

#[tokio::test]
async fn release_rejects_publication_without_mutating_main() -> Result<()> {
    let fixture = ReleaseFixture::new()?;
    fixture.git(
        &fixture.other,
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "Remote release work",
        ],
    )?;
    fixture.git(&fixture.other, &["push", "--quiet", "origin", "develop"])?;
    configure_release_gh(&fixture, "success")?;
    fixture.reject_pushes()?;
    let main_before = fixture.git(&fixture.origin, &["rev-parse", "refs/heads/main"])?;

    let output = fixture.run("just", &["release"], &[]).await?;

    assert!(!output.status.success());
    assert_eq!(
        fixture.git(&fixture.origin, &["rev-parse", "refs/heads/main"])?,
        main_before
    );
    assert!(text(&output).contains("publication progress is preserved"));
    assert!(text(&output).contains("refs/heads/main"));
    Ok(())
}

#[tokio::test]
async fn release_preserves_dirty_local_files_on_api_failure() -> Result<()> {
    let fixture = ReleaseFixture::new()?;
    configure_release_gh(&fixture, "api-error")?;
    fs::write(fixture.work.join("notes.txt"), "keep me\n")?;

    let output = fixture.run("just", &["release"], &[]).await?;

    assert!(!output.status.success());
    assert_eq!(fixture.read("notes.txt")?, "keep me\n");
    assert_eq!(
        fixture.git(
            &fixture.work,
            &["status", "--porcelain", "--untracked-files=all"]
        )?,
        "?? notes.txt"
    );
    Ok(())
}

#[tokio::test]
async fn release_watches_only_the_matching_competing_dispatch() -> Result<()> {
    let fixture = ReleaseFixture::new()?;
    configure_release_gh(&fixture, "competing")?;

    let output = fixture.run("just", &["release"], &[]).await?;

    assert!(output.status.success(), "{}", text(&output));
    let watches = fixture
        .calls("gh")?
        .into_iter()
        .filter(|call| call.starts_with("run watch"))
        .collect::<Vec<_>>();
    assert_eq!(
        watches,
        ["run watch 7 --exit-status", "run watch 8 --exit-status"]
    );
    Ok(())
}

#[tokio::test]
async fn release_rejects_wrong_identity_and_missing_runs() -> Result<()> {
    for mode in ["wrong-identity", "missing-run"] {
        let fixture = ReleaseFixture::new()?;
        configure_release_gh(&fixture, mode)?;
        let before = fixture.origin_refs()?;

        let output = fixture.run("just", &["release"], &[]).await?;

        assert!(!output.status.success(), "{mode} unexpectedly succeeded");
        assert_eq!(fixture.origin_refs()?, before);
        assert!(
            fixture
                .calls("gh")?
                .iter()
                .all(|call| !call.starts_with("run watch")),
            "{mode} watched an untrusted run"
        );
    }
    Ok(())
}

#[tokio::test]
async fn release_hanging_watch_cleans_up_process_group() -> Result<()> {
    let fixture = ReleaseFixture::new()?;
    configure_release_gh(&fixture, "hanging-watch")?;
    let error = fixture
        .run(
            "just",
            &["release"],
            &[("GRUEZI_RELEASE_TEST_TIMEOUT_SECONDS", "1")],
        )
        .await
        .err()
        .ok_or_else(|| anyhow!("hanging release unexpectedly completed"))?;

    let message = error.to_string();
    assert!(message.contains("process group killed"));
    assert!(message.contains("run watch 7"));
    Ok(())
}

#[tokio::test]
async fn release_recovery_reuses_the_same_run_without_dispatch() -> Result<()> {
    let fixture = ReleaseFixture::new()?;
    configure_release_gh(&fixture, "recover-after-first")?;
    let first = fixture.run("just", &["release"], &[]).await?;
    assert!(!first.status.success());
    let dispatches_before = fixture
        .calls("gh")?
        .iter()
        .filter(|call| call.starts_with("workflow run"))
        .count();

    let recovery = fixture
        .run("gh", &["run", "watch", "7", "--exit-status"], &[])
        .await?;
    let repeated = fixture
        .run("gh", &["run", "watch", "7", "--exit-status"], &[])
        .await?;

    assert!(recovery.status.success());
    assert!(repeated.status.success());
    assert_eq!(
        fixture
            .calls("gh")?
            .iter()
            .filter(|call| call.starts_with("workflow run"))
            .count(),
        dispatches_before
    );
    assert_eq!(fixture.git(&fixture.work, &["tag", "--list"])?, "1.2.2");
    Ok(())
}

#[tokio::test]
async fn release_stops_when_develop_moves_between_selection_and_dispatch() -> Result<()> {
    let fixture = ReleaseFixture::new()?;
    fixture.git(
        &fixture.other,
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "Future develop work",
        ],
    )?;
    fixture.git(
        &fixture.other,
        &["push", "--quiet", "origin", "HEAD:refs/heads/moving"],
    )?;
    configure_release_gh(&fixture, "moved")?;
    let main_before = fixture.git(&fixture.origin, &["rev-parse", "refs/heads/main"])?;

    let output = fixture.run("just", &["release"], &[]).await?;

    assert!(!output.status.success());
    assert_eq!(
        fixture.git(&fixture.origin, &["rev-parse", "refs/heads/main"])?,
        main_before
    );
    assert!(
        fixture
            .calls("gh")?
            .iter()
            .all(|call| !call.starts_with("run watch"))
    );
    Ok(())
}

#[tokio::test]
async fn release_rejects_main_that_is_ahead_of_selected_develop() -> Result<()> {
    let fixture = ReleaseFixture::new()?;
    fixture.git(
        &fixture.other,
        &["commit", "--quiet", "--allow-empty", "-m", "Main-only work"],
    )?;
    fixture.git(
        &fixture.other,
        &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
    )?;
    configure_release_gh(&fixture, "success")?;
    let main_before = fixture.git(&fixture.origin, &["rev-parse", "refs/heads/main"])?;

    let output = fixture.run("just", &["release"], &[]).await?;

    assert!(!output.status.success());
    assert!(text(&output).contains("main is not an ancestor"));
    assert_eq!(
        fixture.git(&fixture.origin, &["rev-parse", "refs/heads/main"])?,
        main_before
    );
    Ok(())
}

#[tokio::test]
async fn release_publication_is_not_force_push() -> Result<()> {
    let fixture = ReleaseFixture::new()?;
    configure_release_gh(&fixture, "success")?;
    write_executable(
        &fixture.bin.join("git"),
        r#"#!/bin/bash
for arg in "$@"; do
  [[ "$arg" != "--force" && "$arg" != "-f" ]] || {
    echo "unsafe force push detected" >&2
    exit 99
  }
done
exec /usr/bin/git "$@"
"#,
    )?;

    let output = fixture.run("just", &["release"], &[]).await?;

    assert!(output.status.success(), "{}", text(&output));
    Ok(())
}

#[tokio::test]
async fn release_success() -> Result<()> {
    release_success_case().await
}

#[tokio::test]
async fn release_failure() -> Result<()> {
    release_failure_case().await
}

#[tokio::test]
async fn documented_flow() -> Result<()> {
    release_success_case().await
}

#[tokio::test]
async fn documented_recovery() -> Result<()> {
    release_failure_case().await
}
