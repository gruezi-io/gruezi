use crate::mariadb_vm_guard_support::Fixture;
use anyhow::{Result, anyhow};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
};

#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Drop,
    Netem,
}

impl Kind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Drop => "drop",
            Self::Netem => "netem",
        }
    }

    pub const fn row(self) -> &'static str {
        match self {
            Self::Drop => "firewall-block",
            Self::Netem => "asymmetric-loss",
        }
    }
}

pub struct CleanupFixture {
    pub base: Fixture,
    kind: Kind,
    ansible: PathBuf,
}

impl CleanupFixture {
    pub fn new(kind: Kind, scenario: &str) -> Result<Self> {
        let base = Fixture::new()?;
        let ansible = std::env::split_paths(
            &std::env::var_os("PATH").ok_or_else(|| anyhow!("PATH unavailable"))?,
        )
        .map(|directory| directory.join("ansible-playbook"))
        .find(|path| path.is_file())
        .ok_or_else(|| anyhow!("actual ansible-playbook required for cleanup boundary tests"))?;
        fs::write(base.root.join("kind"), kind.name())?;
        fs::write(base.root.join("scenario"), scenario)?;
        fs::write(base.root.join("transport.log"), "")?;
        fs::write(
            base.root.join("ansible.cfg"),
            "[defaults]\nretry_files_enabled=False\n",
        )?;
        let fake = base.root.join("fake-ssh");
        fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/mariadb_vm_cleanup_support/fake-ssh.sh"),
            &fake,
        )?;
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755))?;
        // Only restoration dispatch reaches real Ansible. Collection and apply
        // remain local recorders, so no fault-injection module can execute.
        let wrapper = format!(
            r#"#!/bin/bash
set -euo pipefail
printf '%s\n' "$*" >>'{root}/calls'
case "$*" in
    *gruezi_drop_state=absent*|*gruezi_netem_state*absent*)
        exec '{ansible}' "$@" -e 'ansible_ssh_executable={fake} ansible_pipelining=true ansible_become=false ansible_python_interpreter=/usr/bin/python3' ;;
    *ha-collect.yml*)
        count=0
        [[ ! -f '{root}/collect-count' ]] || count="$(<'{root}/collect-count')"
        count=$((count+1))
        printf '%s' "$count" >'{root}/collect-count'
        if [[ -f '{root}/exit-path' && "$count" == 2 ]]; then exit 24; fi ;;
esac
"#,
            root = base.root.display(),
            ansible = ansible.display(),
            fake = fake.display(),
        );
        fs::write(base.root.join("bin/ansible-playbook"), wrapper)?;
        Ok(Self {
            base,
            kind,
            ansible,
        })
    }

    pub fn playbook(&self) -> Result<Output> {
        let options = format!(
            "gruezi_{}_state=absent gruezi_peer_ip=10.88.0.11 ansible_ssh_executable={} ansible_pipelining=true ansible_become=false ansible_python_interpreter=/usr/bin/python3",
            self.kind.name(),
            self.base.root.join("fake-ssh").display(),
        );
        Ok(Command::new("timeout")
            .arg("45")
            .arg(&self.ansible)
            .arg("-i")
            .arg(self.base.root.join("hosts.ini"))
            .arg(
                self.base
                    .root
                    .join(format!("ansible/ha-{}.yml", self.kind.name())),
            )
            .args(["--limit", "disposable-one", "-e", &options])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.base.root)
            .env("ANSIBLE_CONFIG", self.base.root.join("ansible.cfg"))
            .output()?)
    }

    pub fn runner(&self, exit_path: bool) -> Result<Output> {
        if exit_path {
            fs::write(self.base.root.join("exit-path"), "")?;
        }
        self.base.operate(self.kind.row())
    }

    pub fn transport_log(&self) -> Result<String> {
        Ok(fs::read_to_string(self.base.root.join("transport.log"))?)
    }
}

pub fn transcript(output: &Output) -> Result<String> {
    Ok(format!(
        "{}{}",
        String::from_utf8(output.stdout.clone())?,
        String::from_utf8(output.stderr.clone())?,
    ))
}
