//! The world the release workflow runs against: a disposable repository fixture
//! plus a scripted GitHub and crates.io, all reachable only through the
//! command-boundary fakes (see `fakes`). Tests script this world, then assert on
//! the recorded calls and the state the workflow left behind.

use crate::release_support::{ReleaseFixture, text, write_executable};
use anyhow::Result;
use std::{fs, path::PathBuf};

pub const REPO: &str = "gruezi-io/gruezi";
pub const WORKFLOW: &str = ".github/workflows/release.yml";
pub const EXPECTED_SHA: &str = "aabbccddeeff00112233445566778899aabbccdd";

/// A scripted `build.yml` run.
#[derive(Clone)]
pub struct CiRun {
    pub id: u64,
    pub head_sha: String,
    pub event: String,
    pub branch: String,
    pub repo: String,
    pub workflow: String,
    pub status: String,
    pub repository_json: String,
}

impl CiRun {
    /// A completed push run of `build.yml` for `head_sha` on `develop` here.
    pub fn green(id: u64, head_sha: &str) -> Self {
        Self {
            id,
            head_sha: head_sha.to_owned(),
            event: "push".to_owned(),
            branch: "develop".to_owned(),
            repo: REPO.to_owned(),
            workflow: ".github/workflows/build.yml".to_owned(),
            status: "completed".to_owned(),
            repository_json: format!(r#"{{"full_name":"{REPO}"}}"#),
        }
    }

    fn json(&self) -> String {
        format!(
            r#"{{"id":{},"name":"Test & Build","head_sha":"{}","event":"{}","head_branch":"{}","repository":{},"path":"{}","status":"{}"}}"#,
            self.id,
            self.head_sha,
            self.event,
            self.branch,
            self.repository_json.replace(REPO, &self.repo),
            self.workflow,
            self.status
        )
    }
}

/// The scripted jobs endpoint of one run; `ci_ok` is the `CI OK` conclusion.
pub fn ci_jobs(run: &CiRun, ci_ok: &str) -> String {
    format!(
        r#"[{{"id":101,"name":"CI OK","status":"completed","conclusion":"{ci_ok}","head_sha":"{0}"}},{{"id":102,"name":"Test","status":"completed","conclusion":"success","head_sha":"{0}"}}]"#,
        run.head_sha
    )
}

pub fn ci_jobs_with_coverage_failure(run: &CiRun) -> String {
    format!(
        r#"[{{"id":101,"name":"CI OK","status":"completed","conclusion":"success","head_sha":"{0}"}},{{"id":102,"name":"Coverage","status":"completed","conclusion":"failure","head_sha":"{0}"}}]"#,
        run.head_sha
    )
}

/// An existing release, scripted into GitHub.
pub struct Release {
    pub id: u64,
    pub tag: String,
    pub target: String,
    pub draft: bool,
    pub latest: bool,
    pub assets: Vec<(String, Vec<u8>)>,
}

/// The fixture and the scripted service state.
pub struct World {
    pub fixture: ReleaseFixture,
    pub state: PathBuf,
}

impl World {
    /// A fixture whose package is `fixture` at `1.2.3`, with a green `CI OK` for
    /// that commit and no tags at all.
    pub fn new() -> Result<Self> {
        let fixture = ReleaseFixture::new()?;
        for (name, script) in [
            ("gh", super::fakes::FAKE_GH),
            ("curl", super::fakes::FAKE_CURL),
            ("cargo", super::fakes::FAKE_CARGO),
            ("rustup", super::fakes::FAKE_RUSTUP),
            ("rustc", super::fakes::FAKE_RUSTC),
            ("sleep", super::fakes::FAKE_SLEEP),
        ] {
            write_executable(&fixture.bin.join(name), script)?;
        }
        let state = fixture.bin.clone();
        for dir in [
            "gh-state/fail",
            "gh-state/tagrefs",
            "gh-state/routes",
            "gh-state/assets",
            "curl-state",
        ] {
            fs::create_dir_all(state.join(dir))?;
        }
        fs::write(state.join("gh-state/releases.json"), "[]\n")?;
        fs::write(state.join("gh-state/next-id"), "100\n")?;
        fs::write(state.join("stable-version"), "1.99.1\n")?;
        let world = Self { fixture, state };
        world.ci_green()?;
        Ok(world)
    }

    /// The commit the workflow releases, and the `expected_sha` input.
    pub fn head(&self) -> Result<String> {
        self.fixture.git(&self.fixture.work, &["rev-parse", "HEAD"])
    }

