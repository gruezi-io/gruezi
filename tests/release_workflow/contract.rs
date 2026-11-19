//! Structural gates on the release workflow: its dispatch contract, the job
//! graph that keeps publication behind every build and package prerequisite,
//! its publisher identity, and the Bash its steps run.

use super::model::{Action, Job, Workflow, workflow};
use super::runner::Runner;
use super::world::World;
use anyhow::Result;
use std::{collections::BTreeMap, fs};

/// The build job's matrix rows, in the order the workflow declares them.
fn jobs_matrix(workflow: &Workflow) -> Vec<(String, String)> {
    let Some(matrix) = workflow
        .jobs
        .get("build")
        .and_then(|job| job.strategy.as_ref())
        .and_then(|strategy| strategy.matrix.as_ref())
    else {
        return Vec::new();
    };
    let field = |row: &BTreeMap<String, String>, key: &str| {
        row.get(key).map_or("", String::as_str).to_owned()
    };
    matrix
        .include
        .iter()
        .map(|row| (field(row, "os"), field(row, "target")))
        .collect()
}

fn job_named<'a>(jobs: &'a BTreeMap<String, Job>, name: &str) -> Result<&'a Job> {
    jobs.get(name)
        .ok_or_else(|| anyhow::anyhow!("the workflow has no job {name}"))
}

#[tokio::test]
async fn release_failure_workflow_contract() -> Result<()> {
    let workflow = workflow()?;
    assert_dispatch_contract(&workflow)?;
    assert_publication_graph(&workflow)?;
    assert_publisher_identity(&workflow)?;
    assert_shell_syntax(&workflow).await
}

/// Dispatch only, with both inputs required and a run name that names them.
fn assert_dispatch_contract(workflow: &Workflow) -> Result<()> {
    assert_eq!(workflow.name, "Release");
    assert_eq!(
        workflow.triggers.keys().cloned().collect::<Vec<_>>(),
        ["workflow_dispatch"]
    );
    let dispatch = workflow
        .triggers
        .get("workflow_dispatch")
        .ok_or_else(|| anyhow::anyhow!("no workflow_dispatch trigger"))?;
    for name in ["expected_sha", "dispatch_id"] {
        let input = dispatch
            .inputs
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("no {name} input"))?;
        assert!(input.required, "{name} is optional");
    }
    assert_eq!(dispatch.inputs.len(), 2);
    let file = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(super::world::WORKFLOW),
    )?;
    for marker in ["${{ inputs.expected_sha }}", "${{ inputs.dispatch_id }}"] {
        let line = file.lines().find(|line| line.starts_with("run-name:"));
        assert!(
            line.is_some_and(|line| line.contains(marker)),
            "run-name does not name {marker}"
        );
    }
    // Serialized releases, and no pending dispatch is cancelled.
    let concurrency = workflow
        .concurrency
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no concurrency"))?;
    assert_eq!(concurrency.group, "release");
    assert!(!concurrency.cancel_in_progress);
    assert_eq!(
        concurrency.queue.as_deref(),
        Some("max"),
        "a pending release is not queued"
    );
    Ok(())
}

/// The publishing jobs are only reachable through every build and package
/// prerequisite, and every job runs on a public Linux runner.
fn assert_publication_graph(workflow: &Workflow) -> Result<()> {
    let jobs = &workflow.jobs;
    for name in ["github", "publish"] {
        let publishing = job_named(jobs, name)?;
        for need in ["guard", "build", "crate"] {
            let reachable = publishing.needs.contains(&need.to_owned())
                || publishing.needs.iter().any(|via| {
                    job_named(jobs, via).is_ok_and(|via| via.needs.contains(&need.to_owned()))
                });
            assert!(reachable, "{name} is reachable before {need}");
        }
    }
    for name in ["build", "crate"] {
        assert!(job_named(jobs, name)?.needs.contains(&"guard".to_owned()));
    }
    for (name, job) in jobs {
        let on = job.runs_on.as_deref().unwrap_or_default();
        assert!(
            on == "ubuntu-latest" || on == "${{ matrix.os }}",
            "{name} runs on {on}"
        );
    }
    assert_eq!(
        jobs_matrix(workflow),
        [
            (
                "ubuntu-latest".to_owned(),
                "x86_64-unknown-linux-musl".to_owned(),
            ),
            (
                "ubuntu-24.04-arm".to_owned(),
                "aarch64-unknown-linux-musl".to_owned(),
            ),
        ]
    );
    Ok(())
}

