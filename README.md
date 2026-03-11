[![Test & Build](https://github.com/gruezi-io/gruezi/actions/workflows/build.yml/badge.svg)](https://github.com/gruezi-io/gruezi/actions/workflows/build.yml)
[![codecov](https://codecov.io/gh/gruezi-io/gruezi/graph/badge.svg)](https://codecov.io/gh/gruezi-io/gruezi)
[![Crates.io](https://img.shields.io/crates/v/gruezi.svg)](https://crates.io/crates/gruezi)
[![License](https://img.shields.io/crates/l/gruezi.svg)](LICENSE)

<p align="center">
  <img src="gruezi-title.svg" alt="Gruezi" width="420">
</p>

<p align="center">High Availability, Service Discovery & Distributed Key-Value Store</p>

## Roadmap

### HA

- [x] HA mode over unicast UDP at L4
- [x] IPv6-first API listener with IPv4 fallback
- [x] CLI for peer management and status
- [x] Live HA status watch mode and packet troubleshooting workflow
- [ ] DNS-based service discovery
- [x] HA packet format and authentication
- [x] HA state machine (`INIT`, `STANDBY`, `ACTIVE`)
- [x] HA management API on `9376/tcp`
- [x] HA transition hooks (`on_promote`, `on_demote`, `on_backup`)
- [x] HA fault hook (`on_fault`) for address-action and runtime failure paths
- [x] Graceful VIP cleanup on shutdown (`SIGINT`, `SIGTERM`)
- [x] Ansible-based HA lab deployment workflow
- [ ] Split-brain prevention and conservative failover rules
- [ ] Performance tuning for heartbeat, timers, and failover latency

### KV

- [ ] KV mode using Raft consensus
- [ ] `raft-engine` for the dedicated Raft log
- [ ] RocksDB for applied key-value state
- [ ] Snapshot creation and install flow
- [ ] Aggressive Raft log truncation after snapshot
- [ ] Snapshot lifecycle management
- [ ] Membership change and bootstrap rules
- [ ] Witness/arbiter support
- [ ] Discovery bootstrap by URL

### Operations

- [ ] Backpressure before disk exhaustion
- [ ] Quotas and reserved free space
- [ ] Clear write-stall behavior under pressure
- [ ] Security model (mTLS and auth)
- [ ] Observability (metrics and tracing)

## DRAFT: Configuration Model

Configuration should use YAML.

Default config lookup order:

* `--config /path/to/gruezi.yaml`
* `GRUEZI_CONFIG=/path/to/gruezi.yaml`
* `/etc/gruezi/gruezi.yaml`

Example configs for local testing live in:

* `examples/ha.yaml`
* `examples/kv.yaml`
* `ansible-playbook -i ansible/inventory/lab.yml ansible/deploy-ha-lab.yml` after creating local files from `ansible/inventory/lab.yml.example` and `ansible/group_vars/gruezi_ha_lab.yml.example`
* `ansible/README.md` for the HA lab deployment workflow
* `contrib/README.md` for package-oriented `.deb` and `.rpm` builds
* `CONTRIBUTING.md` for the development and pull request workflow

Draft default ports:

* `9375/udp` for `gruezi-ha`
* `9376/tcp` for `gruezi-api`
* `9377/tcp` for `gruezi-peer`

For v1, `gruezi` should expose a single top-level selector:

```yaml
mode: ha
```

or:

```yaml
mode: kv
```

Current meaning:

* `mode: ha`: keepalived-like high availability for a minimum 2-node deployment
* `mode: kv`: etcd-like distributed key-value store with quorum semantics

Deployment guidance:

* `mode: ha` for 2-node deployments
* `mode: kv` for 3+ quorum participants

In other words, when only 2 nodes are available, HA is the viable mode. Once a cluster has enough participants for Raft, consensus should become the source of truth for leadership and failover decisions.

The `mode` field is the user-facing operational choice. The underlying protocol or algorithm used to implement that mode is an internal detail.

Internally, the configuration can still be normalized into dedicated sections, but the user-facing interface should remain simple:

```yaml
mode: ha

node:
  id: node-a

ha:
  bind: 0.0.0.0:9375
  interface: eth0
  group_id: cluster-ha
  addresses:
    - 10.0.0.10/24
  peer: 10.0.0.2:7000
  protocol_version: 1
  priority: 100
  preempt: false
  advert_interval_ms: 1000
  dead_factor: 3
  hold_down_ms: 3000
  jitter_ms: 100
  auth:
    mode: none
```

```yaml
mode: kv

kv:
  role: voter
  listen_client: 0.0.0.0:9376
  listen_peer: 0.0.0.0:9377
  data_dir: /var/lib/gruezi
  initial_cluster:
    - node-a=http://10.0.0.1:2380
    - node-b=http://10.0.0.2:2380
    - witness=http://10.0.0.3:2380
```

This keeps the external configuration explicit and simple while leaving room for richer internal validation and future expansion.

## DRAFT: Protocol Direction

### HA mode

`mode: ha` should use a high-availability protocol over unicast UDP at L4.

Current implementation overview:

```mermaid
flowchart TD
    A["Node A<br/>gruezi start --config ..."] --> B["Bind HA socket<br/>9375/udp"]
    C["Node B<br/>gruezi start --config ..."] --> D["Bind HA socket<br/>9375/udp"]

    B --> E["Periodic HA advertisement<br/>protocol_version, state, priority,<br/>dead_factor, advert_interval, sequence,<br/>node_id, group_id, auth_tag"]
    D --> F["Periodic HA advertisement<br/>same packet shape"]

    E --> G["Peer receives UDP packet"]
    F --> H["Peer receives UDP packet"]

    G --> I["Validate packet<br/>version, group_id, auth_tag,<br/>not looped local node"]
    H --> J["Validate packet<br/>version, group_id, auth_tag,<br/>not looped local node"]

    I --> K["Update peer observation<br/>peer node_id, state, priority,<br/>last seen timestamp"]
    J --> L["Update peer observation<br/>peer node_id, state, priority,<br/>last seen timestamp"]

    K --> M["Recompute local state<br/>INIT | STANDBY | ACTIVE"]
    L --> N["Recompute local state<br/>INIT | STANDBY | ACTIVE"]

    M --> O{"State changed?"}
    N --> P{"State changed?"}

    O -- "to ACTIVE" --> Q["Add VIP addresses to interface<br/>run promote hook"]
    O -- "to STANDBY" --> R["Remove VIP addresses from interface<br/>run backup/demote hook"]
    P -- "to ACTIVE" --> S["Add VIP addresses to interface<br/>run promote hook"]
    P -- "to STANDBY" --> T["Remove VIP addresses from interface<br/>run backup/demote hook"]

    M --> U["Publish HA status snapshot<br/>9376/tcp API"]
    N --> V["Publish HA status snapshot<br/>9376/tcp API"]
```

In practice, each node continuously:

* sends HA advertisements to exactly one configured peer over `9375/udp`
* tracks the peer's last observed state, priority, and liveness deadline
* chooses `ACTIVE` or `STANDBY` based on peer health, priority, node ID tiebreak, and `preempt`
* adds or removes the configured VIP addresses on state transition
* exposes the current snapshot through the management API on `9376/tcp`

Current implementation walkthrough:

1. Startup:
   the node loads the HA runtime config, binds the UDP socket on `ha.bind`, parses the single configured peer, initializes the local state as `INIT`, and publishes an initial status snapshot.
2. Advertisement loop:
   each iteration recomputes local state, waits until the next advertisement deadline, then either sends one HA packet to the configured peer or handles one received packet from that peer.
3. Packet validation:
   received packets are accepted only when the magic bytes, protocol version, `group_id`, and auth tag match, and when the packet is not looped back from the local node ID.
4. Peer observation:
   after a valid packet, the node stores the peer node ID, peer state, peer priority, and the timestamp of when that packet was observed.
5. State choice:
   if the peer is considered alive, the node compares peer state, peer priority, local priority, local node ID, and `preempt` to decide between `ACTIVE` and `STANDBY`.
   if the peer is not alive, the node promotes itself after the startup follower deadline or keeps `ACTIVE` if it already held it.
6. VIP handling:
   transitions to `ACTIVE` add the configured VIP addresses to the interface and run `on_promote`.
   transitions to `STANDBY` remove the configured VIP addresses and run either `on_backup` or `on_demote`.
7. Fault handling:
   address add/remove failures trigger `on_fault`.
   fatal runtime send/receive failures also trigger shutdown cleanup and `on_fault`.
8. Shutdown:
   on graceful shutdown, including `SIGINT` and `SIGTERM`, the node removes its configured VIP addresses before the runtime exits and publishes a final `INIT` snapshot.

The goal is to preserve the operational model of VRRP/CARP best practices while avoiding a dependency on L2 multicast, gratuitous ARP, or other mechanisms commonly blocked by cloud providers.

This means:

* leader election and liveness detection happen over UDP
* the state machine should remain close to active/backup failover behavior
* priority, advertisement interval, preemption, and authentication should be first-class concepts

Operational scope for HA mode:

* `mode: ha` is an internal infrastructure component, similar in intent to keepalived
* HA advertisements on `9375/udp` are expected to stay on a trusted private network
* the HA peer channel should not be exposed to the public Internet
* firewall rules should restrict HA traffic to the expected peer nodes

This is intentionally **VRRP/CARP-like**, not wire-compatible VRRP or CARP. The project should not claim protocol compatibility unless it implements the actual protocol semantics and packet format.

The HA advertisement format should be versioned and minimal. At a minimum, each packet should carry:

* protocol version
* node ID
* group or instance identifier
* current state
* priority
* advertisement interval
* sequence number
* authentication tag

Performance and reliability should be first-class requirements for HA mode:

* low-overhead UDP heartbeats
* deterministic state transitions
* conservative failover under packet loss or partitions
* explicit authentication on HA advertisements
* predictable recovery after peer restart or transient network loss
* first-class IPv6 support with IPv4 fallback when dual-stack binding is unavailable

HA observability should also be a first-class design goal:

* every promotion, demotion, backup transition, and VIP move should be explainable after the fact
* operators should be able to tell whether the cause was peer timeout, priority/preempt logic, node ID tiebreak, shutdown cleanup, or an explicit fault path
* `gruezi status`, logs, hooks, and future metrics should make the decision path visible instead of only showing the final state
* debugging HA should answer "why did this node take or drop the VIP?" without requiring packet capture as the primary source of truth

### KV mode

`mode: kv` should use Raft for consensus.

The KV subsystem is intended to provide etcd-like semantics:

* quorum-based writes
* leader election through Raft
* replicated log and durable state
* membership-aware cluster status

Operationally, `mode: kv` requires at least 3 quorum participants, or 2 nodes plus a witness/arbiter.

### API and management port

`9376/tcp` should be the common API and management port.

That means:

* in `mode: kv`, it is the client-facing API port
* in `mode: ha`, it is the live management/status port
* CLI commands such as `gruezi status` already target this API instead of talking directly to the HA or Raft peer ports

The port split should remain:

* `9375/udp`: HA peer advertisements
* `9376/tcp`: API, management, and client access
* `9377/tcp`: KV peer and Raft traffic

### Separation of concerns

HA and KV solve different problems and should remain conceptually separate:

* HA decides which node should be active in a 2-node deployment
* KV uses Raft to decide leadership and maintain authoritative replicated state in a 3+ participant deployment

For v1, the user-facing configuration should stay explicit and simple with `mode: ha` or `mode: kv`. For 2 nodes, use HA. For 3 or more quorum participants, KV is the preferred mode because Raft already provides leadership and failover behavior through consensus.

## DRAFT: KV Validation Strategy

`mode: kv` should be validated with [Maelstrom](https://github.com/jepsen-io/maelstrom), the Jepsen-based distributed systems workbench used by the Fly.io distributed systems challenges.

This is useful because Maelstrom provides:

* a simulated network with latency, loss, and partitions
* workload-specific correctness checks
* visualization and history analysis for distributed failures

The Fly.io challenges should be treated as a staged validation path for KV work, not as a literal promise to implement every challenge unchanged.

Initial KV validation milestones:

* basic RPC/message handling
* node identity and request correlation
* inter-node message propagation
* replicated log behavior
* Raft leader election and quorum behavior

As `mode: kv` evolves, tests should move from local unit coverage to Maelstrom-driven fault-injection and correctness checks.

## DRAFT: Storage Direction

For `mode: kv`, storage should be split by workload:

* `raft-engine` as the dedicated Raft log engine for the consensus journal
* RocksDB for the applied key-value state
* snapshots managed separately from the live Raft log and KV state

The Raft log and the applied KV state are different things:

* the Raft log stores ordered replicated commands
* the KV state stores the result after committed commands are applied

This separation should make it easier to optimize for performance and resilience under disk pressure.

The intended operational goals are:

* fast sequential Raft appends and replay
* efficient log truncation after snapshotting
* durable and performant KV reads/writes through RocksDB
* better disk-pressure handling than a single shared backend

For v1, `gruezi` should use a single Raft group. `raft-engine` is still the preferred direction for the Raft log even though it is designed to support Multi-Raft, because the log engine characteristics are a better fit for consensus journals than a general-purpose KV backend.

To support that, the system should explicitly implement:

* aggressive Raft log truncation after snapshot
* snapshot lifecycle management
* backpressure before disk exhaustion
* quotas and reserved free space
* clear write-stall behavior under pressure

## DRAFT: Snapshot Model

Snapshots are required to keep the Raft log from growing without bound.

The expected model is:

* committed log entries are applied to RocksDB
* a snapshot captures the applied KV state at a specific Raft index and term
* once the snapshot is durable, older Raft log segments can be truncated

Each snapshot should include:

* last included Raft index
* last included Raft term
* cluster membership metadata
* snapshot format version
* checksum

Snapshots should be used for:

* faster node restart and recovery
* catching up lagging or newly joined followers
* bounding disk usage for the Raft log

Snapshot lifecycle should define:

* when snapshots are triggered
* how snapshots are transferred to other nodes
* when old snapshots can be deleted
* when Raft log truncation is allowed after snapshot persistence

## DRAFT: On-Disk Layout

`mode: kv` should separate persistent data by purpose:

* `raft/` for the Raft log engine
* `kv/` for RocksDB applied state
* `snapshots/` for snapshot files and metadata

This layout should allow independent quotas, cleanup policies, and recovery behavior.

## DRAFT: Membership And Bootstrap

Cluster lifecycle needs explicit rules.

Items that must be defined:

* first cluster bootstrap
* adding a new node
* replacing a failed node
* removing a node safely
* restart behavior after crash or partial disk loss
* witness or arbiter behavior, if supported

For v1, membership changes should be conservative and explicit. Unsafe ad-hoc joins should be avoided.

## DRAFT: Failure And Disk-Pressure Behavior

Disk pressure should be treated as a first-class failure mode.

Behavior to define:

* reserved free-space threshold
* warning threshold and critical threshold
* when writes are throttled
* when writes are rejected
* when compaction, truncation, or snapshot cleanup is triggered
* when the node reports degraded or read-only state

The goal is to fail predictably before the disk is fully exhausted.

## DRAFT: HA Failure Semantics

For `mode: ha`, split-brain prevention must be documented explicitly.

Items to define:

* promotion rules
* preemption rules
* peer loss timeout
* behavior under network partition
* fencing or external safety checks, if required

For a 2-node deployment, HA should prefer deterministic and conservative failover behavior over aggressive promotion.

## DRAFT: HA Implementation Priorities

The HA path should be built first as the smallest end-to-end feature.

Suggested order:

* YAML config schema and validation for `mode: ha`
* node identity, peer identity, and interface/address configuration
* UDP packet format with versioning and authentication fields
* heartbeat sender/receiver loop with bounded timers
* HA state machine with `INIT`, `STANDBY`, and `ACTIVE`
* promotion, preemption, and failover rules
* CLI status output and metrics

HA v1 should optimize for:

* strong reliability before fast failover
* bounded CPU and memory overhead
* clear behavior during packet loss, delay, or temporary partitions
* simple and observable state transitions for debugging

Recommended HA timer defaults:

* `advert_interval_ms: 1000`
* `dead_factor: 3`
* `hold_down_ms: 3000`
* `jitter_ms: 100`

Recommended HA auth shape:

```yaml
ha:
  group_id: cluster-ha
  auth:
    mode: shared_key
    key: change-me
```

Meaning of the HA fields:

* `group_id`: logical HA domain. Only nodes in the same group should accept each other's advertisements.
* `priority`: preferred active-node weight. Higher priority wins when both nodes are healthy. If priorities are equal, the higher `node.id` wins as the current tiebreak.
* `preempt`: whether a higher-priority node is allowed to take the VIP back after a lower-priority peer is already `ACTIVE`.
* `advert_interval_ms`: heartbeat send interval in milliseconds.
* `dead_factor`: multiplier used with `advert_interval_ms` to decide when a peer is considered dead.
* `hold_down_ms`: additional delay before promoting after peer loss, to avoid overly aggressive failover.
* `jitter_ms`: bounded per-packet send jitter applied below the advertisement interval to avoid perfectly synchronized heartbeats.
* `auth.mode: none`: disable packet authentication. This is only suitable for local development or isolated lab testing.
* `auth.mode: shared_key`: every HA packet carries an authentication tag derived from a shared secret and the packet contents.
* `auth.key`: the shared secret used by all nodes in the same HA group. It should be treated like any other cluster secret and distributed securely.

### HA Behavior By Configuration

For HA mode, these settings directly control who owns the VIP and whether it moves back after a failed node returns.

Default behavior:

* a node promotes itself after the peer is considered dead for `advert_interval_ms * dead_factor + hold_down_ms`
* if both nodes are healthy, the higher `priority` wins
* if both nodes are healthy and priorities are equal, the higher `node.id` wins
* with `preempt: true`, a returning higher-priority node takes the VIP back
* with `preempt: false`, the current healthy active node keeps the VIP even if a higher-priority node comes back

Recommended `gruezi` default:

* `preempt: false` for application VIPs

Rationale:

* keepalived/VRRP-style router behavior often defaults to preemption enabled
* application failover has a different cost model, because every VIP move can interrupt in-flight connections or force neighbor-cache convergence
* in practice, a second VIP move during recovery is often less desirable than keeping the already-healthy active node in place

This means the current implementation behaves like this:

1. Node A is `ACTIVE`, Node B is `STANDBY`.
2. Node A fails.
3. Node B promotes and takes the VIP.
4. Node A comes back.
5. If `preempt: true`, Node A retakes the VIP if it outranks Node B.
6. If `preempt: false`, Node B keeps the VIP while healthy and Node A stays `STANDBY`.

If you want keepalived-style `nopreempt` behavior, use:

```yaml
ha:
  priority: 110
  preempt: false
```

That is the right setting when the operator goal is:

* fail over quickly after peer loss
* do not move the VIP again just because the old preferred node came back
* keep the recovered node passive until the current active node fails or is stopped

If you want the preferred node to reclaim the VIP after recovery, use:

```yaml
ha:
  priority: 110
  preempt: true
```

That is useful when:

* one node is intentionally preferred for operational reasons
* returning to the preferred active node is more important than avoiding a second VIP move

Current test coverage includes:

* initial election by priority
* takeover after peer loss
* higher-priority startup preemption
* higher-priority startup without preemption
* returning preferred node reclaiming the VIP with `preempt: true`
* returning preferred node staying `STANDBY` with `preempt: false`

### HA Validation Checklist

The goal is not wire-compatible VRRP/CARP certification. The goal is VRRP/CARP-like behavioral validation for the operational cases that matter.

Recommended checks:

* election by priority on cold start
* deterministic tie-break when priorities are equal
* no unnecessary failback when `preempt: false`
* explicit failback when `preempt: true` during clean recovery or join
* takeover only after `advert_interval_ms * dead_factor + hold_down_ms`
* no early promotion before the configured timeout window
* graceful shutdown removes the VIP and lets the peer take over cleanly
* invalid HA packets are ignored without crashing the runtime
* mismatched `group_id` is rejected
* mismatched auth key is rejected
* duplicate `node.id` is treated as an invalid peer condition
* hooks run once per transition with the expected state context
* VIP add/remove commands run once per transition
* `/status` reflects the actual runtime state during election, failover, and recovery
* packet loss or partition scenarios remain conservative and explainable

Current integration coverage already includes:

* priority-based election and failover
* equal-priority `node.id` tie-break behavior
* duplicate `node.id` isolation behavior
* preempt and no-preempt behavior
* returning-node reclaim versus no reclaim
* live `group_id` mismatch isolation
* live auth-mismatch isolation
* VIP add/remove side effects
* promote, demote, backup, and fault hooks
* live `/status` API reflection of runtime state

Current unit coverage also includes:

* timer-window assertions across multiple `advert_interval_ms`, `dead_factor`, and `hold_down_ms` combinations
* asymmetric-loss conflict suppression when a higher-priority node sees a lower-priority peer newly become `Active`

Still useful to add or expand:

* automated side-by-side comparison against keepalived for the same operator intent, while remaining explicit that `gruezi` is not wire-compatible VRRP

Linux namespace and `tc netem` lab coverage is now available through:

```bash
sudo ./scripts/test-ha-netem.sh all
```

### Keepalived Comparison

`gruezi` should be compared to keepalived at the behavior level, not the packet level.

What should be the same:

* priority decides the preferred owner
* `preempt: false` behaves like keepalived `nopreempt`
* `preempt: true` allows the preferred node to reclaim ownership during clean join or recovery
* takeover is driven by advertisement loss and timeout windows
* a returning preferred node should not reclaim the VIP when preemption is disabled

What is intentionally different:

* `gruezi` is not wire-compatible VRRP and does not exchange VRRP advertisements
* `gruezi` uses unicast UDP on `9375/udp`, not VRRP multicast or VRID semantics
* `gruezi` exposes operator-facing decision reasons through `/status`, logs, and hooks
* `gruezi` now suppresses repeated reclaim during asymmetric one-way loss by using `peer_became_active_conflict`

Suggested config mapping when comparing the two:

* `priority` in `gruezi` maps to `priority` in keepalived
* `preempt: false` in `gruezi` maps to `nopreempt` in keepalived
* `advert_interval_ms: 1000` in `gruezi` maps to `advert_int 1` in keepalived
* `dead_factor * advert_interval_ms + hold_down_ms` in `gruezi` is the effective takeover window to compare against keepalived failover timing
* `ha.addresses` in `gruezi` maps to `virtual_ipaddress` in keepalived

Recommended comparison scenarios:

* cold start with different priorities
* cold start with equal priorities and deterministic tie-break
* active node stop or crash
* returning preferred node with preemption enabled
* returning preferred node with preemption disabled
* one-way packet loss on the preferred node
* full bidirectional partition

Recommended comparison method:

1. run the same two-node topology twice: once with `gruezi`, once with keepalived
2. keep priorities, VIP, interface, and nominal advertisement interval aligned
3. record for each run:
   * initial owner
   * takeover time
   * failback or no-failback behavior
   * whether the pair converges to one owner under one-way loss
   * whether full partition can create dual-active
4. compare operator outcome rather than packet contents

What to expect today:

* clean election, failover, and `nopreempt`-style behavior should align closely
* `gruezi` will differ intentionally under asymmetric one-way loss, because it now prefers converging to one owner without a second VIP move
* full-partition dual-active remains possible in both 2-node designs unless quorum or fencing exists

This comparison is meant to validate operator intent, not to claim protocol compatibility.

`shared_key` in HA mode is not transport encryption. It exists to answer a narrower question:

* is this UDP advertisement from a node that knows the group secret?
* was the packet likely modified in transit?

This is a better fit for HA v1 because the HA control plane is unicast UDP. `mTLS` is a strong option for TCP-based APIs and Raft peer links, but it does not apply directly to raw UDP advertisements. The comparable UDP-level option would be `DTLS` or a more advanced per-packet cryptographic scheme, which adds more complexity than is needed for the initial HA protocol.

Recommended direction:

* HA over UDP: start with explicit packet authentication using `shared_key`
* API and KV peer traffic over TCP: use TLS/mTLS
* future HA hardening: consider DTLS or stronger keyed message authentication if the simpler HA packet auth is not sufficient

Threat-model note:

* `shared_key` is a practical first step for a private HA network, not a full Internet-facing security model
* it should be combined with network isolation, peer allow-listing, and standard infrastructure firewalling
* if HA traffic ever needs to cross less-trusted networks, the design should be revisited with stronger transport or packet-level protections

Operational guidance:

* use `auth.mode: none` only for tests and local experiments
* use a different `auth.key` per HA group/environment
* rotate the key carefully, because all HA peers in the same group must agree on it
* do not treat `shared_key` as a substitute for TLS on the management API

### Current HA API

The current HA management API is read-only and listens on `9376/tcp`.

Available endpoints:

* `GET /status`
* `GET /ha/status`
* `GET /health`

The current `gruezi status` command queries this API.

The HA status payload and `gruezi status` output now also expose the operator-facing reason fields used during failover analysis:

* `decision_reason`: why the node currently prefers its present HA state
* `last_transition_reason`: why the most recent HA state change happened
* `last_transition_ms_ago`: how long ago the last HA state change occurred
* `duplicate_node_id_packets`: how many HA packets were rejected because the peer used the same `node.id`

Typical reason values include:

* `startup_hold`
* `startup_deadline_expired`
* `peer_timeout`
* `peer_active_no_preempt`
* `local_higher_priority`
* `preempt_higher_priority`
* `local_node_id_tiebreak`
* `peer_node_id_tiebreak`

Asymmetric-loss caveat:

* one-way loss is not the same as a clean fail-stop
* with `preempt: false`, the lower-priority peer can promote and keep the VIP while the higher-priority node demotes after it sees the peer as `Active`
* with `preempt: true`, `gruezi` now treats a newly promoted lower-priority peer as an active-peer conflict and demotes the higher-priority node to avoid persistent dual-active ownership
* that same conflict stays sticky while the peer remains a live `Active` owner, so the recovered preferred node does not immediately reclaim the VIP after the one-way loss heals
* a full bidirectional partition or a receive-side isolation can still produce dual-active, because the node cannot see the peer's promotion at all

That means one-way packet loss is now handled conservatively: ownership can move to the lower-priority peer, but the pair converges to one owner instead of repeatedly preempting.

Duplicate `node.id` caveat:

* `node.id` values must be unique within an HA pair
* if a node receives HA packets with its own `node.id`, it treats them as an invalid peer condition
* those packets are counted as invalid duplicate-node-id packets and are ignored
* because the peer is never accepted as healthy, both nodes can eventually promote after the startup timeout if they are misconfigured with the same `node.id`

For a live view during failover testing:

```bash
gruezi status --watch --interval-ms 1000 --node 192.0.2.5:9376
```

### HA Packet Troubleshooting

For HA packet troubleshooting, use `gruezi status --watch` and `tcpdump` together:

```bash
gruezi status --watch --interval-ms 1000 --node 192.0.2.10:9376
```

```bash
sudo tcpdump -ni eth0 'udp port 9375 and host 192.0.2.11' -tttt -vvv -X -s0
```

Recommended `tcpdump` flags:

* `-n`: disable DNS lookups
* `-tttt`: print readable timestamps for correlation with `status --watch`
* `-vvv`: increase protocol detail
* `-X`: show packet payload in hex and ASCII
* `-s0`: capture the full packet instead of truncating it

This lets you correlate:

* `sent` and `recv` counters from `gruezi status --watch`
* peer liveness and `last_peer_seen`
* raw UDP payload bytes on `9375/udp`

### HA Netem Lab

For Linux-only failure-injection testing, the repository includes a namespace-based lab runner:

```bash
cargo build --release --locked
sudo ./scripts/test-ha-netem.sh all
```

Why this is a script and not a normal Cargo test:

* it needs root privileges
* it creates and destroys Linux network namespaces
* it uses `tc netem` to inject delay and loss on the HA link
* those operations are not stable or appropriate inside `cargo test`

Prerequisites:

* Linux
* `ip`, `tc`, and `curl`
* a built release binary at `target/release/gruezi`

The script writes generated configs and logs under `/tmp/gruezi-netem-lab` by default.

Available scenarios:

* `baseline`: verify the higher-priority node becomes `Active` and the peer stays `Standby`
* `delay-jitter`: add bounded delay and jitter that should stay below the failover threshold and confirm ownership does not move
* `one-way-loss`: drop node A to node B traffic only and observe the asymmetric failover behavior with `preempt: false`
* `full-partition`: drop all traffic in both directions and observe that both nodes eventually become `Active`
* `heal-after-partition`: restore a full partition and verify the pair converges back to one `Active` and one `Standby`

Examples:

```bash
sudo ./scripts/test-ha-netem.sh baseline
sudo ./scripts/test-ha-netem.sh one-way-loss
sudo ./scripts/test-ha-netem.sh full-partition
sudo ./scripts/test-ha-netem.sh heal-after-partition
```

What each scenario validates:

* `baseline`: election by priority works
* `delay-jitter`: moderate latency by itself should not trigger failover
* `one-way-loss`: with the current script's `preempt: false`, asymmetric visibility moves ownership to the lower-priority node without causing a second failback during the impairment window
* `full-partition`: a 2-node deployment without quorum or fencing cannot distinguish peer failure from total isolation, so both nodes can become `Active`
* `heal-after-partition`: once packets flow again, the pair should converge back to a single owner

The repository's live Ansible lab also validated the `preempt: true` case:

* the lab pair started as higher-priority `Active` and lower-priority `Standby`
* one-way HA packet loss from the preferred node caused the lower-priority peer to promote on timeout
* once the preferred node saw the peer's new `Active` advertisements, it demoted with `decision_reason=peer_became_active_conflict`
* after the impairment healed, the pair stayed converged as lower-priority `Active` and higher-priority `Standby` to avoid a second VIP move

The `full-partition` result is not a test failure in this lab. It documents the current safety boundary of the implementation:

* `gruezi` does not have quorum in `mode: ha`
* `gruezi` does not perform fencing
* a total partition can therefore create dual-active ownership until connectivity is restored

That behavior should be understood and accepted before using 2-node HA for workloads that cannot tolerate split brain.

### Current HA Hooks

The current HA implementation supports transition hooks in YAML:

```yaml
ha:
  hooks:
    on_promote: /etc/gruezi/hooks/promote.sh
    on_demote: /etc/gruezi/hooks/demote.sh
    on_backup: /etc/gruezi/hooks/backup.sh
    on_fault: /etc/gruezi/hooks/fault.sh
    timeout_ms: 5000
```

Implemented today:

* `on_promote`
* `on_demote`
* `on_backup`
* `on_fault` for explicit HA address-action and runtime failure paths

Hook scripts currently receive runtime context through environment variables:

* `GRUEZI_EVENT`
* `GRUEZI_NODE_ID`
* `GRUEZI_GROUP_ID`
* `GRUEZI_INTERFACE`
* `GRUEZI_REASON`
* `GRUEZI_PRIORITY`
* `GRUEZI_STATE`
* `GRUEZI_PREVIOUS_STATE`
* `GRUEZI_PEER_ID`
* `GRUEZI_PEER_STATE`
* `GRUEZI_PEER_PRIORITY`
* `GRUEZI_LAST_PEER_SEEN_MS`

`GRUEZI_REASON` is the important new field for post-failover analysis. It lets a hook distinguish between:

* election decisions such as `LOCAL_HIGHER_PRIORITY` or `PEER_TIMEOUT`
* steady-state behavior such as `PEER_ACTIVE_NO_PREEMPT`
* side-effect failures such as `ADDRESS_ACTION_FAILED`

## DRAFT: API Surface

The external API surface for `mode: kv` still needs to be defined.

Questions to settle:

* etcd-compatible API or custom API
* gRPC, HTTP, or both
* key-space layout and prefix conventions
* watch/stream semantics
* lease/session behavior

API listeners must not be IPv4-only by default. The preferred behavior is:

* explicit `listen` IP binds exactly that IP family
* if no listen IP is provided, try dual-stack IPv6 first
* if dual-stack IPv6 is unavailable, fall back to IPv4

If an HTTP API is added, `axum` is a reasonable choice on top of a pre-bound `TcpListener`.

## DRAFT: Service Discovery

Service discovery should be a first-class feature, not just a byproduct of the KV layer.

Preferred direction:

* `mode: kv` is the authoritative source of service records
* `mode: ha` is responsible for VIP ownership and failover, not cluster-wide service registration
* service discovery should be consumable through both an API and a DNS-oriented interface

The initial discovery model should support:

* service registration with a service name and instance ID
* one or more addresses per instance
* port and protocol metadata
* optional labels or tags for filtering
* TTL or lease-backed liveness
* explicit deregistration

A reasonable KV layout would be:

```text
/services/<service>/<instance-id>
```

Where each record stores fields such as:

* service name
* instance ID
* node ID
* address list
* port map
* protocol
* labels or tags
* lease ID or expiration timestamp
* last health update

Discovery behavior also needs clear rules:

* how records are created and renewed
* when records expire after missed renewals
* whether failed health checks suppress DNS answers immediately or only after lease expiry
* how stale records are garbage-collected after node crash or partition
* whether clients can watch discovery changes over the API

DNS-based service discovery should project KV state into standard record types:

* `A` and `AAAA` for instance addresses
* `SRV` for named service endpoints and ports
* `TXT` only for small metadata when explicitly useful

The DNS view should be conservative:

* only return live records backed by an active lease or healthy session
* avoid serving stale endpoints after known failure
* keep TTLs short enough for fast failover, but not so short that clients or resolvers are forced into constant churn

This should answer two different operator needs:

* stable service naming for clients
* dynamic endpoint updates as instances move, restart, fail, or recover

## DRAFT: Security And Observability

Both modes should plan for production-grade safety and debugging.

Minimum areas to define:

* mTLS between nodes
* client authentication and authorization
* certificate rotation
* metrics for leadership, replication lag, snapshot size, disk usage, and write stalls
* tracing for elections, failover, and storage operations
