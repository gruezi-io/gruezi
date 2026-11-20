# MariaDB adapter for Gruezi strict HA

`gruezi-mariadb-adapter` is a bash CLI that owns every MariaDB interaction for
Gruezi's strict 2-node HA mode. **Implemented:** bounded SQL, origin-aware GTID
parsing, eligibility, permanent quarantine, dirty epochs, `activate`,
`deactivate`, read-only `verify`, and audited one-shot authorization.
**Experimental:** MariaDB `read_only` as a write restriction. Tests here use
a stateful fake client. Real MariaDB cancellation, privilege sufficiency,
and last-token preservation remain laboratory acceptance obligations.

## Safety boundaries (read this first)

- **This is not write fencing.** Nothing here can stop a privileged or
  out-of-band writer. The adapter restricts writes and terminates sessions;
  a fencing strategy remains a separate deployment responsibility.
- **Not a trust root.** The quarantine latch is hazard memory: it only ever
  makes outcomes worse. Only an explicit, audited operator authorization can
  bypass the limited connectivity checks described below.
- **Credential scope.** Option files must be regular non-symlinks, mode 0600,
  owned by the adapter's execution UID, beneath a mode-0700 directory owned by
  the same UID; `STATE_DIR` follows the same rule. This protects credentials
  from *other* UIDs. Same-UID isolation (e.g. against notification children) is
  explicitly **Unsupported** at this layer and is separate work (B4).
- The adapter refuses to run as root (`id -u` must not be 0) and refuses
  symlinks anywhere in its config, option-file and state paths.
- Divergence vocabulary is exactly `detected` or `unknown`. A node with no
  prior evidence is `unknown` - never "clean", never "none", never
  "not_detected".

## Files

| Path | Purpose |
| --- | --- |
| `gruezi-mariadb-adapter` | the adapter (bash, `set -euo pipefail`, no `eval`) |
| `mariadb-adapter.conf.example` | credential-free example configuration |
| `/etc/gruezi/mariadb-adapter.conf` | default configuration location (override with `GRUEZI_MARIADB_ADAPTER_CONF`) |
| `/etc/gruezi/mariadb/*.cnf` | option files holding all credentials |
| `/var/lib/gruezi/mariadb-adapter` | `STATE_DIR` (quarantine, dirty history, receipt state) |

## Configuration

A closed set of literal `key=value` lines; every key required exactly once.
Parsing is data-only: no shell sourcing, no expansion, no `eval`. Unknown or
duplicate keys, values outside `[A-Za-z0-9_./:-]`, relative paths and
non-canonical numbers are refused (exit 13). Keys:

`OWN_SERVER_ID`, `PEER_SERVER_ID` (1..4294967295, must differ),
`OWN_DOMAIN`, `PEER_DOMAIN` (0..4294967295, must differ),
`LOCAL_OPTION_FILE`, `PEER_OPTION_FILE` (absolute, must differ),
`STATE_DIR` (absolute), `CONNECT_TIMEOUT_S`, `STATEMENT_TIMEOUT_S`,
`COMMAND_TIMEOUT_S`, `MAX_APPLY_WAIT_S` (1..86400).

## Bounded SQL

SQL uses the following bounded client invocation. Enabling writes additionally
uses an interactive invocation with `--unbuffered` so the connection identity
can be recorded before enable:

```
timeout --signal=KILL COMMAND_TIMEOUT_S mariadb \
    --defaults-extra-file=<option-file> \
    --connect-timeout=CONNECT_TIMEOUT_S \
    --init-command="SET SESSION max_statement_time=STATEMENT_TIMEOUT_S" \
    --batch --skip-column-names
```

SQL is passed on stdin; credentials live only in the option file, so no secret
ever appears on argv. A wall-clock timeout is exit 12; any other client
failure is exit 13; output is parsed strictly and any unparseable value is
exit 13. Never a guess, never a default.

