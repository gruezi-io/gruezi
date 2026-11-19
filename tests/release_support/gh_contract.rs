//! Independent command contracts captured from the real gh CLI.

use super::{ReleaseFixture, text};
use anyhow::Result;
use std::fs;

#[tokio::test]
async fn release_uses_selected_repository_when_ambient_repository_differs() -> Result<()> {
    let fixture = ReleaseFixture::new()?;
    crate::configure_release_gh(&fixture, "success")?;
    fs::write(fixture.bin.join("ambient-repo"), "fixture/ambient")?;

    let output = fixture.run("just", &["release"], &[]).await?;

    assert!(output.status.success(), "{}", text(&output));
    Ok(())
}

#[tokio::test]
async fn rejects_unknown_json_fields_for_each_subcommand() -> Result<()> {
    let fixture = ReleaseFixture::new()?;
    for (command, field, available) in [
        ("list", "repository", "databaseId"),
        ("list", "jobs", "workflowName"),
        ("view", "repository", "jobs"),
    ] {
        let output = fixture
            .run(
                "gh",
                &["run", command, "-R", "fixture/repo", "--json", field],
                &[],
            )
            .await?;
        assert!(!output.status.success(), "{command} accepted {field}");
        let error = String::from_utf8(output.stderr)?;
        assert!(error.contains(&format!("Unknown JSON field: \"{field}\"")));
        assert!(error.contains("Available fields:\n  attempt\n"));
        assert!(error.lines().any(|line| line == format!("  {available}")));
    }
    Ok(())
}

#[tokio::test]
async fn accepts_real_json_fields_for_each_subcommand() -> Result<()> {
    let fixture = ReleaseFixture::new()?;
    for args in [
        vec![
            "run",
            "list",
            "-R",
            "fixture/repo",
            "--json",
            "attempt,conclusion,createdAt,databaseId,displayTitle,event,headBranch,headSha,name,number,startedAt,status,updatedAt,url,workflowDatabaseId,workflowName",
        ],
        vec!["run", "view", "-R", "fixture/repo", "--json=jobs,headSha"],
        vec!["repo", "view", "--json", "nameWithOwner"],
    ] {
        let output = fixture.run("gh", &args, &[]).await?;
        assert!(output.status.success(), "{}", text(&output));
    }
    Ok(())
}

#[tokio::test]
async fn resolves_runs_and_dispatches_only_in_the_selected_repository() -> Result<()> {
    let fixture = ReleaseFixture::new()?;
    fs::write(fixture.bin.join("ambient-repo"), "fixture/ambient")?;
    for command in [["run", "list"], ["run", "view"], ["workflow", "run"]] {
        for repository in [None, Some("fixture/other"), Some("fixture/repo")] {
            let mut args = command.to_vec();
            if let Some(repository) = repository {
                args.extend(["-R", repository]);
            }
            let output = fixture.run("gh", &args, &[]).await?;
            assert_eq!(
                output.status.success(),
                repository == Some("fixture/repo"),
                "{args:?}: {}",
                text(&output)
            );
        }
    }
    Ok(())
}
