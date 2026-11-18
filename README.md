# Gruezi

Gruezi is a small, Linux-first coordination service. The near-term goal is
simple two-node HA that is easy to inspect and operate. The longer-term path is
one Rust crate that can add quorum-backed coordination, a key-value API, and
service discovery without hiding the safety trade-offs.

The design is deliberately KISS: ordinary unicast UDP/IP, explicit state,
small operational surfaces, and no claim stronger than the evidence supports.
Gruezi does not claim to prevent split brain or to outperform established
systems.

## Current capabilities

| Area | Status | What exists today |
| --- | --- | --- |
| HA election over UDP | Implemented | Prototype election and state reporting on `9375/udp`. |
| Read-only status API | Implemented | `/status`, `/ha/status`, and `/health` on `9376/tcp`. |
| Transition hooks | Implemented | Promote, demote, backup, and fault hook configuration. |
| VIP mutation | Experimental | Uses external Linux commands and needs kernel-level validation. |
| Authenticated HA packets | Planned | HMAC, peer identity checks, and replay protection are designed but not shipped. |
| Explicit failure policies | Planned | Strict, available, and external policies are documented below. |
| Watchdog fencing | Planned | Linux watchdog integration is not implemented. |
| `gruezi peers` listing | Experimental | The command exists, but peer discovery currently returns an empty list. |
| Direct `start` flags | Unsupported | `gruezi start --bind/--peers/--node-id` is a stub. Use YAML only. |
| KV mode | Planned | No Raft or key-value runtime is shipped. |
| DNS and agent | Planned | No service discovery runtime is shipped. |
| FreeBSD backend | Planned | The next platform step is an `ifconfig` resource backend. |
| macOS and Windows product behavior | Unsupported | CI portability is not a product support promise. |

## HA quick start

The HA runtime uses `9375/udp` for peer advertisements and `9376/tcp` for the
read-only management API. The following is a complete shape for the planned
authenticated configuration. Two nodes need different `node.id`, `peer_id`, and
addresses.

```yaml
mode: ha

node:
  id: node-a

ha:
  bind: 0.0.0.0:9375
  peer: 192.0.2.11:9375
  peer_id: node-b
  group_id: example-ha
  interface: eth0
  addresses:
    - 192.0.2.10/24
  failure_policy: strict
  priority: 100
  preempt: false
  advert_interval_ms: 1000
  dead_factor: 3
  hold_down_ms: 3000
  jitter_ms: 100
  auth:
    key: replace-with-at-least-16-random-bytes
```

Configuration lookup order is `--config`, `GRUEZI_CONFIG`, then
`/etc/gruezi/gruezi.yaml`. The current implementation still has prototype
configuration behavior, so check the capability table before deploying.

## Limitations

A two-node pair cannot distinguish a crashed peer from a healthy peer hidden by
a network partition. Automatic takeover favors availability, while refusing
takeover favors exclusive ownership. A watchdog can reset a stalled local owner,
but it cannot arbitrate two healthy nodes that cannot see each other.

Removing a VIP does not stop direct connections to a database. MariaDB
master-master remains writable on both servers unless a separate application or
database fencing hook changes that behavior. Read [HA safety](docs/HA-SAFETY.md)
before using a shared address for writes.

The current HA protocol and configuration are prototype v1 and unstable. The
planned authenticated protocol, ownership confirmation, failure policies, and
watchdog behavior are not current guarantees.

## Documentation

* [Architecture](docs/ARCHITECTURE.md), boundaries and growth path
* [HA safety](docs/HA-SAFETY.md), policies and the complete failure matrix
* [Roadmap](docs/ROADMAP.md), milestones with measurable acceptance criteria
* [Raft storage](docs/RAFT-STORAGE.md), planned storage evaluation
* [Compatibility](docs/COMPATIBILITY.md), planned Patroni etcd3 surface
* [Contributing](CONTRIBUTING.md), development and test workflow
* [CLI architecture](CLI_ARCHITECTURE.md), command and telemetry layout
* [Packaging](contrib/README.md), package and systemd assets

For packet troubleshooting, combine `gruezi status --watch` with a capture:

```bash
gruezi status --watch --interval-ms 1000 --node 192.0.2.5:9376
sudo tcpdump -ni eth0 'udp port 9375 and host 192.0.2.11' -tttt -vvv -X -s0
```

The Linux namespace and `tc netem` lab is an operator test tool, not a
production guarantee:

```bash
cargo build --release --locked
sudo ./scripts/test-ha-netem.sh baseline
```

## Roadmap summary

The roadmap starts with verified HA ownership, then adds explicit safety
policies and a watchdog. Only after those boundaries are tested does it move to
three-node Raft, a measured KV service, a narrow Patroni integration surface,
and DNS plus an agent. See [docs/ROADMAP.md](docs/ROADMAP.md).

## Telemetry

Build with the optional `telemetry` feature and set
`OTEL_EXPORTER_OTLP_ENDPOINT` to export traces. Local logging is available in
every build. See [CLI_ARCHITECTURE.md](CLI_ARCHITECTURE.md) for startup details.

## License

BSD-3-Clause.
