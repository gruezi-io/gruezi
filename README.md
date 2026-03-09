# gruezi

Service Discovery & Distributed Key-Value Store

## Roadmap

### HA

- [ ] HA mode over unicast UDP at L4
- [ ] IPv6 support
- [ ] CLI for peer management and status
- [ ] DNS-based service discovery
- [ ] HA packet format and authentication
- [ ] HA state machine (`INIT`, `BACKUP`, `MASTER`)
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
  preempt: true
  advert_interval_ms: 1000
  dead_factor: 3
  hold_down_ms: 3000
  jitter_ms: 100
  auth:
    mode: none

kv: {}
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

ha: {}
```

This keeps the external configuration explicit and simple while leaving room for richer internal validation and future expansion.

## DRAFT: Protocol Direction

### HA mode

`mode: ha` should use a high-availability protocol over unicast UDP at L4.

The goal is to preserve the operational model of VRRP/CARP best practices while avoiding a dependency on L2 multicast, gratuitous ARP, or other mechanisms commonly blocked by cloud providers.

This means:

* leader election and liveness detection happen over UDP
* the state machine should remain close to active/backup failover behavior
* priority, advertisement interval, preemption, and authentication should be first-class concepts

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
* in `mode: ha`, it is the management/status port if a remote API is enabled
* CLI commands such as `gruezi status` or future management commands should be able to target this API instead of talking directly to the HA or Raft peer ports

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
* HA state machine with `INIT`, `BACKUP`, and `MASTER`
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

## DRAFT: API And Service Discovery

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

Service discovery also needs clear rules:

* how records are stored in KV
* how DNS responses are generated
* TTL and expiration behavior
* health integration and stale record cleanup

## DRAFT: Security And Observability

Both modes should plan for production-grade safety and debugging.

Minimum areas to define:

* mTLS between nodes
* client authentication and authorization
* certificate rotation
* metrics for leadership, replication lag, snapshot size, disk usage, and write stalls
* tracing for elections, failover, and storage operations