The session initialization bounds statements on that connection without
changing global server settings. MariaDB 11.8's client rejects
`--max-statement-time`; the timeout is a session system variable.

Mixed eligibility and release-snapshot reads use `--column-names` instead:
MariaDB 11.8 also suppresses vertical field names with `--skip-column-names`.
The adapter removes the `@@global.*` SELECT header lines from those fixed
queries while preserving the named `SHOW ALL SLAVES STATUS\G` fields.
Strict-mode and binlog booleans accept exactly `1`/`0` or `ON`/`OFF`, normalized
to `ON`/`OFF` before eligibility evaluation; other values exit 13.

## `check` (read-only eligibility)

One bounded round trip per node. The exact statements:

```
SELECT @@global.server_id,@@global.gtid_domain_id,@@global.gtid_strict_mode,
       @@global.log_bin,@@global.log_slave_updates,@@global.binlog_format,
       @@global.slave_skip_errors,@@global.pid_file;
SELECT @@global.gtid_binlog_pos;
SELECT @@global.gtid_binlog_state;
SELECT @@global.gtid_slave_pos;
SHOW ALL SLAVES STATUS\G
```

Evidence rules:

- The applied position always comes from the separate
  `SELECT @@global.gtid_slave_pos` statement. The `Gtid_Slave_Pos` column of
  `SHOW ALL SLAVES STATUS` is ignored even when present.
- `SHOW ALL SLAVES STATUS` (not the ordinary `SHOW REPLICA STATUS`) is parsed
  BY FIELD NAME; it carries `Connection_name`, and exactly one channel row is
  accepted. Required fields: `Slave_IO_Running`, `Slave_SQL_Running`,
  `Last_IO_Errno`, `Last_SQL_Errno`, `Using_Gtid`, `Gtid_IO_Pos`,
  `Connection_name`. Any `Replicate_*` filter with a value is a finding.
- `Seconds_Behind_Master` is never read and never evidence.
- `gtid_current_pos` is never a coverage target. Release snapshots read it
  only as a live-equality sample.

Report: `key=value` lines including every verified variable, the vectors,
`db_incarnation`, `dirty_history`, `quarantine`, `rpo0`,
`divergence`, `history_integrity` and named `reason=` findings. Never cached;
no health marker is ever read.

Verdict (exit 0 eligible / 10 ineligible / 11 quarantined):

- Latch-class (permanent quarantine, exit 11): `Last_SQL_Errno != 0`,
  `gtid_strict_mode` off (a strict-mode violation), origin conflicts (unknown
  domain, wrong server_id for a domain, multi-origin `gtid_binlog_state`),
  interval gaps, `RESET MASTER`/binlog loss (applied position not covered by
  the binlog state when `log_slave_updates=ON`), or any prior latch. Once
  latched, no later evidence clears it. Open dirty-history records also block
  with exit 11, using a separate `dirty.pending` marker that a verified
  closure can remove.
- Ineligible-class (exit 10, recoverable without operator action): transient
  IO trouble (`Slave_IO_Running != Yes` or `Last_IO_Errno != 0` with a clean
  SQL thread - healing must return to eligible with no clear), SQL thread not
  running with no error, `log_bin`/`log_slave_updates` off (`rpo0=unavailable`
  is marked), `binlog_format` not ROW, `slave_skip_errors` set, replication
  filters set, GTID replication not in use, relay backlog
  (`Gtid_IO_Pos` not covered by `gtid_slave_pos`), missing required domain in
  the binlog state.

## GTID vectors

POSIX awk only (tested under mawk 1.3.4; no gawk extensions, no `strtonum`).
Grammar: comma-separated `d-s-n` / `d-s-n1-n2` elements, `:`-joined interval
chains; canonical decimal numbers only (no leading zeros). Bounds:
domain `0..4294967295`, server_id `1..4294967295`,
sequence `1..18446744073709551615`, checked digit-length first and then
lexicographically - never floats, never whole-string comparison.

Duplicate rules by kind:

