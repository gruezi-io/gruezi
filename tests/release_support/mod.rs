//! Disposable repository fixture for release-recipe tests.
//!
//! Every fixture owns a private directory holding an isolated `HOME` and git
//! config, a throwaway SSH signing key with its allowed-signers file, a local
//! bare `origin.git`, a working clone that carries the real `.justfile`, and a
//! second clone used to move the remote. Only `cargo` and `gh` are faked, at
//! the command boundary: they log each call and obey control files in `bin/`
//! (`fail-<subcommand>`, `wrong-bump`, `verify-modify-unrelated`,
//! `verify-stage-unrelated`). Whether a recipe is right is decided by the real
//! recipe code and read back from refs, commits and call logs.

#[path = "../common/executable.rs"]
mod executable;

use anyhow::{Result, anyhow};
pub use executable::write_executable;
use std::{
    ffi::OsString,
    fs,
    io::ErrorKind,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command as StdCommand, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    task::JoinHandle,
    time::{Duration, timeout},
};

/// Upper bound for one recipe run. On expiry the recipe's whole process group is
/// killed (a hung fake tool must not outlive the test) and the test fails with the
/// recipe output and the fake-tool call logs.
const DEFAULT_RECIPE_TIMEOUT: Duration = Duration::from_secs(60);

/// Upper bound for collecting output once the process (group) is gone.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

const MANIFEST: &str = "[package]\nname = \"fixture\"\nversion = \"1.2.3\"\nedition = \"2024\"\n";
const LOCKFILE: &str = "version = 4\n\n[[package]]\nname = \"fixture\"\nversion = \"1.2.3\"\n";

/// Fake `cargo`: logs its arguments, edits the manifest and lockfile like
/// `cargo set-version --bump` / `cargo update` would, and fails on request.
const FAKE_CARGO: &str = r#"#!/bin/bash
set -eu
ctl="$(cd "$(dirname "$0")" && pwd -P)"
printf '%s\n' "$*" >> "$ctl/cargo.log"
sub="${1:-}"
if [[ -f "$ctl/fail-$sub" ]]; then
    echo "injected cargo $sub failure" >&2
    exit 1
fi
case "$sub" in
set-version)
    [[ "${2:-}" == --bump ]] || exit 2
    current="$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml)"
    IFS=. read -r major minor patch <<< "$current"
    case "${3:-}" in
        patch) patch=$((patch + 1)) ;;
        minor) minor=$((minor + 1)); patch=0 ;;
        major) major=$((major + 1)); minor=0; patch=0 ;;
        *) exit 2 ;;
    esac
    next="$major.$minor.$patch"
    if [[ -f "$ctl/wrong-bump" ]]; then next=9.9.9; fi
    sed -i "0,/^version = \".*\"/s//version = \"$next\"/" Cargo.toml
    sed -i "/^name = \"fixture\"\$/{n;s/.*/version = \"$next\"/}" Cargo.lock
    ;;
update)
    count="$(grep -c '^# refreshed' Cargo.lock || true)"
    printf '# refreshed %s\n' "$((count + 1))" >> Cargo.lock
    ;;
test)
    if [[ -f "$ctl/verify-modify-unrelated" ]]; then echo verified >> README.md; fi
    if [[ -f "$ctl/verify-stage-unrelated" ]]; then echo staged > staged.txt; git add staged.txt; fi
    ;;
esac
"#;

/// Fake `gh`: a publication boundary that only records that it was reached.
const FAKE_GH: &str = "#!/bin/bash\nprintf '%s\\n' \"$*\" >> \"$(dirname \"$0\")/gh.log\"\n";

pub struct ReleaseFixture {
    root: PathBuf,
    home: PathBuf,
    /// Fake tools and their control files; first on every command's `PATH`.
    pub bin: PathBuf,
    pub work: PathBuf,
    pub origin: PathBuf,
    pub other: PathBuf,
}

