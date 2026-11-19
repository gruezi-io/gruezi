# MariaDB adapter foundation for Gruezi strict HA

`gruezi-mariadb-adapter` is a bash CLI that owns every MariaDB interaction for
Gruezi's strict 2-node HA mode. This directory contains the **foundation**
(Implementation status for the pieces below): bounded read-only SQL, checked
GTID vector parsing, the eligibility `check` report, the permanent quarantine
latch with dirty-history state, DB incarnation derivation, and the release
receipt parse/verify primitives. The write transitions (`activate` /
`deactivate`), the release barrier and the override flow are built on these
primitives by the next task and are **Planned**, not present here.

## Safety boundaries (read this first)

- **This is not write fencing.** Nothing here can stop a privileged or
  out-of-band writer. `read_only` enforcement, session termination and
  fencing strategy live elsewhere.
- **Not a trust root.** The quarantine latch is hazard memory: it only ever
  makes outcomes worse. No file this adapter reads can authorize anything.
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

`sql_local` / `sql_peer` are the only paths to the database:

```
timeout --signal=KILL COMMAND_TIMEOUT_S mariadb \
    --defaults-extra-file=<option-file> \
    --connect-timeout=CONNECT_TIMEOUT_S \
    --max-statement-time=STATEMENT_TIMEOUT_S \
    --batch --skip-column-names
```

SQL is passed on stdin; credentials live only in the option file, so no secret
ever appears on argv. A wall-clock timeout is exit 12; any other client
failure is exit 13; output is parsed strictly and any unparseable value is
exit 13. Never a guess, never a default.

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
- `gtid_current_pos` is never read and never a target.

Report: `key=value` lines including every verified variable, the vectors,
`db_incarnation`, `dirty_history`, `quarantine`, `rpo0`,
`divergence`, `history_integrity` and named `reason=` findings. Never cached;
no health marker is ever read.

Verdict (exit 0 eligible / 10 ineligible / 11 quarantined):

- Latch-class (permanent quarantine, exit 11): `Last_SQL_Errno != 0`,
  `gtid_strict_mode` off (a strict-mode violation), origin conflicts (unknown
  domain, wrong server_id for a domain, multi-origin `gtid_binlog_state`),
  interval gaps, `RESET MASTER`/binlog loss (applied position not covered by
  the binlog state when `log_slave_updates=ON`), open dirty-history records,
  or any prior latch. Once latched, no later evidence clears it - GTID
  equality least of all.
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

Dirty-history records (a write-enabling operation whose passive closure was
never verified) survive Gruezi-only restarts, DB restarts, deactivation and
baseline cleanup; only demonstrated peer coverage of the recorded target can
resolve them (task 8), and that never clears a quarantine latch.

## DB incarnation

`db_incarnation = <pid>-<starttime>` derived from the pid file named by
`@@global.pid_file` and `/proc/<pid>/stat` field 22 (start time). Stable
within one server process, different after a restart. If the exact identity is
unavailable the value is literally `UNKNOWN` - there is no `now - Uptime`
approximation. Database incarnation, Gruezi instance id and operation
generation are three separate identities and are reported separately.

## Receipt primitives (task 8 consumes; parse + verify only here)

Single-row receipt as literal `key=value` fields: `transfer_id`, `server_id`,
`gruezi_instance_id`, `generation`, `db_incarnation`, `target_binlog_state`,
`target_binlog_pos`, `target_slave_pos`, `release_nonce`, `op_seq`,
`record_digest`. `record_digest` is the sha256 over the canonical field tuple:
the ten fields above except `record_digest`, in that order, each serialized as
`key=value\n`. Verification recomputes the digest, binds all identity fields,
refuses `db_incarnation=UNKNOWN`, and enforces `op_seq` consumption in
`receipt.op_seq` keyed by `(gruezi_instance_id, db_incarnation)`:

- mutating mode (`--mode=mutate`, task 8 activate/deactivate): `op_seq` must be
  strictly greater than anything consumed - one-shot.
- read mode (`--mode=read`, default): a verification at the same `op_seq` is
  accepted when the receipt's `generation` is the recorded active operation
  generation - read-only verification is idempotent.

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

`rpo0=available` is reported only when `log_bin=ON` and `log_slave_updates=ON`;
otherwise `rpo0=unavailable`. Any RPO-0 claim built on this adapter is scoped
to binlog-logged transactions and inherits the documented limits:
`sql_log_bin=0` writes, privileged writes, unlogged DDL, restores, external
side effects and corruption are outside every guarantee.

## Test guard

Internal primitives (`internal-gtid-parse`, `internal-gtid-covers`,
`internal-peer-vectors`, `internal-dirty-open`, `internal-baseline-cleanup`,
`internal-receipt-verify`) are refused (exit 64) unless
`GRUEZI_ADAPTER_TEST=1`. They never change divergence or quarantine semantics.
Integration coverage lives in `tests/mariadb_adapter.rs` with a scripted fake
`mariadb` client (`tests/mariadb_support/`) that logs every argv and stdin, so
read-only claims are proven from the call log.
