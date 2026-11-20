# MariaDB master-master acceptance lab

Experimental Linux acceptance fixture. It uses two Podman pods on the private
`10.249.15.0/24` network. Each pod shares one namespace between MariaDB 11.8 and
a release Gruezi binary. Gruezi gets NET_ADMIN and NET_RAW for the VIP and ARP.
Do not run on a host routing that subnet. No host ports are published.

```bash
cargo build --release --locked
scripts/mariadb-ha-lab/run.sh --check-prerequisites
scripts/mariadb-ha-lab/run.sh --force --with-hook
scripts/mariadb-ha-lab/run.sh --profile strict --scenario strict-smoke
```

On Rocky Linux install prerequisites with
`sudo dnf install podman iproute coreutils`. The runner uses the current user's
Podman connection and records its first error if unavailable. Containers must support
user-defined bridge networks and NET_ADMIN. All Podman and SQL commands have
deadlines; observable predicates have a 60 second deadline. The 200 ms poll
interval never determines success. A timeout prints kernel addresses, status,
last rows, replication status and Gruezi logs and exits nonzero.

The fixture builds its own image from `target/release/gruezi`; the repository's
root Containerfile still uses a debug binary. MariaDB replicates asynchronously
in both directions using binlog positions captured before writes, distinct
server IDs and GTID domains, ROW binlogs, log-slave-updates, and odd/even
auto-increment offsets. `MARIADB_IMAGE` can override the default image.
Generated credentials and the fresh 32 byte HA key exist only in a private
temporary directory, published atomically to the containers and deleted on
exit. No credentials are committed.

Each scenario emits one PASS or FAIL with the kernel VIP owner and the matching
row token plus originating `@@server_id` from each database. The writer loop
reconnects through the VIP with bounded retries; it asserts that the inserted
origin matches the independently observed owner. Row tokens make an ambiguous
client response observable without inserting duplicate writes.

| Scenario | Injected fault and acceptance observation |
| --- | --- |
| normal | Exactly one kernel VIP; VIP writer row originates there and appears identically on both databases. |
| unfenced-direct-write | An unprivileged direct connection to the non-owner inserts a row successfully; both copies identify that non-owner as the origin. This PASS captures a limitation. |
| graceful-failover | SIGTERM Gruezi owner; kernel VIP moves to successor; new VIP writes originate there and replicate. |
| active-crash | SIGKILL the entire owner pod, remove its namespace, retain data volume; surviving kernel gains VIP and accepts writes. Restart the old DB and prove replication catches up. |
| partition | Drop HA UDP in both directions with iptables; both kernel namespaces gain the VIP under `available`. Replication remains healthy. PASS means the expected unsafe overlap was observed. |
| replication-link-loss | STOP SLAVE both ways while HA remains reachable; VIP owner stays the same, its new row is absent on the other server. HA health is not replication health. |
| recovery | START SLAVE; entire ordered row sets converge and a single kernel VIP remains after healing. |
| read-only-hook (optional) | Start with both servers read-only and configure the example hook; owner becomes writable, non-owner stays read-only, direct non-owner write fails specifically with ERROR 1290, VIP write replicates. |
| read-only-hook-handoff (optional) | SIGTERM the owner; demote sets read_only=1 and promote sets read_only=0 on successor; VIP writes continue. |

The explicit policy is `available` to demonstrate automatic takeover and its
partition hazard. This fixture does not establish exclusive ownership during
partitions. A process-only SIGKILL leaves the pod namespace and its VIP alive;
the host-crash scenario deliberately destroys that namespace, and must not be
quoted as evidence of safe process-crash fencing.

Gruezi decides VIP ownership only. Removing a VIP does not stop direct
connections or conflicting writes. Strict single-writer behavior needs separate
application/database fencing. `read-only-hook.sh` is the recommended example
pattern for ordinary application users: promote clears `read_only`, while
demote, backup and fault set it. It consumes `GRUEZI_EVENT`, `GRUEZI_NODE_ID`
and `GRUEZI_GROUP_ID`; Gruezi also provides `GRUEZI_INTERFACE`, `GRUEZI_STATE`,
`GRUEZI_PREVIOUS_STATE`, `GRUEZI_PRIORITY` and optional reason/peer variables
(see `src/gruezi/hooks.rs`). `MARIADB_HOOK_DEFAULTS` can select a protected client
credentials file in another deployment.

