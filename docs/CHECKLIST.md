# Checklist

This is the human tracking list for what Gruezi does and what is still missing.
It restores the checklist that the README carried before the November 2026
restructure, with each box brought up to date.

How to read it:

* `[x]` means **Implemented**: the behavior exists and the linked evidence backs
  it.
* `[ ]` means it is not complete. The label says why: **Experimental** (exists
  but its acceptance is partial), **Planned** (not started), or **Unsupported**.
* Each line points to the code, test, or document that supports it. Do not tick
  a box without adding that pointer.
* The [README](../README.md) table is the short summary. The
  [roadmap](ROADMAP.md) holds the milestones and their acceptance criteria. When
  a status changes, update this file and the README table together.

## HA

* [x] HA mode over unicast UDP at L4. **Implemented**, `9375/udp`. See the
  README table.
* [x] IPv6-first API listener with IPv4 fallback. **Implemented**, in
  `src/gruezi/net.rs`: an explicit address, else dual-stack `[::]`, else IPv4
  `0.0.0.0`.
* [x] HA packet format and authentication. **Implemented**: HMAC-SHA-256, peer
  pinning, sessions, and replay protection. See [HA safety](HA-SAFETY.md).
* [x] HA state machine (`INIT`, `STANDBY`, `ACTIVE`). **Implemented**, `HaState`
  in `src/gruezi/ha/election.rs`.
* [x] HA management API on `9376/tcp`. **Implemented** as read-only:
  `/status`, `/ha/status`, and `/health`.
* [x] `gruezi status` with live watch mode, and the packet troubleshooting
  workflow. **Implemented**. See the README troubleshooting section.
* [ ] `gruezi peers` listing. **Experimental**: the command exists, but peer
  discovery returns an empty list.
* [ ] Direct `gruezi start --bind/--peers/--node-id` flags. **Unsupported**: the
  flags are a stub. Use YAML only.
* [x] HA transition hooks (`on_promote`, `on_demote`, `on_backup`) and the fault
  hook (`on_fault`). **Implemented**.
* [x] VIP ownership confirmed by kernel read-back before `owned=true`.
  **Implemented**. See [roadmap M1](ROADMAP.md).
* [x] Graceful VIP cleanup on shutdown (`SIGINT`, `SIGTERM`). **Implemented**.
  Evidence is listed under [roadmap M1](ROADMAP.md).
* [x] Explicit failure policies: `strict`, `available`, and `external`.
  **Implemented**. See the failure matrix in [HA safety](HA-SAFETY.md).
* [x] Ansible-based HA lab deployment workflow. **Implemented**, in `ansible/`.
* [ ] Watchdog fencing of a stalled local owner. **Experimental**: the
  fake-device paths and one Rocky softdog helper with a real VM reset pass. More
  real-device acceptance is open. See [HA safety](HA-SAFETY.md).
* [ ] External grant authority. **Experimental**: only the two-node happy path,
  renewal, and stale-grant withdrawal are tested.
* [ ] Confirmed fencing under the `external` policy. **Planned**: a lone node
  stays unowned with fencing `requested`, never `confirmed`.
* [ ] Split-brain prevention and conservative failover rules. **Partial**:
  `strict` and `external` avoid overlap, `available` allows it by design, and
  there is no witness or confirmed fencing yet.
* [ ] Native netlink VIP management. **Planned**: the address code still takes
  `arping_command` and `ndsend_command` settings, so announcements use external
  tools. It should drop that runtime dependency, as keepalived does.
* [ ] Performance tuning for heartbeat, timers, and failover latency.
  **Planned**: no measurements are recorded.
* [ ] DNS-based service discovery. **Planned**, [roadmap M6](ROADMAP.md).

## KV

All items here are **Planned**. No Raft or key-value runtime is shipped. See
[roadmap M3 and M4](ROADMAP.md) and the [storage evaluation](RAFT-STORAGE.md).

* [ ] KV mode using Raft consensus (M3).
* [ ] `raft-engine` for the dedicated Raft log. Under evaluation, not decided.
* [ ] RocksDB for applied key-value state. Under evaluation, not decided.
* [ ] Snapshot creation and install flow.
* [ ] Aggressive Raft log truncation after snapshot.
* [ ] Snapshot lifecycle management.
* [ ] Membership change and bootstrap rules.
* [ ] Witness or arbiter support. Also relevant to two-node HA.
* [ ] Discovery bootstrap by URL.

## Operations

* [x] Tracing through OpenTelemetry. **Implemented**: set
  `OTEL_EXPORTER_OTLP_ENDPOINT` to export traces. See the README.
* [ ] Metrics. **Planned**: only traces are exported.
* [ ] Backpressure before disk exhaustion. **Planned**, measured under
  [roadmap M4](ROADMAP.md).
* [ ] Quotas and reserved free space. **Planned**, [roadmap M4](ROADMAP.md).
* [ ] Clear write-stall behavior under pressure. **Planned**,
  [roadmap M4](ROADMAP.md).
* [ ] Security model: mTLS and authentication. **Planned**: the `9376/tcp` API
  has no authentication or TLS. Telemetry export has its own TLS settings.
