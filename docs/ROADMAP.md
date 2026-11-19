# Roadmap

Each milestone below is complete only when its acceptance criteria can be
reproduced from a clean checkout and the recorded output is retained.

## M1: HA ownership correctness, Implemented

**Objective:** Separate election, authority, resource state, and status, then
report ownership only after kernel read-back confirms the optional resource.

**Acceptance criteria:**

* A stateful fake kernel test proves add, remove, duplicate, missing, partial,
  and tentative IPv6 cases. Evidence: `tests/fake_ip.rs` and
  `.omo/evidence/task-4-gruezi-ha-safety-roadmap.txt`.
* A delayed acquisition followed by demotion leaves the resource absent.
  Evidence: `ownership::tests::delayed_add_then_demote_ends_absent` in
  `.omo/evidence/task-6-gruezi-ha-safety-roadmap.txt`.
* Every shutdown path joins the resource worker and confirms absence, or reports
  a fault. Evidence: `hanging_fault_hook_does_not_exceed_join_deadline` in
  `.omo/evidence/task-7-gruezi-ha-safety-roadmap.txt`.
* `owned` is false unless authority and confirmed resource state are valid.
  Evidence: `status_api_reflects_live_ha_state` in
  `.omo/evidence/task-7-gruezi-ha-safety-roadmap.txt`.

## M2: HA safety policies and watchdog, Experimental

**Objective:** Implement strict, available, and external authority semantics,
authenticated peer sessions, replay protection, handoff, and Linux watchdog
operation.

**Acceptance criteria:**

* Deterministic policy tests cover the documented rows. Available process
  freeze is 3/3 PASS in
  `.omo/evidence/task-24-gruezi-ha-safety-roadmap.txt`; the three
  failed-withdrawal rows pass in
  `.omo/evidence/task-27-gruezi-ha-safety-roadmap.txt`; all ten external rows
  pass in `.omo/evidence/task-26-gruezi-ha-safety-roadmap.txt`. Two consecutive
  full 30/30 runs are recorded at
  `/home/ansible/t28/trace-head-full-1/run.out` and
  `/home/ansible/t28/trace-head-full-2/run.out`.
* Strict mode never promotes after arbitrary simulated peer silence.
  Evidence: `matrix_strict_active_crash`,
  `matrix_strict_full_partition` in
  `.omo/evidence/task-10-gruezi-ha-safety-roadmap.txt`.
* Available mode records `assumed` and demonstrates the documented partition
  outcome. Evidence: `matrix_available_full_partition` in
  `.omo/evidence/task-10-gruezi-ha-safety-roadmap.txt`.
* Required watchdog arm failure produces zero resource additions. Evidence:
  `matrix_strict_watchdog_arm_fails`,
  `matrix_available_watchdog_arm_fails`, and
  `matrix_external_watchdog_arm_fails` in
  `.omo/evidence/task-13-gruezi-ha-safety-roadmap.txt`.
* Fault injection proves a frozen owner stops feeding and the resource is
  withdrawn or the host resets. Evidence:
  `.omo/evidence/task-13-gruezi-ha-safety-roadmap.txt` and
  `.omo/evidence/task-22-gruezi-ha-safety-roadmap.txt`.

## M3: Three-node Raft, Planned

**Objective:** Add quorum-backed authority for a three-node cluster.

**Acceptance criteria:**

* Three nodes elect one leader after bootstrap and retain one leader through one
  member failure.
* A two-member partition cannot commit a write.
* A recovered member catches up from a snapshot or log without manual state
  editing.
* Leadership and authority are visible in status with term and member identity.

## M4: etcd v3 subset KV, Planned

**Objective:** Add a durable, quota-aware KV service with the narrow API surface
needed by the integration milestone.

**Acceptance criteria:**

* Range, put, delete-range, transaction, lease, keepalive, watch, auth, member
  list, and version behavior pass a checked protocol fixture.
* A benchmark records bytes reclaimed after compaction, p50 and p99 write
  latency under disk pressure, recovery time after an unclean stop, and quota
  behavior at 80%, 95%, and 100% of the configured quota.
* Recovery time, write latency, reclaimed bytes, and quota results are stored
  with hardware, filesystem, dataset, and command details.

## M5: Patroni etcd3 integration, Planned

**Objective:** Support the specific etcd3 calls Patroni actually uses, without
claiming general etcd compatibility.

**Acceptance criteria:**

* A pinned Patroni integration fixture completes lease, watch, transaction, and
  member discovery scenarios against the documented endpoints.
* Failure tests show lease expiry and watch reconnection behavior.
* Unsupported endpoints return a documented error and are not described as
  compatible.

## M6: DNS and agent, Planned

**Objective:** Project live KV records through DNS and provide a small node agent
for registration and health updates.

**Acceptance criteria:**

* A registered service produces correct A, AAAA, and SRV answers with bounded
  TTLs.
* Lease expiry removes an instance from answers within the measured TTL plus
  one renewal interval.
* The agent reconnects after a coordinator restart without duplicate records.
* DNS answers and API records agree in an end-to-end fixture.