- `position` (`gtid_binlog_pos`, `gtid_slave_pos`, `Gtid_IO_Pos`): duplicate
  domains are a parse error (13).
- `state` (`gtid_binlog_state`): keyed by `(domain_id, server_id)`; several
  server_ids in one domain are legitimate structure and surface as QUARANTINE
  evidence (origin conflict, exit 11) at evaluation; a duplicated
  `(domain_id, server_id)` pair is a parse error (13).

`covers(local, target)` is origin-aware frontier coverage over
`(domain, server_id, seq)` tuples: every target range needs a local range with
the same domain *and* server_id reaching at least as far. Extra local
origins never break coverage; a wrong origin never covers. Interval chains
with holes emit gap evidence.

## Quarantine and dirty history

`STATE_DIR` holds `quarantine.record` (written atomically: temp + fsync +
rename), `quarantine.cleared.<timestamp>` (rotated records), `quarantine.audit`
(append-only), `dirty.<id>.record` (open dirty-history records) and
`receipt.op_seq` (receipt consumption state).

The latch is PERMANENT until

```
gruezi-mariadb-adapter quarantine-clear --i-have-reconciled-history --operator <id>
```

which rotates the record and writes an audit line naming the operator; any
other argument shape is usage error 64. There is no automatic clear path.

Dirty-history records are fsynced before enabling writes. They survive
Gruezi-only restarts, DB restarts, baseline and revoke. A verified
`handoff_release` closes only records belonging to the current Gruezi
instance and exact local DB process incarnation. Other records remain
unresolved and block activation, including operator override. A successfully
closed epoch removes its pending marker; it never clears `quarantine.record`.

## DB incarnation

`db_incarnation = <pid>-<starttime>` derived from the pid file named by
`@@global.pid_file` and `/proc/<pid>/stat` field 22 (start time). Stable
within one server process, different after a restart. If the exact identity is
unavailable the value is literally `UNKNOWN` - there is no `now - Uptime`
approximation. Database incarnation, Gruezi instance id and operation
generation are three separate identities and are reported separately.

## Lifecycle environment and ordering

Lifecycle commands require `GRUEZI_EVENT` matching the command, nonzero u64
`GRUEZI_GENERATION`, 32 lowercase hex `GRUEZI_INSTANCE_ID` (the wire boot nonce),
u64 `GRUEZI_TRANSFER_ID`, `GRUEZI_PEER_NONCE` (32 lowercase hex or empty),
`GRUEZI_AUTHORITY_SOURCE` (`none|peer|assumed|external`),
u64 `GRUEZI_AUTHORITY_TERM`, and positive `GRUEZI_TIMEOUT_MS` (at most one day).
Missing/malformed values exit 64. Authority/consent and watchdog validity are
supplied by the trusted Gruezi caller. Activation requires a non-`none`
source and nonzero term.

All commands share exclusive `flock`. Lock acquisition and SQL use the
remaining monotonic event budget, capped by `COMMAND_TIMEOUT_S`. Mutating
generations must strictly increase within one instance. The only exception is
the startup baseline `deactivate` (`GRUEZI_DEACTIVATE_MODE=baseline`), which
carries `GRUEZI_GENERATION=0` because no generation exists yet: it runs the full
baseline cleanup, is not recorded in the generation ledger, and never writes a
receipt. Generation 0 is refused with exit 64 for every other operation. Verify
accepts repeated calls at exactly the active generation.

`activate` additionally requires `GRUEZI_ACTIVATION_MODE=handoff|cold`, no
quarantine or unresolved dirty epoch, exact local process identity,
`read_only=1`, and the full eligibility check.

- Handoff requires nonzero transfer and peer nonce, peer read-only, both
  appliers stopped, event scheduler OFF, valid release receipt, live vector
  equality and SQL continuity. Wait only on the peer-origin frontier using
  bounded `MASTER_GTID_WAIT`, accepting exactly 0. Check full origin-aware
  coverage of binlog state and applied positions, then recheck the live peer.
  Fsync consumption of `(peer instance, predecessor generation)` before enable.
