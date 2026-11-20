# Experimental disposable VM fault runner

This is opt-in operation tooling, not VM acceptance evidence. No VM is authorized
by this repository. Task 16 requires the operator to name the disposable hosts
and approve the specific rows, including any reboot, before execution.
`maria-a`, `maria-b`, and `192.168.255.50` through `.52` are always refused,
including when embedded in labels, inventory paths or content, and allowlists.
There is no override.

Use the restricted static INI shape in `inventory.ini.example`. Exactly two
hosts are required; every field is mandatory and in the shown order. Labels
are identifiers, not SSH aliases: both label and canonical RFC1918 IPv4 address
must match a row in the ignored `ansible/vm-allowlist.txt`. No inventory plugin,
DNS resolution, extra variables, external addresses, SSH proxy, or ambient
Ansible inventory/configuration is evaluated. The example allowlist is never
loaded. There is no default inventory, target or fault.

Offline planning requires that explicit inventory and allowlist, but no opt-in
environment gates and makes no SSH or Ansible calls:

```bash
bash scripts/mariadb-ha-lab/vm/run-vm.sh --inventory /path/disposable.ini --list
bash scripts/mariadb-ha-lab/vm/run-vm.sh --inventory /path/disposable.ini --dry-run
```

Each of the five rows is printed as `NOT RUN`, with its required environment.
Actual execution also requires `--row`, `--target` (one label), and `--output`
(a new absolute local artifact directory). It requires
`GRUEZI_ALLOW_VM_FAULTS=1`; `watchdog-reset` additionally requires
`GRUEZI_ALLOW_WATCHDOG_RESET=1`. The local SSH known_hosts must already pin
the IP addresses. Connections ignore SSH configuration, forbid proxies, use
strict host key checks, and have bounded connection and operation deadlines.
The runner never deploys a binary or changes an existing service configuration.

The catalogue is fixed:

| Row | Operation and restoration |
| --- | --- |
| `kernel-sigkill` | Require active `gruezi.service`, `Restart=no`, current ownership, no active host/systemd watchdog; kill its one MainPID, print kernel addresses and residual-writer limitation, start the service on exit |
| `watchdog-reset` | Stream the existing `scripts/test-watchdog-softdog.sh` to the selected host; its required-watchdog arm/feed/disarm/SIGSTOP sequence runs in its own namespace and deliberately resets that host |
| `watchdog-arm-failure` | Run an isolated namespace probe with a nonexistent required-watchdog device; require failure without a VIP; remove the namespace and scratch files |
| `firewall-block` | Reuse `ansible/ha-drop.yml` on the selected host, collect both hosts, remove the rule on normal or failed exit |
| `asymmetric-loss` | Reuse `ansible/ha-netem.yml` with fixed `loss 100%` on the selected host, collect both hosts, clear qdisc on normal or failed exit |

`ansible/ha-collect.yml` runs before and after faults, using private copies of
the three playbooks and a generated inventory with only validated values.
Artifacts survive in `--output`; temporary inventories/configuration are removed.
An interrupted controller (SIGKILL or power failure) cannot guarantee restoration.
Existing qdisc state must be disposable: the reused netem playbook replaces it.
Collection and restoration failures are nonzero and require operator recovery.
Restoration first reads and parses complete OUTPUT rules or JSON qdisc state.
Only independently observed absence permits skipping deletion. A deletion error,
failed or unparseable observation, remaining managed drop rule, netem qdisc or
managed root handle `1:` makes restoration fail. Diagnostic listing success
alone is never proof of restoration. Both normal and EXIT restoration retain the
cleanup status and print `manual recovery required` with the kind, target and
exit code when absence is unconfirmed. The runner clears its pending cleanup
only after the playbook confirms absence; an earlier normal-path failure stays
nonzero even if EXIT restoration subsequently succeeds.

An executed transport is reported `EXECUTED`, never `PASS`. The collector does
not measure MariaDB SQL state, unique acknowledged writes, replication, RTO or
divergence. Task 16 must independently obtain those observations and assign the
acceptance verdict. The watchdog probes use the existing isolated `available`
test configuration; they establish kernel mechanics, not strict MariaDB
failover behavior. A reset disconnect is not reset proof: collect boot identity
and the retained watchdog log after reboot. SIGKILL can leave a writable database;
`read_only` is an Experimental write restriction, never fencing. These limitations
must remain in any evidence report.