Hooks are asynchronous notifications and are not an exclusive DB grant.
`read_only` does not restrict privileged administrators or replication threads,
does not revoke existing transactions, and a crashed/frozen process cannot run
its demote hook. Two owners under `available` can both run promote hooks.
Production crash/partition fencing needs an independent authority and an
application-specific design.

The EXIT/INT/TERM/error traps remove lab pods, containers, volumes, network,
generated credentials and local images, and print cleanup receipts. Upstream
base images remain cached. A previous SIGKILL of the runner can leave resources;
`--force` explicitly destroys only the names prefixed `gruezi-mariadb-lab`.
No existing resources are touched without that flag.
Concurrent adapter compatibility probes named `gruezi-mariadb-lab-fix*` are
reported separately by the resource audit and are never removed by this lab.

`run.sh` loads `lib.sh` and every `scenarios/*.sh` file. The default profile is
`available`; its nine scenarios retain their original operations and order.
`--scenario <id>` selects one result. Available scenarios execute preceding
scenarios as setup because they share a sequence of faults and recovery; only
the selected scenario emits a result. Selecting a hook scenario enables hooks.
Strict scenarios run smoke prerequisites first, then the selected scenario.
`--test-report-emitter` tests the report vocabulary and serialization without
containers. Helpers impose command deadlines; a timeout is a failure.
New scenario files register their profile, ID and function with
`register_scenario`; the loader discovers them through the glob.
`bash scripts/mariadb-ha-lab/test-awk-portability.sh` runs the complete adapter
test executable under host awk and container gawk. Its shell stays PID 1 so
incarnation tests can compare the test process against a different live PID.

The strict profile is Experimental. It configures strict policy, disables
preemption, and gates VIP ownership through the installed MariaDB lifecycle
adapter. Watchdog is off because pods expose no watchdog device. Pod process
crashes cannot prove fencing. `read_only` is an Experimental write restriction.
The smoke scenario verifies that both databases start read-only, exactly one
kernel owns the VIP, an `app` write through that VIP records the owner's
`@@server_id`, and an app write directly to the standby receives ERROR 1290.
A separate lab-root write to the standby succeeds and is recorded as the
privileged bypass. It is unlogged and removed after observation; GTID equality
cannot detect such writes.

Strict MariaDB servers persist `read_only=ON`, `gtid_strict_mode=ON` and
`event_scheduler=OFF` in their startup arguments. They use distinct server IDs
and domains 1 and 2, ROW binlogs, log-slave-updates, odd/even auto-increment
offsets and no replication filters. Schema and the singleton MEMORY receipt
table are provisioned with session binlogging disabled. One binlogged sentinel
transaction is then committed in each node's own domain before starting GTID
replication with `MASTER_USE_GTID=slave_pos`. Both executed vectors must contain
both domains before Gruezi starts.

The private host temporary directory is mode 0700 and mounted read-only at
`/lab`. Generated option files are mode 0600. Accounts have separate passwords
and option files: app has SELECT/INSERT on the fixture table; repl has
REPLICATION SLAVE; gruezi_monitor has REPLICA MONITOR and receipt-table SELECT;
gruezi_adapter has READ_ONLY ADMIN, CONNECTION ADMIN, REPLICA MONITOR, PROCESS,
REPLICATION SLAVE ADMIN, BINLOG ADMIN and receipt-table CRUD. Root is lab-only.
The monitor denied-control test must receive ERROR 1142 on DELETE.