- Cold requires both nodes reachable and read-only, converged valid vectors,
  no prior local write-operation record, no release mirror, and no receipt
  on either server. The existing Gruezi bootstrap/consent gate remains required.
- Enable uses one bounded interactive connection. Record `CONNECTION_ID()`
  with instance, generation and local process incarnation; fsync the dirty
  epoch; enable; read back on a fresh connection. Failure attempts restriction,
  preserves the epoch and exits nonzero.

`deactivate` requires `GRUEZI_DEACTIVATE_MODE=baseline|handoff_release|revoke`.
Order: lock; kill obsolete actuator connections and observe absence;
set `read_only=1` and read back; kill application connections; prove zero
remaining sessions, zero INNODB_TRX and empty XA RECOVER; fresh readback.
It exempts the current query connection and system/replication threads, never
every session of the actuator user. Monitor sessions may be disconnected.

Only handoff_release then stops replication, verifies both threads No,
verifies event_scheduler OFF without changing it, captures vectors and SQL
continuity, publishes and mirrors the receipt, rereads it and live state,
then closes the matching dirty epoch. Failed publication invalidates the
receipt on a bounded best-effort basis and withholds success. Baseline resumes
stopped replication only without quarantine or unresolved dirty history; it
also serves as post-handoff standby resumption. Revoke never resumes automatically.

`verify` additionally requires `GRUEZI_DB_INCARNATION` from activation.
The executor reuses activate_command: invoking `activate` with
`GRUEZI_EVENT=verify` dispatches to this same read-only verify path.
It reads only local identity, read_only and pid-file/proc incarnation.
Exact active instance/generation/incarnation plus read_only=0 produces exactly
one `db_incarnation=` line and exit 0. Restriction exits 10; unknown/mismatched
identity exits 13; timeout exits 12. Verify never touches the peer or mutates
SQL, and proves liveness rather than release finality.

## Release carrier and privileges

Provision this local nonreplicated carrier at cluster initialization.
The adapter never executes DDL:

```sql
SET SESSION sql_log_bin=0;
CREATE DATABASE IF NOT EXISTS gruezi_ha;
CREATE TABLE IF NOT EXISTS gruezi_ha.handoff_release (
  singleton_id TINYINT UNSIGNED NOT NULL PRIMARY KEY,
  transfer_id BIGINT UNSIGNED NOT NULL,
  server_id INT UNSIGNED NOT NULL,
  gruezi_instance_id VARCHAR(64) NOT NULL,
  generation BIGINT UNSIGNED NOT NULL,
  db_incarnation VARCHAR(128) NOT NULL,
  target_gtid_binlog_pos VARCHAR(512) NOT NULL,
  target_gtid_current_pos VARCHAR(512) NOT NULL,
  target_gtid_slave_pos VARCHAR(512) NOT NULL,
  target_gtid_binlog_state VARCHAR(1024) NOT NULL,
  release_nonce CHAR(32) NOT NULL,
  record_digest CHAR(64) NOT NULL,
  released_at_utc VARCHAR(32) NOT NULL
) ENGINE=MEMORY DEFAULT CHARSET=ascii;
```

Exactly one row, singleton_id=1, is accepted. MEMORY deliberately empties the
carrier on DB restart. Generation serves as op_seq; predecessor and successor
counters are independent. The successor binds peer instance to the wire peer
nonce, transfer to the current wire transfer ID, and server to configuration,
then fsyncs the never-before-consumed `instance:generation` key in consumed.keys.

The carrier's db_incarnation is the SQL continuity tuple
`server_id:gtid_domain_id:uptime_s:binlog_state_sha256`. Live uptime must be
at least barrier uptime, identities must match and state digest remain equal.
Local operation records separately carry pid/proc identity.
target_gtid_binlog_state contains sorted canonical domain-server-frontier
tuples and drives coverage and origin checks. The three other target columns
remain live-equality samples. Slave position also participates in coverage.