/// Publisher identity: no environment binding, and the publish job mints only
/// its registry token.
fn assert_publisher_identity(workflow: &Workflow) -> Result<()> {
    let jobs = &workflow.jobs;
    for name in ["guard", "build", "crate", "github", "publish"] {
        assert!(
            job_named(jobs, name)?.environment.is_none(),
            "{name} is bound to an environment"
        );
    }
    let permissions = &job_named(jobs, "github")?.permissions;
    for (right, level) in [
        ("contents", "write"),
        ("id-token", "write"),
        ("attestations", "write"),
    ] {
        assert_eq!(
            permissions.get(right).map(String::as_str),
            Some(level),
            "github {right}"
        );
    }
    let permissions = &job_named(jobs, "publish")?.permissions;
    for (right, level) in [("contents", "read"), ("id-token", "write")] {
        assert_eq!(
            permissions.get(right).map(String::as_str),
            Some(level),
            "publish {right}"
        );
    }
    // The removed helper is gone and nothing runs it.
    assert!(
        !std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".github/actions/release-is-latest")
            .exists()
    );
    for job in jobs.values() {
        for step in &job.steps {
            if let Some(uses) = &step.uses {
                assert!(!uses.contains("release-is-latest"), "{uses} is still used");
            }
        }
    }
    Ok(())
}

/// Untrusted input never reaches a `run:` block, and every run block of the
/// workflow and the local actions is valid Bash.
async fn assert_shell_syntax(workflow: &Workflow) -> Result<()> {
    let mut scripts = Vec::new();
    for (name, job) in &workflow.jobs {
        for step in &job.steps {
            if let Some(run) = &step.run {
                assert!(
                    !run.contains("${{"),
                    "job {name} interpolates an expression into a run block"
                );
                scripts.push(run.clone());
            }
        }
    }
    for action in [
        "crate-info",
        "build-binary",
        "linux-packages",
        "release-version-gate",
    ] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".github/actions")
            .join(action)
            .join("action.yml");
        let parsed: Action = serde_saphyr::from_str(&fs::read_to_string(&path)?)?;
        for step in &parsed.runs.steps {
            if let Some(run) = &step.run {
                assert!(
                    !run.contains("${{"),
                    "{action} interpolates an expression into a run block"
                );
                scripts.push(run.clone());
            }
        }
    }
    let fixture = World::new()?;
    for script in &scripts {
        let output = fixture
            .fixture
            .run("bash", &["--noprofile", "--norc", "-n", "-c", script], &[])
            .await?;
        assert!(output.status.success(), "not valid Bash: {script}");
    }
    Ok(())
}

#[tokio::test]
async fn publish_rejects_transitive_crate_output() -> Result<()> {
    let world = World::new()?;
    let mut runner = Runner::new(&world)?;
    for job in ["guard", "build", "crate", "github"] {
        assert!(
            runner.run(job).await?.success(),
            "{job} did not prepare publication"
        );
    }
    assert!(
        runner
            .workflow
            .jobs
            .get("publish")
            .is_some_and(|job| job.needs.iter().any(|need| need == "crate")),
        "publish must directly depend on crate before testing unavailable output"
    );
    runner
        .workflow
        .jobs
        .get_mut("publish")
        .ok_or_else(|| anyhow::anyhow!("publish job is missing"))?
        .needs
        .retain(|need| need != "crate");

    let error = match runner.run("publish").await {
        Ok(outcome) => {
            return Err(anyhow::anyhow!(
                "publish accepted an output from a non-direct need: {outcome:?}"
            ));
        }
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("needs.crate.outputs.sha256"),
        "unexpected missing-output error: {error}"
    );
    Ok(())
}
