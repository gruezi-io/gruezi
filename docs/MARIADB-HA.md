# MariaDB strict HA operator guide

This guide describes the experimental two-node MariaDB profile. It combines
Gruezi authority, a floating address, a MariaDB lifecycle adapter, and a Linux
watchdog. It is not a production-readiness statement, a fencing guarantee, or
a split-brain-proof design.

## What the components do

Gruezi owns election and VIP authority. The lifecycle adapter is the only
component in this profile that may change MariaDB's `read_only` state. The
adapter runs `check` before activation, records the database incarnation and
operation generation, and refuses activation when eligibility, GTID coverage,
quarantine, or dirty history is not proven.

The VIP is not a database write gate. A client can still connect directly to a
node, and a privileged MariaDB account can bypass `read_only`. Application
accounts must have neither `SUPER` nor `READ_ONLY ADMIN`. Keep the adapter
credentials in its option files. Never put credentials in YAML, command
arguments, logs, or this repository.

The adapter's example configuration is
`contrib/mariadb/mariadb-adapter.conf.example`. Install it as the local
adapter configuration, then place the local and peer option files below the
mode-0700 adapter configuration directory. Option files are regular files,
owned by the actuator account, and mode 0600. The adapter state directory uses
the same ownership and mode rules.

## Strict configuration

Start from `examples/ha-mariadb-node-a.yaml` and
`examples/ha-mariadb-node-b.yaml`. Replace node addresses, peer addresses, and
the shared authentication key for the deployment. The key shown in the
examples is a placeholder, not a credential.

Both nodes use `failure_policy: strict`, `preempt: false`, the same VIP, and
the packaged adapter path:

```yaml
ha:
  failure_policy: strict
  preempt: false
  lifecycle:
    activate_command: /usr/libexec/gruezi/gruezi-mariadb-adapter
    deactivate_command: /usr/libexec/gruezi/gruezi-mariadb-adapter
    timeout_ms: 2500
  watchdog:
    mode: required
    device: /dev/watchdog0
    timeout_ms: 30000
    safety_margin_ms: 2000
    nowayout: reject
```

`watchdog: required` is the deployment profile. Gruezi arms the watchdog during
ownership acquisition, after authority and resource checks. If acquisition
fails, required mode revokes authority and prevents ownership, while the daemon
may remain running. `automatic` tolerates only an acquisition failure that
leaves the device unarmed, logs that the node is unprotected, and continues
without watchdog coverage. Other watchdog failures still block acquisition.
The lab uses `off` because its containers do not expose a watchdog device. A
real deployment must not copy that lab setting.

`strict` chooses a safe outage when peer authority or required observations are
missing. `available` is an unsafe alternative for a single-writer MariaDB and
must not be used for exclusive ownership. `external` is Experimental and
requires a fence provider whose scope includes the database, not only the VIP.

## MariaDB grants

| Account | Required use | Required scope |
| --- | --- | --- |
| `gruezi_adapter` | Local lifecycle actuator | `READ_ONLY ADMIN`, `CONNECTION ADMIN`, `REPLICA MONITOR`, `PROCESS`, and the adapter's documented replication and receipt privileges |
| `gruezi_monitor` | Peer observation | `REPLICA MONITOR` and read access to the release receipt |
| `repl` | Replication | Replication privileges only |
| application account | Client traffic | Table access only, never `SUPER` or `READ_ONLY ADMIN` |

The exact grant checks and denied-control checks are part of the acceptance lab.
The lab-only root account is not a production design.

## Replication and safety limits

Use GTID replication with `gtid_strict_mode=ON`, `binlog_format=ROW`,
`log_bin=ON`, `log_slave_updates=ON`, no replication filters, and distinct
server IDs and GTID domains. `Seconds_Behind_Master` is never evidence of
delivery. Asynchronous replication cannot prove delivery by itself.

The adapter reports divergence as `detected` or `unknown`. Equal vectors do not
prove equal data or reconciled history. A quarantine latch survives convergence
and restart. Inspect it with:

```bash
sudo -u gruezi-actuator \
  /usr/libexec/gruezi/gruezi-mariadb-adapter quarantine-status
```

Clear it only after reconciling the history and recording the operator:

```bash
sudo -u gruezi-actuator \
  /usr/libexec/gruezi/gruezi-mariadb-adapter \
  quarantine-clear --i-have-reconciled-history --operator OPERATOR_ID
```