SHA256 serialization is `key=value\n`, in this order: singleton_id,
transfer_id, server_id, gruezi_instance_id, generation, db_incarnation,
target_gtid_binlog_pos, target_gtid_current_pos, target_gtid_slave_pos,
target_gtid_binlog_state, release_nonce, released_at_utc. record_digest is
excluded. The nonce is random 32-hex. An atomic mirror lives in
STATE_DIR/last_release.record.

Local actuator: READ_ONLY ADMIN, CONNECTION ADMIN, REPLICA MONITOR, PROCESS,
REPLICATION SLAVE ADMIN, BINLOG ADMIN, and SELECT/INSERT/UPDATE/DELETE on the
carrier. Peer monitor: REPLICA MONITOR and SELECT on the carrier only.
Application users must have neither SUPER nor READ_ONLY ADMIN. Lab root is a
declared bypass. SQL permission failures always fail closed.

Runtime SQL scope: reads/SHOW, MASTER_GTID_WAIT, SET GLOBAL read_only,
KILL CONNECTION, STOP/START REPLICA, XA RECOVER, and receipt CRUD with
SET SESSION sql_log_bin=0. No global event-scheduler mutation, RESET,
FLUSH TABLES WITH READ LOCK or retry loop.

## One-shot operator authorization

```sh
gruezi-mariadb-adapter declare-authoritative \
  --i-accept-possible-data-loss --incident-id INC-19 \
  --reason "Peer network unavailable" --operator ops-42
```

This local administrative command uses the complete activation env binding.
It fsyncs audit and a pending authorization bound to instance, generation
and exact local process incarnation, expiring after 60 monotonic seconds.
Activation consumes it by rename and directory sync before enabling. Normal
planned handoff leaves it unconsumed. It bypasses only peer transport failure,
replication IO connectivity and the unavailable peer-target wait.
An observed writable peer, permission errors, authority, quarantine, dirty
history, SQL-thread errors, local restriction, identity, config validation
and readback remain mandatory. Failure after consumption requires new
explicit authorization and resolution of any open epoch.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | ok / eligible |
| 10 | ineligible (recoverable operational/config condition) |
| 11 | quarantined (latch active or newly latched) |
| 12 | timeout (bounded SQL exceeded its wall clock) |
| 13 | unknown or unparseable evidence (incl. identity/credential refusals) |
| 64 | usage error |

## RPO-0 markers

`rpo0=available` in check requires `log_bin=ON` and `log_slave_updates=ON`;
otherwise `rpo0=unavailable`. Any RPO-0 claim built on this adapter is scoped
to binlog-logged transactions and inherits the documented limits:
`sql_log_bin=0` writes, privileged writes, unlogged DDL, nontransactional
anomalies, restores/backups, external effects and corruption are outside every
guarantee. Activation prints verified/assumed evidence and conservatively
reports rpo=unknown, possible_data_loss=yes, divergence=unknown and
history_integrity=unknown. Cold/operator recovery also reports target=unknown.
The stronger planned-handoff RPO sentence requires real PO-1 through PO-6
laboratory evidence, which these fake-client tests do not supply. Equal GTIDs
cannot detect content divergence, forged GTIDs, unlogged/filesystem writes
or restore divergence.

## Test guard

Internal primitives (`internal-gtid-parse`, `internal-gtid-covers`,
`internal-peer-vectors`, `internal-dirty-open`, `internal-baseline-cleanup`,
`internal-receipt-verify`) are refused (exit 64) unless
`GRUEZI_ADAPTER_TEST=1`. They never change divergence or quarantine semantics.
The old standalone receipt parser remains internal-only; production uses
the fixed SQL carrier described above.
Integration coverage lives in `tests/mariadb_adapter.rs` with a scripted fake
`mariadb` client (`tests/mariadb_support/`) that logs every argv and stdin, so
read-only claims are proven from the call log.
