# MariaDB strict HA: investigation record and lessons

Status: Closed, November 21, 2026. The decision is
[docs/decisions/0001-ha-coordination-not-application-consistency.md](decisions/0001-ha-coordination-not-application-consistency.md).
This document preserves findings. It is not a readiness statement, and it
promotes no test result.

## 1. Original objective

Prove that a two-node Gruezi and MariaDB pair performs safe, automatic
failover under a defined fault matrix: one writer at a time, no divergence, and
measured recovery, with independent observations and three passing repetitions
per scenario.

## 2. What was implemented and tested

Implemented: the strict MariaDB profile, a lifecycle adapter that is the only
component permitted to change `read_only`, origin-aware GTID evaluation,
quarantine and dirty-history records, an 18-row acceptance matrix with a
disposable Podman lab, a guarded opt-in VM fault runner, a native systemd lab
backend, and an offline wire parser. An F-06 recovery design and a fencing
feasibility assessment were written and independently reviewed.

Tested: the matrix was executed in the Podman lab, and a native two-node pair
was provisioned, configured, and observed holding a single-writer baseline.

## 3. Confirmed findings

Proven by observation:

* A planned switchover and recovery after an unexpected failure are different
  problems. The first was demonstrated; the second was not.
* VIP ownership is not database write authority. A client can reach a node
  directly, and a privileged account bypasses `read_only`.
* `read_only` and lifecycle hooks are not fencing. They gate cooperative
  writers only.
* Process termination, delayed SQL effects, residual VIPs, and restarts are
  distinct safety concerns with distinct evidence requirements. A controller
  that has lost its instance can still have SQL in flight.
* Peer silence does not establish that a previous owner stopped acting.

Mathematical limitations, independent of implementation quality:

* Two-node asynchronous replication cannot guarantee lossless automatic
  failover under arbitrary failures.
* GTIDs and lag indicators cannot universally establish complete data
  consistency; they bound what is known, not what was lost.
* A partition can produce divergent transaction histories.
* Two-node coordination cannot deliver automatic partition tolerance and
  exclusive ownership together without an additional assumption or an external
  authority.
* Adding Raft will improve coordination guarantees. It will not solve MariaDB
  replication semantics and will not enforce application fencing.

These limits are specific to this profile and this topology. They are not a
claim that database HA is impossible in general.

## 4. Unresolved safety limitations

* No proven automatic recovery for a node carrying unresolved dirty history.
* No enforcement that a fenced former writer cannot resume writes; the F-06
  design identifies guarantees that require external authority.
* `strict` mode's exclusive-safety claim remains dependent on fencing
  assumptions that were never satisfied in the lab.

## 5. Explicitly untested scenarios

These retain their real status and must not be read as passing:

* All five VM fault cases of task 16, including watchdog reset: NOT RUN.
* Packet observation rows 9 to 11: NOT RUN. Row 8 is FAIL, from a denied
  packet socket (`Operation not permitted`), not from product behaviour.
* The proposed native destructive suite: NOT RUN and not authorized.
* The node-B restore rehearsal: NOT RUN. Recovery is documented, never
  executed, and no idempotence is claimed.
* Native acceptance on MariaDB 11.8: the verified single-writer baseline was
  observed on 10.11. The pair was later aligned to 11.8.10 and deliberately
  left unconfigured, so no native baseline exists on the current version.

Matrix status at closure: 18 rows, 54 repetition slots, 22 scoped PASS, 4 FAIL,
28 NOT RUN.

## 6. Retained evidence

Artifacts live under `.omo/evidence/gruezi-mariadb-strict-ha/` in the working
checkout, with an append-only decision ledger at `.omo/ulw-execute/ledger.jsonl`.

**Durability limitation:** `.omo/` is excluded from version control through
`.git/info/exclude`, so none of it is committed or pushed. It exists only in
the local checkout at `/home/devops/projects/gruezi/gruezi` and is lost if that
working copy is lost. Copy it out before discarding the machine.

Entry points, all relative to `.omo/evidence/gruezi-mariadb-strict-ha/`:

* `resume-checkpoint.md` - session handoff and current holds.
* `native-execution-amendment.md` - why execution moved from Podman to VMs.
* `native-pair-strict-baseline.md` - the single-writer native baseline on 10.11.
* `native-pair-mariadb-118-alignment.md` - the 11.8.10 alignment.
* `native-pair-predestructive-record.md` - identities, storage, isolation,
  recovery status.
* `destructive-suite-authorization-request.md` - the unapproved suite, marked
  superseded.
* `adapter-exit11-epoch-verdict.md` - open-epoch semantics of `check` exit 11.
* `resume-row16-proof-model/f06-recovery-design/` - the accepted F-06 design
  and its independent review.
* `resume-task19-prep/` - matrix attribution and live-acceptance prerequisites.

## 7. Why we stopped

The investigation answered the architectural question. The outstanding rows
need enforcement authority outside Gruezi's model, so further effort would buy
preparation rather than guarantees, while the generic coordinator still needs
to be delivered.

## 8. Conditions for reopening

Reopen when at least one holds:

* An external fencing or arbiter authority exists that Gruezi can require and
  verify, making an exclusive-ownership claim checkable rather than assumed.
* Three-node consensus has landed and the question becomes coordination with a
  real quorum instead of two-node ownership.
* A synchronous or externally arbitrated replication topology replaces two-node
  asynchronous replication.
* The operator accepts explicitly documented data-loss bounds for automatic
  failover, in which case the matrix thresholds must be restated, not relaxed.

## 9. Resumption point

* Branch and commit: `sandbox` at `1427f069c6396dbedaa2f41f94f6f942f409255a`.
  The work branch `work/gruezi-mariadb-strict-ha-resume-wave-8` sits on the same
  commit in a separate worktree.
* Implemented and verified in Gruezi itself: ownership correctness (M1), and the
  Experimental policy, watchdog, and authenticated peer work of M2, with their
  recorded evidence.
* Last verified test results: `cargo fmt`, `clippy`, and 504 passing tests on
  the Wave 8 source, with release binary
  `a5c3f613bf94df1a1d3cf938d1f89a11196ea4c7ac32fdd532e8c01ff088b255`.
* Outstanding issues in Gruezi itself: none identified as a product defect by
  this investigation. The unresolved items are unproven application-level
  guarantees, not coordinator bugs.
* Deferred MariaDB-only findings: F-06 recovery, dirty-history reconciliation,
  and database fencing, all design-only.
* Uncommitted lab state: the wave-8 worktree holds modified and untracked
  `scripts/mariadb-ha-lab/` files, including a partially tested node-B recovery
  path and precondition work whose suites last exited non-zero. That work is
  unfinished and uncommitted by intent; it is lab tooling, not product code.
* Smallest useful next task: finish M2's documented failure guarantees for the
  generic coordinator, so each policy's observable promise and its limits are
  stated and tested without any application-specific dependency.