Container root copies the node's adapter files from `/lab` into
`/etc/gruezi/mariadb`, owned by `gruezi-actuator`, directory 0700 and files 0600.
The state directory has the same owner and mode. The child therefore does not
need to traverse the root-only bind mount. `gruezi-notify` has a distinct UID
and owns neither directory; the same-UID case is eliminated by construction
in lifecycle mode. The smoke records denied reads of credentials, state, and
the actuator's `/proc/<pid>/fd`. Root and a compromised daemon remain outside
this boundary. Strict pods share the PID namespace and a dedicated
`gruezi-mariadb-lab-run-*` volume at `/run/mysqld`, read-only in the Gruezi
container, so the adapter derives incarnation from the actual pidfile and
`/proc` start time. Unknown identity fails closed.

`strict-cooperative-last-token` acknowledges a unique application token before
SIGTERM handoff, checks its presence on the successor, verifies the receipt
digest and live target, repeats `MASTER_GTID_WAIT=0` and origin-aware coverage,
and records measured failover time. Its RPO label remains conditional pending
the full PO-1 through PO-6 evidence. A1 is barrier completeness; A2 excludes
privileged or unlogged writers during release; A3 requires binlogging and
log-slave-updates; A4 excludes filters; A5 requires GTID strict mode; A6 assumes
receipt and incarnation integrity inside the deployment trust boundary.
Before handoff, lightweight observers start in the existing pods. They record
monotonic kernel samples in both DB namespaces and bracket one fresh app VIP
INSERT with successor `/status` owned=true, valid authority, kernel presence,
and read_only=0. The client token must originate on server 2, and the old
acknowledged token must remain present with origin 1. An app-client error is
terminal; it is never hidden by retrying the INSERT. Retained kernel samples
must show the predecessor absent throughout the observed window, including
samples before and after it within 200 ms.

The scenario separately checks receipt digest, barrier and exact GTID coverage,
then requires later peer-consent expiry, no kernel owner, read_only=1/1 and
successor owned=false before restarting the predecessor. This proves activation
and token preservation, not continued availability without the peer.
`LAB_COMPARE_LEGACY_OBSERVATION=1` also runs the old sequential host observation,
logging each predicate's monotonic interval, value and actual exit. The QA-only
`LAB_HOST_OBSERVATION_DELAY_MS` injects 0 through 2000 ms per host observation;
it does not affect the daemon, authority or prearmed local observer.
`strict-late-queued-replication` queues an event in the predecessor's relay log
with the SQL thread stopped, performs release, then resumes the IO stream.
The stopped SQL applier must block that queued commit after capture.

Reports append to `$REPORT_DIR/report.tsv` and render to `report.txt`
(`REPORT_DIR` defaults to `/tmp/gruezi-mariadb-lab-report`). Each record includes
a fresh run ID, profile and scenario, result, kernel ownership, status authority
and fencing, initial/current read-only state, replication threads and GTIDs,
unique client token and origin, timing, receipt and coverage evidence, server
version and the pulled image digest. Each record is also printed. The derived
DB image uses that digest, so a moving tag cannot change it mid-run.
`MARIADB_IMAGE` overrides the MariaDB 11.8 default. PASS, FAIL and NOT RUN are
distinct; divergence is only `detected` or `unknown`, and data-loss possibility
stays unknown without proof-obligation evidence.

Privilege evidence uses only information-schema scope and privilege names;
authentication columns and `SHOW GRANTS` are never printed. Set
`LAB_TRANSCRIPT` to the current tee transcript to audit that transcript and
report artifacts against the generated passwords, native-password verifiers
and HA key before private credentials are destroyed. The auditor prints counts
only. `bash scripts/mariadb-ha-lab/test-credential-output.sh` exercises both the
smoke logging regression and contaminated negative controls.
`bash scripts/mariadb-ha-lab/test-timeout.sh` deliberately exits 1 after a real
60-second unsatisfied wait, with a FAIL record, diagnostics and cleanup.

Limits outside every guarantee include unlogged writes, privileged writes in
the release window, unlogged DDL, non-transactional anomalies, restores and
backups, external side effects, corruption and forged GTIDs. Equal vectors do
not prove equal data or reconciled history.
