//! Runs the real steps of the release workflow's jobs against the scripted world.
//!
//! The workflow file is the source of truth: jobs run in the order their `needs`
//! graph and `steps` list describe, `if:` conditions are evaluated from step
//! outputs, and matrix rows run like the runner's would. Local composite actions
//! execute their own `run:` blocks; third-party actions are the boundary and are
//! recorded (artifact uploads and downloads move the real files).

use super::model::{Job, Step, Workflow, workflow};
use super::world::{self, REPO, World};
use crate::release_actions::{ActionRun, Step as ShellStep};
use anyhow::{Result, anyhow};
use std::{collections::BTreeMap, fs};

/// What happened to one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Skipped,
    Failed(String),
}

impl Outcome {
    pub fn success(&self) -> bool {
        *self == Self::Success
    }

    pub fn failed(&self, step: &str) -> bool {
        matches!(self, Self::Failed(name) if name == step)
    }
}

/// One workflow run in the scripted world.
pub struct Runner<'a> {
    pub world: &'a World,
    pub workflow: Workflow,
    pub sha: String,
    pub expected_sha: String,
    pub ref_name: String,
    pub dispatch_id: String,
    pub stop_after: Option<(String, String)>,
    pub skip_jobs: Vec<String>,
    /// `(action name, target)` pairs whose `uses:` step must fail (empty target
    /// matches every target).
    pub failed_uses: Vec<(String, String)>,
    attempt: u64,
    outputs: BTreeMap<String, BTreeMap<String, String>>,
}

impl<'a> Runner<'a> {
    pub fn new(world: &'a World) -> Result<Self> {
        let sha = world.head()?;
        Ok(Self {
            world,
            workflow: workflow()?,
            expected_sha: sha.clone(),
            sha,
            ref_name: "refs/heads/develop".to_owned(),
            dispatch_id: "dispatch-test".to_owned(),
            stop_after: None,
            skip_jobs: Vec::new(),
            failed_uses: Vec::new(),
            attempt: 0,
            outputs: BTreeMap::new(),
        })
    }

    /// A declared output of a job that ran.
    pub fn output(&self, job: &str, name: &str) -> Option<&str> {
        self.outputs
            .get(job)
            .and_then(|outputs| outputs.get(name))
            .map(String::as_str)
    }

    /// Run one job as the runner would: after the jobs it needs, once per matrix
    /// row, steps in order and only those whose `if:` holds.
    pub async fn run(&mut self, job_name: &str) -> Result<Outcome> {
        let job = self
            .workflow
            .jobs
            .get(job_name)
            .ok_or_else(|| anyhow!("the workflow has no job {job_name}"))?
            .clone();
        for need in &job.needs {
            if !self.outputs.contains_key(need.as_str()) {
                return Ok(Outcome::Skipped);
            }
        }
        if self.skip_jobs.iter().any(|skip| skip == job_name) {
            return Ok(Outcome::Skipped);
        }
        let mut rows = match job
            .strategy
            .as_ref()
            .and_then(|strategy| strategy.matrix.as_ref())
        {
            Some(matrix) => matrix.include.clone(),
            None => Vec::new(),
        };
        if rows.is_empty() {
            rows.push(BTreeMap::new());
        }
        for row in rows {
            if let Outcome::Failed(step) = self.execute(job_name, &job, &row).await? {
                // A job that failed part way through produced no outputs.
                self.outputs.remove(job_name);
                return Ok(Outcome::Failed(step));
            }
        }
        Ok(Outcome::Success)
    }

