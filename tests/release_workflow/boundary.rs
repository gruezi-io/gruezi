//! The workflow's command boundary: local composite actions run for real, and
//! the compiler and packagers stand in for what they would produce.

use super::model::{Scalar, Step};
use super::runner::Runner;
use super::world;
use crate::release_actions::{ActionRun, Step as ShellStep};
use anyhow::{Result, anyhow};
use serde::Deserialize;
use std::{collections::BTreeMap, fs};

/// A composite action's own steps and declared outputs.
#[derive(Deserialize)]
struct ActionMeta {
    #[serde(default)]
    runs: ActionRuns,
    #[serde(default)]
    outputs: BTreeMap<String, ActionOutput>,
}

#[derive(Deserialize)]
struct ActionOutput {
    value: String,
}

#[derive(Deserialize, Default)]
struct ActionRuns {
    #[serde(default)]
    steps: Vec<ShellStep>,
}

impl Runner<'_> {
    pub(super) async fn uses(
        &self,
        run: &mut ActionRun<'_>,
        step: &Step,
        uses: &str,
    ) -> Result<()> {
        self.world.record(&format!("uses {uses}"))?;
        let target = match step.with.get("target") {
            Some(value) => run.resolve(&value.text())?,
            None => String::new(),
        };
        for (action, failing) in &self.failed_uses {
            if uses.ends_with(&format!("/{action}")) && (failing.is_empty() || *failing == target) {
                return Err(anyhow!("injected failure of {uses}"));
            }
        }
        if let Some(name) = uses.strip_prefix("./.github/actions/") {
            return match name {
                // The compiler and the packagers are the build boundary: what they
                // would produce is staged, never built here.
                "build-binary" => self.stage_build(run, step),
                "linux-packages" => self.stage_packages(run, step),
                _ => self.composite(run, step, name).await,
            };
        }
        if uses.starts_with("actions/checkout@") {
            if step
                .with
                .get("persist-credentials")
                .map(Scalar::text)
                .as_deref()
                != Some("false")
            {
                return Err(anyhow!("checkout keeps credentials"));
            }
            let reference = input(step, "ref")?;
            if run.resolve(&reference)? != self.sha {
                return Err(anyhow!("checkout is not of the dispatched commit"));
            }
            self.world.record("checkout")
        } else if uses.starts_with("actions/upload-artifact@") {
            let name = run.resolve(&input(step, "name")?)?;
            let path = run.resolve(&input(step, "path")?)?;
            if step.with.get("retention-days").map(Scalar::text).as_deref() != Some("90") {
                return Err(anyhow!("upload {name} does not keep artifacts for 90 days"));
            }
            self.upload(&name, &path)
        } else if uses.starts_with("actions/download-artifact@") {
            let path = run.resolve(&input(step, "path")?)?;
            let merge =
                step.with.get("merge-multiple").map(Scalar::text).as_deref() == Some("true");
            match step.with.get("pattern") {
                Some(pattern) => {
                    self.download_pattern(&run.resolve(&pattern.text())?, &path, merge)
                }
                None => self.download(&run.resolve(&input(step, "name")?)?, &path),
            }
        } else if uses.starts_with("actions/attest-build-provenance@") {
            let subject = run.resolve(&input(step, "subject-path")?)?;
            let files: usize = subject
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(|line| self.expand(line).map(|files| files.len()))
                .sum::<Result<usize>>()?;
            if files == 0 {
                return Err(anyhow!("nothing to attest"));
            }
            self.world.record(&format!("attest {files} files"))
        } else if uses.starts_with("rust-lang/crates-io-auth-action@") {
            let id = step.id.clone().unwrap_or_default();
            run.provide(&id, "token", "fixture-token");
            self.world.record("auth")
        } else {
            Err(anyhow!("unscripted uses {uses}"))
        }
    }

    async fn composite(&self, run: &mut ActionRun<'_>, step: &Step, name: &str) -> Result<()> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".github/actions")
            .join(name)
            .join("action.yml");
        let meta: ActionMeta = serde_saphyr::from_str(&fs::read_to_string(&path)?)?;
        let inputs: Vec<(String, String)> = step
            .with
            .iter()
            .map(|(key, value)| Ok((key.clone(), run.resolve(&value.text())?)))
            .collect::<Result<_>>()?;
        let borrowed: Vec<(&str, &str)> = inputs
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        let mut action = ActionRun::load(&self.world.fixture, name, &borrowed)?;
        for (key, value) in run.exported_env() {
            action.expression(&format!("env.{key}"), &value);
        }
        for shell in &meta.runs.steps {
            let (Some(id), Some(_)) = (&shell.id, &shell.run) else {
                continue;
            };
            self.world.record(&format!("composite {name} {id}"))?;
            let output = action.step(id).await?;
            world::log(&format!("{name} {id}"), &output);
            if !output.status.success() {
                return Err(anyhow!("composite {name} step {id} failed"));
            }
        }
        if let Some(id) = &step.id {
            for (key, output) in &meta.outputs {
                let value = declared_output(&action, name, key, &output.value)?;
                run.provide(id, key, &value);
            }
        }
        Ok(())
    }

    /// `build-binary` stands in for the compiler: the binary it would build.
    fn stage_build(&self, run: &mut ActionRun<'_>, step: &Step) -> Result<()> {
        let target = run.resolve(&input(step, "target")?)?;
        let (name, version) = package(run)?;
        let dir = self
            .world
            .fixture
            .work
            .join(format!("target/{target}/release"));
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join(&name),
            format!("fixture binary {target} {version}\n"),
        )?;
        self.world.record(&format!("build {target}"))
    }

    /// `linux-packages` stands in for the packagers: the one DEB and one RPM of
    /// this target, with the names and architectures the action checks.
    fn stage_packages(&self, run: &mut ActionRun<'_>, step: &Step) -> Result<()> {
        let target = run.resolve(&input(step, "target")?)?;
        let (name, version) = package(run)?;
        let (deb, rpm) = match target.as_str() {
            "x86_64-unknown-linux-musl" => ("amd64", "x86_64"),
            "aarch64-unknown-linux-musl" => ("arm64", "aarch64"),
            _ => return Err(anyhow!("no package names for {target}")),
        };
        let out = self.world.fixture.work.join("dist");
        fs::create_dir_all(&out)?;
        fs::write(
            out.join(format!("{name}_{version}-1_{deb}.deb")),
            format!("deb {target}\n"),
        )?;
        fs::write(
            out.join(format!("{name}-{version}-1.{rpm}.rpm")),
            format!("rpm {target}\n"),
        )?;
        self.world.record(&format!("packages {target}"))
    }
}

/// The package the guard established for this release.
fn package(run: &ActionRun<'_>) -> Result<(String, String)> {
    Ok((
        run.resolve("${{ needs.guard.outputs.name }}")?,
        run.resolve("${{ needs.guard.outputs.version }}")?,
    ))
}

/// One `with:` input of a step, as written.
fn input(step: &Step, name: &str) -> Result<String> {
    step.with
        .get(name)
        .map(Scalar::text)
        .ok_or_else(|| anyhow!("step has no input {name}"))
}

/// A composite action's declared output is `${{ steps.<id>.outputs.<name> }}`.
fn declared_output(
    action: &ActionRun<'_>,
    action_name: &str,
    key: &str,
    template: &str,
) -> Result<String> {
    let (step, name) = template
        .strip_prefix("${{")
        .and_then(|rest| rest.strip_suffix("}}"))
        .map(str::trim)
        .and_then(|value| value.strip_prefix("steps."))
        .and_then(|rest| rest.split_once(".outputs."))
        .ok_or_else(|| {
            anyhow!("{action_name} declares output {key} from an unexpected expression")
        })?;
    Ok(action.output(step, name)?.to_owned())
}