The adapter must run as the distinct `gruezi-actuator` account, not root and
not `gruezi-notify`. Its option files and state directory must be owned by that
account, with directories mode 0700 and option files mode 0600. The lifecycle
environment must be supplied by Gruezi, including `GRUEZI_EVENT`,
`GRUEZI_GENERATION`, `GRUEZI_INSTANCE_ID`, `GRUEZI_TRANSFER_ID`,
`GRUEZI_PEER_NONCE`, `GRUEZI_AUTHORITY_SOURCE`, `GRUEZI_AUTHORITY_TERM`, and
`GRUEZI_TIMEOUT_MS`; activation and deactivation also require their operation
mode variables. `HOME=/nonexistent` and the adapter configuration passthrough
are part of the actuator child environment. Do not run the recovery command
from a shell with guessed identity values.

For recovery from uncertainty, the complete adapter declaration is:

```bash
sudo -u gruezi-actuator \
  env HOME=/nonexistent \
  GRUEZI_EVENT=activate \
  GRUEZI_GENERATION=GENERATION \
  GRUEZI_INSTANCE_ID=INSTANCE_NONCE \
  GRUEZI_TRANSFER_ID=0 \
  GRUEZI_PEER_NONCE= \
  GRUEZI_AUTHORITY_SOURCE=peer \
  GRUEZI_AUTHORITY_TERM=AUTHORITY_TERM \
  GRUEZI_TIMEOUT_MS=TIMEOUT_MS \
  GRUEZI_ACTIVATION_MODE=cold \
  /usr/libexec/gruezi/gruezi-mariadb-adapter \
  declare-authoritative --i-accept-possible-data-loss \
  --incident-id INCIDENT_ID --reason "REASON" --operator OPERATOR_ID
```

This declaration is local, audited, one-shot, expires after 60 seconds, and
prints `authorization=pending`. The later activation consumes it and reports
`target=unknown`, `rpo=unknown`, `possible_data_loss=yes`, and
`divergence=unknown` when it is the allowed recovery path. The invocation above
is a contract example, not a claim that these placeholder values are valid or
that this recovery has been run here. It does not bypass Gruezi authority,
quarantine, local safety checks, or a dirty history record, and it does not
establish fencing.

## RPO and RTO

The conditional planned-handoff claim is RPO 0 for binlog-logged transactions
acknowledged on the predecessor before the release barrier, under deployment
assumptions A1 through A6. The claim is not established by this guide. The
acceptance obligations are separate:

| Obligation | Required named tests and existing evidence | Status |
| --- | --- | --- |
| PO-1 barrier capability | `release_barrier_stops_appliers_and_blocks_late_commit`, `release_barrier_readback_precedes_capture`, `task-8-activate-deactivate.log` | NOT RUN as a real MariaDB acceptance result |
| PO-2 receipt identity binding | `receipt_rejects_wrong_instance_generation_or_incarnation`, `receipt_rejects_stale_op_seq`, `receipt_digest_mismatch_refuses`, `task-8-activate-deactivate.log` | NOT RUN as a real MariaDB acceptance result |
| PO-3 target completeness | `planned_handoff_includes_acknowledged_last_token`, `late_queued_replication_commit_refuses_activation`, `master_gtid_wait_zero_but_coverage_missing_refuses`, `task-8-activate-deactivate.log`, `task-10-strict-cooperative-last-token-*.log`, `task-10-strict-late-queued-replication-*.log` | NOT RUN, Podman scenario execution was not performed |
| PO-4 origin coverage | `gtid_covers_cases`, `local_origin_vs_slave_pos_origin_mismatch_quarantines`, `log_slave_updates_off_refuses_rpo0`, `task-8-activate-deactivate.log` | NOT RUN as a real MariaDB acceptance result |
| PO-5 assumption surfacing | Per-assumption verified or assumed fields in the release result | NOT RUN |
| PO-6 divergence bounds | `lab_overlap_double_origin`, `lab_unlogged_privileged_write`, `lab_reset_master_history_loss`, `task-14-replication-scenarios.log` | NOT RUN, task 14 records these as NOT RUN |

Assumptions A1 through A6 are deployment conditions, not proof results:

1. A1, the release barrier is complete.
2. A2, no privileged or `sql_log_bin=0` writer acts during release.
3. A3, both nodes have `log_bin` and `log_slave_updates` enabled.
4. A4, no replication filter drops the target domain.
5. A5, `gtid_strict_mode=ON`.
6. A6, receipt and incarnation binding is unforgeable within the deployment
   trust boundary.