    async fn execute(
        &mut self,
        name: &str,
        job: &Job,
        row: &BTreeMap<String, String>,
    ) -> Result<Outcome> {
        self.attempt += 1;
        let work = self.world.fixture.work.clone();
        self.world.fixture.git(&work, &["clean", "-fdxq"])?;
        self.world
            .fixture
            .git(&work, &["checkout", "--quiet", "--detach", &self.sha])?;
        let temp = self
            .world
            .state
            .join(format!("runner-temp/{name}-{}", self.attempt));
        fs::create_dir_all(&temp)?;
        let temp = temp.to_string_lossy().into_owned();
        let steps = job
            .steps
            .iter()
            .map(|step| ShellStep {
                id: step.id.clone(),
                run: step.run.clone(),
                env: step.env.clone(),
            })
            .collect();
        let mut run = ActionRun::from_steps(&self.world.fixture, steps);
        for (key, value) in self.context(job, row, &temp) {
            run.expression(&key, &value);
        }
        for (key, template) in &job.env {
            let value = run.resolve(template)?;
            run.expression(&format!("env.{key}"), &value);
        }
        for step in &job.steps {
            if !holds(&run, step)? {
                continue;
            }
            let label = step
                .id
                .clone()
                .or_else(|| step.name.clone())
                .unwrap_or_default();
            let outcome = match (&step.uses, &step.run) {
                (Some(uses), None) => self.uses(&mut run, step, uses).await.map(|()| true),
                (None, Some(_)) => {
                    let id = step
                        .id
                        .clone()
                        .ok_or_else(|| anyhow!("step '{label}' has no id"))?;
                    let output = run.step(&id).await?;
                    world::log(&label, &output);
                    Ok(output.status.success())
                }
                _ => return Err(anyhow!("step '{label}' must have exactly one of uses/run")),
            };
            match outcome {
                Ok(true) => {}
                Ok(false) => return Ok(Outcome::Failed(label)),
                Err(error) => {
                    println!("== {label}: {error}");
                    return Ok(Outcome::Failed(label));
                }
            }
            if self
                .stop_after
                .as_ref()
                .is_some_and(|(job, step)| job == name && step == &label)
            {
                break;
            }
        }
        let mut outputs = BTreeMap::new();
        for (key, template) in &job.outputs {
            outputs.insert(key.clone(), run.resolve(template)?);
        }
        self.outputs.insert(name.to_owned(), outputs);
        Ok(Outcome::Success)
    }

    /// The expressions and runner variables one job attempt runs with.
    fn context(
        &self,
        job: &Job,
        row: &BTreeMap<String, String>,
        temp: &str,
    ) -> BTreeMap<String, String> {
        let mut values = BTreeMap::new();
        for (key, value) in [
            ("github.sha", self.sha.clone()),
            ("github.repository", REPO.to_owned()),
            ("github.token", "github-token".to_owned()),
            ("github.run_id", "4242".to_owned()),
            ("runner.temp", temp.to_owned()),
            ("inputs.expected_sha", self.expected_sha.clone()),
            ("inputs.dispatch_id", self.dispatch_id.clone()),
        ] {
            values.insert(key.to_owned(), value);
        }
        for job_name in &job.needs {
            if let Some(outputs) = self.outputs.get(job_name) {
                for (key, value) in outputs {
                    values.insert(format!("needs.{job_name}.outputs.{key}"), value.clone());
                }
            }
        }
        for (key, value) in row {
            values.insert(format!("matrix.{key}"), value.clone());
        }
        for (key, value) in [
            ("GITHUB_REPOSITORY", REPO),
            ("GITHUB_SHA", self.sha.as_str()),
            ("GITHUB_REF", self.ref_name.as_str()),
            (
                "GITHUB_REF_NAME",
                self.ref_name.rsplit('/').next().unwrap_or_default(),
            ),
            ("GITHUB_EVENT_NAME", "workflow_dispatch"),
            ("GITHUB_TOKEN", "github-token"),
            ("GH_TOKEN", "github-token"),
            ("RUNNER_TEMP", temp),
        ] {
            values.insert(format!("env.{key}"), value.to_owned());
        }
        values
    }
}

/// Whether a step's `if:` holds; a missing condition always runs the step.
fn holds(run: &ActionRun<'_>, step: &Step) -> Result<bool> {
    let Some(condition) = &step.condition else {
        return Ok(true);
    };
    for (operator, equal) in [("==", true), ("!=", false)] {
        if let Some((left, right)) = condition.split_once(operator) {
            let left = run.resolve(&format!("${{{{ {}}}}}", left.trim()))?;
            let right = right.trim().trim_matches(['\'', '"']);
            return Ok((left == right) == equal);
        }
    }
    let value = run.resolve(&format!("${{{{ {}}}}}", condition.trim()))?;
    Ok(!value.is_empty())
}
