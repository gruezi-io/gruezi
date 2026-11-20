use anyhow::{Result, anyhow};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
pub const INVENTORY: &str = "[gruezi_ha_lab]\n\
    disposable-one ansible_host=10.88.0.10 ansible_user=ansible gruezi_interface=eth0 gruezi_vip=10.88.0.20/32\n\
    disposable-two ansible_host=10.88.0.11 ansible_user=ansible gruezi_interface=eth0 gruezi_vip=10.88.0.20/32\n";

/// A private miniature checkout: its authorization never reaches the worktree.
pub struct Fixture {
    pub root: PathBuf,
}

impl Fixture {
    pub fn new() -> Result<Self> {
        let root = std::env::temp_dir().join(format!(
            "gruezi-vm-guard-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root)?;
        let fixture = Self { root };
        for directory in ["scripts/mariadb-ha-lab/vm", "ansible", "bin", ".ssh"] {
            fs::create_dir_all(fixture.root.join(directory))?;
        }
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        for file in [
            "guard.sh",
            "run-vm.sh",
            "operations.sh",
            "kernel-sigkill.sh",
            "watchdog-arm-failure.sh",
        ] {
            let relative = format!("scripts/mariadb-ha-lab/vm/{file}");
            fs::copy(source.join(&relative), fixture.root.join(&relative))?;
        }
        fs::copy(
            source.join("scripts/test-watchdog-softdog.sh"),
            fixture.root.join("scripts/test-watchdog-softdog.sh"),
        )?;
        for name in ["drop", "netem", "collect"] {
            let file = format!("ansible/ha-{name}.yml");
            fs::copy(source.join(&file), fixture.root.join(&file))?;
        }
        fixture.inventory(INVENTORY)?;
        fs::write(
            fixture.root.join("ansible/vm-allowlist.txt"),
            "disposable-one 10.88.0.10\ndisposable-two 10.88.0.11\n",
        )?;
        fs::write(fixture.root.join(".ssh/known_hosts"), "# local fake pins\n")?;
        fs::write(fixture.root.join("calls"), "")?;
        for name in ["ssh", "ansible-playbook", "ansible-inventory"] {
            let script = format!(
                "#!/bin/bash\nprintf '%s\\n' \"$0 $*\" >>'{}/calls'\n\
                 if [[ \"$0\" == *ssh ]]; then cat >/dev/null; fi\n\
                 if [[ -f '{}/fail-apply' && \"$*\" == *gruezi_drop_state=present* ]]; then exit 17; fi\n",
                fixture.root.display(),
                fixture.root.display()
            );
            let path = fixture.root.join("bin").join(name);
            fs::write(&path, script)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
        }
        Ok(fixture)
    }

    pub fn inventory(&self, text: &str) -> Result<()> {
        fs::write(self.root.join("hosts.ini"), text)?;
        Ok(())
    }

    pub fn run(&self, args: &[&str], gates: &[(&str, &str)]) -> Result<Output> {
        let output = Command::new("bash")
            .arg(self.root.join("scripts/mariadb-ha-lab/vm/run-vm.sh"))
            .arg("--inventory")
            .arg(self.root.join("hosts.ini"))
            .args(args)
            .env_clear()
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            )
            .env("HOME", &self.root)
            .env("TMPDIR", &self.root)
            .envs(gates.iter().copied())
            .output()?;
        for entry in fs::read_dir(&self.root)? {
            assert!(
                !entry?
                    .file_name()
                    .to_string_lossy()
                    .starts_with("gruezi-vm-run."),
                "runner scratch leaked after exit"
            );
        }
        println!(
            "local_fake_transport_calls={}",
            self.calls()?.lines().count()
        );
        Ok(output)
    }

    pub fn calls(&self) -> Result<String> {
        Ok(fs::read_to_string(self.root.join("calls"))?)
    }

    pub fn operate(&self, row: &str) -> Result<Output> {
        let output = self.root.join("artifacts");
        self.run(
            &[
                "--row",
                row,
                "--target",
                "disposable-one",
                "--output",
                output
                    .to_str()
                    .ok_or_else(|| anyhow!("non UTF-8 fixture path"))?,
            ],
            &[
                ("GRUEZI_ALLOW_VM_FAULTS", "1"),
                ("GRUEZI_ALLOW_WATCHDOG_RESET", "1"),
            ],
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.root) {
            eprintln!("VM fixture cleanup failed: {error}");
        }
    }
}

pub fn check(output: &Output, expected: i32) -> Result<String> {
    let text = format!(
        "{}{}",
        String::from_utf8(output.stdout.clone())?,
        String::from_utf8(output.stderr.clone())?
    );
    assert_eq!(output.status.code(), Some(expected), "{text}");
    if expected == 2 {
        assert_eq!(
            text.lines()
                .filter(|line| line.contains("\tNOT RUN\t"))
                .count(),
            5,
            "a refused run must report all rows NOT RUN"
        );
    }
    println!("exit={expected}\n{text}");
    Ok(text)
}
