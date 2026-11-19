# MariaDB master-master acceptance lab

Experimental Linux acceptance fixture. It uses two Podman pods on the private
`10.249.15.0/24` network. Each pod shares one namespace between MariaDB 11.4 and
a release Gruezi binary. Gruezi gets NET_ADMIN and NET_RAW for the VIP and ARP.
Do not run on a host routing that subnet. No host ports are published.

```bash
cargo build --release --locked
scripts/mariadb-ha-lab/run.sh --check-prerequisites
scripts/mariadb-ha-lab/run.sh --force --with-hook
```

On Rocky Linux install prerequisites with
`sudo dnf install podman iproute coreutils`. Podman is tried rootless first,
then `sudo -n podman` if rootless `podman info` fails. Containers must support
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