    /// Script one `build.yml` run and the jobs endpoint that reports it.
    pub fn script_ci(&self, runs: &[CiRun], jobs: &[String]) -> Result<()> {
        let json = runs.iter().map(CiRun::json).collect::<Vec<_>>().join(",");
        self.route(
            "actions_workflows_build.yml_runs",
            &format!(
                r#"{{"total_count":{},"workflow_runs":[{json}]}}"#,
                runs.len()
            ),
        )?;
        for (index, run) in runs.iter().enumerate() {
            let jobs = jobs.get(index).map_or("[]", String::as_str);
            let count = jobs.matches("\"name\"").count();
            self.route(
                &format!("actions_runs_{}_jobs", run.id),
                &format!(r#"{{"total_count":{count},"jobs":{jobs}}}"#),
            )?;
        }
        Ok(())
    }

    /// A green `build.yml` push run for the current head.
    pub fn ci_green(&self) -> Result<()> {
        let run = CiRun::green(10, &self.head()?);
        self.script_ci(std::slice::from_ref(&run), &[ci_jobs(&run, "success")])
    }

    /// Record a tag the remote has; `annotated` gives it a peeled tag object
    /// pointing at `commit`.
    pub fn add_tag(&self, tag: &str, annotated: bool, commit: &str) -> Result<()> {
        let object = format!("tagobject-{tag}");
        let kind = if annotated {
            self.route(
                &format!("git_tags_{object}"),
                &format!(r#"{{"object":{{"type":"commit","sha":"{commit}"}}}}"#),
            )?;
            "tag"
        } else {
            "commit"
        };
        let sha = if annotated { object } else { commit.to_owned() };
        self.write(
            &format!("gh-state/tagrefs/{tag}"),
            &format!(r#"{{"object":{{"type":"{kind}","sha":"{sha}"}}}}"#),
        )
    }

    /// The files a user can download for a published release of `tag`.
    pub fn published(&self, tag: &str) -> Result<Vec<(String, Vec<u8>)>> {
        let dir = self.state.join(format!("gh-state/published/{tag}"));
        let mut files = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            files.push((
                entry.file_name().to_string_lossy().into_owned(),
                fs::read(entry.path())?,
            ));
        }
        files.sort();
        Ok(files)
    }

    /// Put an existing release (published or draft) in the scripted service.
    pub fn add_release(&self, release: &Release) -> Result<()> {
        let names = release
            .assets
            .iter()
            .map(|(name, _)| format!(r#"{{"name":"{name}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let releases = fs::read_to_string(self.state.join("gh-state/releases.json"))?;
        let releases = releases.trim_end().trim_end_matches(']');
        let sep = if releases == "[" { "" } else { "," };
        let entry = format!(
            r#"{{"id":{},"tag_name":"{}","draft":{},"target_commitish":"{}","assets":[{names}]}}"#,
            release.id, release.tag, release.draft, release.target
        );
        self.write(
            "gh-state/releases.json",
            &format!("{releases}{sep}{entry}]\n"),
        )?;
        let assets = self.state.join(format!("gh-state/assets/{}", release.id));
        fs::create_dir_all(&assets)?;
        for (name, contents) in &release.assets {
            fs::write(assets.join(name), contents)?;
        }
        if release.latest {
            self.write("gh-state/latest", &release.tag)?;
        }
        Ok(())
    }

    /// Fail an operation of the scripted GitHub (empty status means 500).
    pub fn gh_fail(&self, operation: &str, status: &str) -> Result<()> {
        self.write(&format!("gh-state/fail/{operation}"), status)
    }

    /// Make the registry unreachable.
    pub fn registry_outage(&self) -> Result<()> {
        self.write("curl-state/outage", "")
    }

    /// Answer registry lookups with an HTTP status instead of a body.
    pub fn registry_status(&self, status: &str) -> Result<()> {
        self.write("curl-state/status", status)
    }

    /// The checksum crates.io reports for `name version`.
    pub fn registry_set(&self, name: &str, version: &str, checksum: &str) -> Result<()> {
        self.write(&format!("curl-state/crate-{name}-{version}"), checksum)
    }

    /// A command-boundary control file (see the fixture and fakes).
    pub fn inject(&self, control: &str) -> Result<()> {
        self.write(control, "")
    }

    /// Remove a control file, as a rerun after the injected failure.
    pub fn clear(&self, control: &str) -> Result<()> {
        fs::remove_file(self.state.join(control))?;
        Ok(())
    }

    /// One line per recorded boundary call, in the order they were made.
    pub fn calls(&self) -> Vec<String> {
        match fs::read_to_string(self.state.join("order.log")) {
            Ok(log) => log.lines().map(str::to_owned).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Record one boundary call of the runner itself (a `uses:` step).
    pub fn record(&self, line: &str) -> Result<()> {
        use std::io::Write;
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.state.join("order.log"))?;
        writeln!(log, "{line}")?;
        Ok(())
    }

    /// Calls recorded for one tool.
    pub fn tool_calls(&self, tool: &str) -> Result<Vec<String>> {
        self.fixture.calls(tool)
    }

    fn write(&self, relative: &str, contents: &str) -> Result<()> {
        let path = self.state.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
        Ok(())
    }

    fn route(&self, name: &str, contents: &str) -> Result<()> {
        self.write(&format!("gh-state/routes/{name}"), contents)
    }
}

/// Print a process result, for `--nocapture` logs.
pub fn log(label: &str, output: &std::process::Output) {
    println!("== {label}: {}\n{}", output.status, text(output));
}
