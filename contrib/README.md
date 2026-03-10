# contrib

Packaging and service-management assets for `gruezi`.

Current layout:

* `systemd/gruezi.service`: systemd unit for packaged installs
* `systemd/gruezi.env.example`: environment file installed as `/etc/gruezi/gruezi.env`
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

This is intended to become the packaging base for future Ansible deployments that install `.deb` or `.rpm` artifacts instead of pushing a raw release binary.