impl ReleaseFixture {
    /// Build a fixture whose `origin/develop` and `main` both sit on a clean
    /// 1.2.3 commit, with tag `1.2.2` already published.
    pub fn new() -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let run_tmp = std::env::temp_dir();
        fs::create_dir_all(&run_tmp)?;
        let root = run_tmp.join(format!(
            "fixture-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // create_dir is atomic: an existing directory is an error, never shared state.
        fs::create_dir(&root)?;
        let fixture = Self {
            home: root.join("home"),
            bin: root.join("bin"),
            work: root.join("work"),
            origin: root.join("origin.git"),
            other: root.join("other"),
            root,
        };
        for dir in [&fixture.home, &fixture.bin, &fixture.root.join("tmp")] {
            fs::create_dir(dir)?;
        }
        fixture.configure_signing()?;
        write_executable(&fixture.bin.join("cargo"), FAKE_CARGO)?;
        write_executable(&fixture.bin.join("gh"), FAKE_GH)?;
        fixture.seed()?;
        Ok(fixture)
    }

    fn configure_signing(&self) -> Result<()> {
        let key = self.home.join("signing_key");
        let key_arg = key.to_string_lossy().into_owned();
        run_checked(
            &mut self.isolated("ssh-keygen", &self.home),
            &[
                "-q", "-t", "ed25519", "-N", "", "-C", "fixture", "-f", &key_arg,
            ],
        )?;
        let public = fs::read_to_string(self.home.join("signing_key.pub"))?;
        let mut fields = public.split_whitespace();
        let (Some(kind), Some(material)) = (fields.next(), fields.next()) else {
            return Err(anyhow!("unexpected public key format: {public}"));
        };
        fs::write(
            self.home.join("allowed_signers"),
            format!("fixture@example.test namespaces=\"git\" {kind} {material}\n"),
        )?;
        fs::write(
            self.home.join("gitconfig"),
            format!(
                "[user]\n\tname = Release Fixture\n\temail = fixture@example.test\n\tsigningkey = {}\n\
                 [gpg]\n\tformat = ssh\n[gpg \"ssh\"]\n\tallowedSignersFile = {}\n\
                 [init]\n\tdefaultBranch = develop\n[commit]\n\tgpgsign = false\n[tag]\n\tgpgsign = false\n",
                key.display(),
                self.home.join("allowed_signers").display(),
            ),
        )?;
        Ok(())
    }

    fn seed(&self) -> Result<()> {
        let origin = self.origin.to_string_lossy().into_owned();
        let work = self.work.to_string_lossy().into_owned();
        let other = self.other.to_string_lossy().into_owned();
        self.git(&self.root, &["init", "--bare", "--quiet", &origin])?;
        self.git(&self.root, &["clone", "--quiet", &origin, &work])?;
        fs::write(self.work.join("Cargo.toml"), MANIFEST)?;
        fs::write(self.work.join("Cargo.lock"), LOCKFILE)?;
        fs::write(self.work.join("README.md"), "fixture\n")?;
        // The real recipes under test, not a copy written for the fixture.
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join(".justfile"),
            self.work.join(".justfile"),
        )?;
        self.git(&self.work, &["add", "--all"])?;
        self.git(
            &self.work,
            &["commit", "--quiet", "-m", "Start the fixture at 1.2.3"],
        )?;
        self.git(&self.work, &["tag", "1.2.2"])?;
        self.git(
            &self.work,
            &[
                "push",
                "--quiet",
                "origin",
                "develop",
                "develop:refs/heads/main",
                "1.2.2",
            ],
        )?;
        self.git(&self.root, &["clone", "--quiet", &origin, &other])?;
        Ok(())
    }

    /// A command confined to the fixture: cleared environment (so no publishing
    /// token or SSH agent leaks in), fixture `HOME`/git config, fake tools first
    /// on `PATH`, no stdin.
    fn isolated(&self, program: &str, dir: &Path) -> StdCommand {
        let mut path = OsString::from(&self.bin);
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        let mut command = StdCommand::new(program);
        command
            .current_dir(dir)
            .env_clear()
            .env("PATH", path)
            .env("HOME", &self.home)
            .env("GIT_CONFIG_GLOBAL", self.home.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("TMPDIR", self.root.join("tmp"))
            .env("LANG", "C")
            .stdin(Stdio::null());
        command
    }

    /// Run git in `dir` and return its trimmed stdout.
    pub fn git(&self, dir: &Path, args: &[&str]) -> Result<String> {
        let output = self.isolated("git", dir).args(args).output()?;
        if !output.status.success() {
            return Err(anyhow!("git {args:?} failed: {}", text(&output)));
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }

    /// Run a program in the working clone with extra environment variables, in
    /// its own process group. After [`RECIPE_TIMEOUT`] the group is killed and the
    /// error carries the partial output plus the fake-tool call logs.
    pub async fn run(&self, program: &str, args: &[&str], env: &[(&str, &str)]) -> Result<Output> {
        let mut std_command = self.isolated(program, &self.work);
        std_command
            .args(args)
            .envs(env.iter().copied())
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = Command::from(std_command).kill_on_drop(true).spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| anyhow!("just exited before its pid was read"))?;
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(anyhow!("just output pipes unavailable"));
        };
        let (stdout, stderr) = (tokio::spawn(drain(stdout)), tokio::spawn(drain(stderr)));
        let timeout_seconds = std::env::var("GRUEZI_RELEASE_TEST_TIMEOUT_SECONDS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map_or(DEFAULT_RECIPE_TIMEOUT, Duration::from_secs);
        let finished = timeout(timeout_seconds, child.wait()).await;
        if finished.is_err() {
            // The group id is the pid: this reaches every descendant, fake tools included.
            run_checked(
                &mut self.isolated("bash", &self.work),
                &[
                    "-c",
                    "kill -KILL -- \"-$1\"",
                    "kill-group",
                    &pid.to_string(),
                ],
            )?;
            child.wait().await?;
        }
        let (stdout, stderr) = (collect(stdout).await?, collect(stderr).await?);
        match finished {
            Ok(status) => Ok(Output {
                status: status?,
                stdout,
                stderr,
            }),
            Err(_) => Err(anyhow!(
                "{program} {args:?} still running after {timeout_seconds:?}; process group killed\n\
                 --- output ---\n{}{}\n--- cargo calls ---\n{}\n--- gh calls ---\n{}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr),
                self.calls("cargo")?.join("\n"),
                self.calls("gh")?.join("\n"),
            )),
        }
    }

    /// Calls the named fake tool recorded, one line per invocation.
    pub fn calls(&self, tool: &str) -> Result<Vec<String>> {
        match fs::read_to_string(self.bin.join(format!("{tool}.log"))) {
            Ok(log) => Ok(log.lines().map(str::to_owned).collect()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for ReleaseFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Stdout and stderr of a finished process, for assertion messages and logs.
pub fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn run_checked(command: &mut StdCommand, args: &[&str]) -> Result<()> {
    let output = command.args(args).output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!("{command:?} failed: {}", text(&output)))
    }
}

async fn drain(mut pipe: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut buffer = Vec::new();
    pipe.read_to_end(&mut buffer).await?;
    Ok(buffer)
}

async fn collect(reader: JoinHandle<Result<Vec<u8>>>) -> Result<Vec<u8>> {
    timeout(DRAIN_TIMEOUT, reader).await??
}
