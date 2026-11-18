# Architecture

Gruezi is one Rust crate with small boundaries. The boundaries are operational
contracts, not a plugin framework.

## Responsibilities

* **Election** chooses an elected role, `init`, `standby`, or `active`, from
  peer observations and local priority.
* **Ownership authority** decides whether the elected role is allowed to own a
  resource. Future quorum or external providers belong here, not in election.
* **Resource driver** reconciles an optional VIP with the authority decision and
  reads the kernel back before reporting ownership.
* **Watchdog** fences a stalled local owner. It is fed only when authority and
  confirmed resource state are valid.
* **Peer protocol** transports authenticated advertisements, identity, session,
  replay, and transfer state.
* **Status** reports each fact separately. Elected role is not the same as
  confirmed ownership.

The VIP is optional. A deployment can consume leadership through hooks or
status without moving an address.

## Growth path

The current path is a two-node HA prototype. The planned growth path is:

1. HA ownership correctness and explicit failure policies.
2. Three-node Raft authority.
3. A measured KV service.
4. DNS and an agent that consume KV state.

The likely implementation order is Raft, then KV, then DNS and agent behavior.
No later component is shipped by this document.

## Platform scope

Linux is the implementation target. FreeBSD is the next platform step, with an
`ifconfig` resource backend. macOS and Windows have no product behavior
promise. Portable tests may keep CI green on those systems, but that is not
support for HA operation there.

See [HA safety](HA-SAFETY.md) for the ownership boundary and
[the roadmap](ROADMAP.md) for objective milestones.
