//! Runs the real `run:` blocks of a local composite action.
//!
//! The action file is parsed and one step is selected by its stable `id`; its
//! shell is executed through the fixture exactly as a runner would (`bash -e -o
//! pipefail`) inside the fixture checkout. Expressions resolve by exact lookup of
//! what the caller provided (`inputs.<name>`, `steps.<id>.outputs.<name>`, or any
//! other key a caller supplies with `expression`); a value stored under
//! `env.<NAME>` is exported to every step as `NAME`, like the runner's own
//! variables. Everything else, every decision, stays in the action's own shell.

use crate::release_support::ReleaseFixture;
use anyhow::{Result, anyhow};
use serde::Deserialize;
use std::{collections::BTreeMap, fs, path::Path, process::Output};

#[derive(Deserialize)]
struct Action {
    #[serde(default)]
    inputs: BTreeMap<String, Input>,
    runs: Runs,
}

#[derive(Deserialize)]
struct Input {
    default: Option<String>,
}

#[derive(Deserialize)]
struct Runs {
    steps: Vec<Step>,
}

/// One step of an action or workflow job: only what a shell step needs.
#[derive(Deserialize)]
pub struct Step {
    pub id: Option<String>,
    pub run: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

pub struct ActionRun<'a> {
    fixture: &'a ReleaseFixture,
    steps: Vec<Step>,
    /// Resolvable expressions, such as `inputs.x` and `steps.id.outputs.x`.
    values: BTreeMap<String, String>,
}

impl<'a> ActionRun<'a> {
    /// Load `.github/actions/<name>/action.yml` from the repository under test.
    /// Inputs take the action's declared defaults unless given.
    pub fn load(fixture: &'a ReleaseFixture, name: &str, inputs: &[(&str, &str)]) -> Result<Self> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".github/actions")
            .join(name)
            .join("action.yml");
        let action: Action = serde_saphyr::from_str(&fs::read_to_string(&path)?)?;
        let mut run = Self::from_steps(fixture, action.runs.steps);
        for (key, input) in &action.inputs {
            if let Some(default) = &input.default {
                run.expression(&format!("inputs.{key}"), default);
            }
        }
        for (key, value) in inputs {
            if !action.inputs.contains_key(*key) {
                return Err(anyhow!("{name} declares no input {key}"));
            }
            run.expression(&format!("inputs.{key}"), value);
        }
        Ok(run)
    }

    /// Run the steps of another source, such as a workflow job.
    pub fn from_steps(fixture: &'a ReleaseFixture, steps: Vec<Step>) -> Self {
        Self {
            fixture,
            steps,
            values: BTreeMap::new(),
        }
    }

    /// Define what an expression resolves to.
    pub fn expression(&mut self, key: &str, value: &str) {
        self.values.insert(key.to_owned(), value.to_owned());
    }

    /// Supply an output of a step this harness does not run, such as a `uses:` step.
    pub fn provide(&mut self, step: &str, name: &str, value: &str) {
        self.expression(&format!("steps.{step}.outputs.{name}"), value);
    }

    /// The runner's variables as `(NAME, value)` pairs, to export to another run.
    pub fn exported_env(&self) -> Vec<(String, String)> {
        self.values
            .iter()
            .filter_map(|(key, value)| Some((key.strip_prefix("env.")?.to_owned(), value.clone())))
            .collect()
    }

    /// An output of an earlier step.
    pub fn output(&self, step: &str, name: &str) -> Result<&str> {
        self.values
            .get(&format!("steps.{step}.outputs.{name}"))
            .map(String::as_str)
            .ok_or_else(|| anyhow!("step {step} produced no output {name}"))
    }

    /// Run the shell of step `id`, record what it wrote to `$GITHUB_OUTPUT`, and
    /// return the process result (a failing step is an `Ok` result with a failed
    /// status, so tests assert on it).
    pub async fn step(&mut self, id: &str) -> Result<Output> {
        let step = self
            .steps
            .iter()
            .find(|step| step.id.as_deref() == Some(id))
            .ok_or_else(|| anyhow!("no step with id {id}"))?;
        let script = step
            .run
            .clone()
            .ok_or_else(|| anyhow!("step {id} has no run block"))?;
        let mut env = self.exported_env();
        for (key, template) in &step.env {
            env.push((key.clone(), self.resolve(template)?));
        }
        let github_output = self.fixture.work.with_file_name(format!("output-{id}"));
        fs::write(&github_output, "")?;
        env.push((
            "GITHUB_OUTPUT".to_owned(),
            github_output.to_string_lossy().into_owned(),
        ));
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let output = self
            .fixture
            .run(
                "bash",
                &[
                    "--noprofile",
                    "--norc",
                    "-e",
                    "-o",
                    "pipefail",
                    "-c",
                    &script,
                ],
                &env,
            )
            .await?;
        for line in fs::read_to_string(&github_output)?.lines() {
            if let Some((name, value)) = line.split_once('=') {
                self.provide(id, name, value);
            }
        }
        Ok(output)
    }

    /// Expand every `${{ expression }}` in `template`.
    pub fn resolve(&self, template: &str) -> Result<String> {
        let mut resolved = String::new();
        let mut rest = template;
        while let Some((head, after)) = rest.split_once("${{") {
            let (expression, tail) = after
                .split_once("}}")
                .ok_or_else(|| anyhow!("unterminated expression in {template}"))?;
            let value = self
                .values
                .get(expression.trim())
                .ok_or_else(|| anyhow!("unresolved expression {}", expression.trim()))?;
            resolved.push_str(head);
            resolved.push_str(value);
            rest = tail;
        }
        resolved.push_str(rest);
        Ok(resolved)
    }
}
