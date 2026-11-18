# Roadmap

Each milestone below is complete only when its acceptance criteria can be
reproduced from a clean checkout and the recorded output is retained.

## M1: HA ownership correctness

**Objective:** Separate election, authority, resource state, and status, then
report ownership only after kernel read-back confirms the optional resource.

**Acceptance criteria:**

* A stateful fake kernel test proves add, remove, duplicate, missing, partial,
  and tentative IPv6 cases.
* A delayed acquisition followed by demotion leaves the resource absent.
* Every shutdown path joins the resource worker and confirms absence, or reports
  a fault.
* `owned` is false unless authority and confirmed resource state are valid.

## M2: HA safety policies and watchdog

**Objective:** Implement strict, available, and external authority semantics,
authenticated peer sessions, replay protection, handoff, and Linux watchdog
operation.

**Acceptance criteria:**

* A deterministic matrix test covers every row in [HA safety](HA-SAFETY.md).
* Strict mode never promotes after arbitrary simulated peer silence.
* Available mode records `assumed` and demonstrates the documented partition
  outcome.
* Required watchdog arm failure produces zero resource additions.
* Fault injection proves a frozen owner stops feeding and the resource is
  withdrawn or the host resets.

## M3: Three-node Raft

**Objective:** Add quorum-backed authority for a three-node cluster.

**Acceptance criteria:**

* Three nodes elect one leader after bootstrap and retain one leader through one
  member failure.
* A two-member partition cannot commit a write.
* A recovered member catches up from a snapshot or log without manual state
  editing.
* Leadership and authority are visible in status with term and member identity.

## M4: etcd v3 subset KV

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

## M5: Patroni etcd3 integration

**Objective:** Support the specific etcd3 calls Patroni actually uses, without
claiming general etcd compatibility.

**Acceptance criteria:**

* A pinned Patroni integration fixture completes lease, watch, transaction, and
  member discovery scenarios against the documented endpoints.
* Failure tests show lease expiry and watch reconnection behavior.
* Unsupported endpoints return a documented error and are not described as
  compatible.

## M6: DNS and agent

**Objective:** Project live KV records through DNS and provide a small node agent
for registration and health updates.

**Acceptance criteria:**

* A registered service produces correct A, AAAA, and SRV answers with bounded
  TTLs.
* Lease expiry removes an instance from answers within the measured TTL plus
  one renewal interval.
* The agent reconnects after a coordinator restart without duplicate records.
* DNS answers and API records agree in an end-to-end fixture.
