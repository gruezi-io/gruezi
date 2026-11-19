# contrib

Packaging and service-management assets for `gruezi`.

Current layout:

* `systemd/gruezi.service`: systemd unit for packaged installs
* `systemd/gruezi.env.example`: environment file installed as `/etc/gruezi/gruezi.env`
* `systemd/gruezi-sysusers.conf`: system accounts `gruezi-actuator` and `gruezi-notify` (installed as `/usr/lib/sysusers.d/gruezi.conf`)
* `systemd/gruezi-tmpfiles.conf`: owner-only directories `/etc/gruezi/mariadb` and `/var/lib/gruezi/mariadb-adapter` (installed as `/usr/lib/tmpfiles.d/gruezi.conf`)
* `mariadb/gruezi-mariadb-adapter`: MariaDB adapter, installed as `/usr/libexec/gruezi/gruezi-mariadb-adapter`
* `mariadb/mariadb-adapter.conf.example`: adapter option file example, installed under `/usr/share/doc/gruezi/examples/`
* `debian/`: maintainer scripts for `cargo deb`

Build flow:

```bash
cargo build --release --locked
cargo deb --no-build
cargo generate-rpm
```

Notes:

* the package installs the binary at `/usr/bin/gruezi`
* the systemd unit expects the active config at `/etc/gruezi/gruezi.yaml`
* the package installs `/etc/gruezi/gruezi.env` for verbosity and OTEL overrides
* the service unit runs as `root` because HA VIP add/remove operations require network administration privileges
* the post-install script enables the service and only tries to start it when `/etc/gruezi/gruezi.yaml` exists
* the post-install script creates `gruezi-actuator` and `gruezi-notify` (system users, no login shell, no home directory) and the two MariaDB adapter directories, owned by `gruezi-actuator` with mode `0700`. It uses `systemd-sysusers` and `systemd-tmpfiles` when present and falls back to `useradd` and `install`. The same script is embedded in the `.deb` and `.rpm` packages, and creation is idempotent.
* the MariaDB adapter option files are operator-provisioned with mode `0600`; the packages never create them
* the adapter runs under the separate accounts above, and its protection rests on file ownership and modes plus `no_new_privs`. Running under separate accounts does not isolate it from processes running as the same UID

This is intended to become the packaging base for future Ansible deployments that install `.deb` or `.rpm` artifacts instead of pushing a raw release binary.