Until every PO row has a passing named test and log, the RPO label is
`conditional` and not an achieved result.

Crash or partition under `strict` has no automatic RTO. It is a safe outage
until recovery or an operator intervention. Forced asynchronous failover has no
zero-RPO claim. RTO is the sum of authority, database, adapter, VIP, and
application phases, not `withdraw_timeout_ms`.

The claim excludes `sql_log_bin=0` writes, privileged writes in the release
window, unlogged DDL, non-transactional anomalies, restores, backups, external
side effects, corruption, and forged GTIDs. MariaDB `read_only` is an
Experimental write restriction, not fencing. Privileged administrators,
replication threads, existing transactions, and direct connections remain
limits.

## Operator procedures

### Planned handoff

1. Confirm both nodes have current status and no quarantine or dirty history.
2. Confirm the predecessor is the only VIP owner and its application writes are
   quiesced.
3. Stop Gruezi gracefully on the predecessor. For the packaged service, run
   `sudo systemctl stop gruezi`; systemd sends SIGTERM to the service started
   by `contrib/systemd/gruezi.service`. For a foreground process, send
   `kill -TERM PID`. Gruezi's shutdown handler enters the cooperative release
   path only while authority and a fresh peer observation are valid.
4. Confirm the predecessor is passive, the VIP is absent there, and the
   successor reports a fresh activation and `read_only=0`.
5. Run a unique application write through the VIP and verify its origin and
   replication.

If authority or peer freshness is missing, shutdown takes the ordinary bounded
cleanup path instead of cooperative release. Do not enable a node by setting
`read_only=0` manually. If any release proof is missing, keep the service in a
safe outage.

### Ongoing verification

When lifecycle is configured, Gruezi reuses `activate_command` with
`GRUEZI_EVENT=verify`; there is no `verify_command` configuration key. The
verification period is `V = ha.lifecycle.timeout_ms`, and each verification
deadline is `D = ha.lifecycle.timeout_ms`. A successful verify is local and
read-only: it checks database identity, `read_only=0`, and the exact process
incarnation from the pid file and `/proc/<pid>` start time. Unknown or changed
incarnation is failure, not an approximation.

Freshness expires at `next_verify_at + D`, so detection is bounded by
`V + D`, and the configured contract requires `V + D <= 2 *
ha.command_timeout_ms`. Any failed, timed-out, stale, cancelled, unparseable,
or identity-mismatched verify revokes ownership and withdraws the VIP through
the compensation path. Verify proves ownership liveness and incarnation
continuity only. It is never release-finality evidence. Release finality still
requires the separate deactivate barrier, receipt, peer checks, and PO-1
through PO-6 evidence.

### Crash or partition

Under `strict`, do not promote the standby because the peer is silent. Inspect
the kernel VIP owner, Gruezi status, MariaDB `read_only`, replication threads,
and adapter quarantine state. Restore communication or use the audited
operator recovery action only after reviewing the possible data-loss result.

### Residual VIP

After a process crash, do not assume the VIP disappeared. At the affected node,
inspect the exact managed address with `ip address show dev eth0`, restart
Gruezi, and let startup cleanup remove residue before bootstrap. Confirm the
database is restricted before any new activation. Never use a substring match
or a health marker as proof.

## Acceptance suite

Build the release artifact used by the lab, then check prerequisites and run a
strict scenario:

```bash
cargo build --release --locked
scripts/mariadb-ha-lab/run.sh --check-prerequisites
scripts/mariadb-ha-lab/run.sh --profile strict --scenario strict-smoke
```

The lab also provides the adapter portability and credential-output checks:

```bash
bash scripts/mariadb-ha-lab/test-awk-portability.sh
bash scripts/mariadb-ha-lab/test-credential-output.sh
bash scripts/mariadb-ha-lab/test-timeout.sh
```

The strict lab is Experimental. Its containers run without a watchdog, and
container process tests cannot prove hardware watchdog behavior. See
`scripts/mariadb-ha-lab/README.md` for report fields, scenario limits, and the
conditional handoff tests. Measured results belong in the final evidence
report, not in this guide.

## Status vocabulary

This guide uses `Implemented` for shipped parser, adapter, and lifecycle
interfaces, `Experimental` for the strict MariaDB profile and `read_only`
restriction, `Planned` for work not present in the repository, and
`Unsupported` for guarantees the design does not provide.
