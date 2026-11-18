# HA safety

This document describes the safety boundary of a two-node pair. It does not
claim split-brain prevention. The protocol and configuration are prototype v1,
and the items marked Planned are not current guarantees.

## The two-node limit

A heartbeat timeout cannot distinguish a crashed peer from a healthy peer
hidden by a network partition. A watchdog can self-fence a stalled owner, but
it cannot arbitrate two healthy partitioned nodes. Therefore two nodes cannot
provide both automatic failover and exclusive ownership without another
authority, witness, or fencing system.

## Failure policies

* **strict** is fail closed. Ownership requires fresh bilateral peer consent.
  Silence never authorizes promotion. After a peer crash, the VIP stays
  unavailable until the peer returns or an operator intervenes. This is
  intentional.
* **available** promotes after the local follower deadline when the peer is
  not fresh. It accepts that a partition can leave both nodes owning the VIP.
  The status source is `assumed`. A watchdog does not prevent ownership overlap
  in `available`.
* **external** requires an external grant and, when needed, confirmed release
  or fencing. Peer silence alone grants nothing.

Authenticated HMAC packets, ownership confirmation, these policies, and the
watchdog are **Planned** until implementation evidence changes their status.

## Failure matrix

| Failure | strict | available | external |
| --- | --- | --- | --- |
| Startup | Both nodes coordinate. A lone node stays unavailable. | Connected nodes coordinate. An isolated node may promote after its deadline. | A valid grant is required. |
| Graceful failover | Safe release, confirmed absence, then acquire, with a brief gap. | Safe handoff when the session is fresh. | Release or confirmed fencing is required before acquire. |
| Active host crash | Standby does not promote. VIP is unavailable until peer return or operator action. | Standby promotes after the follower deadline. | No promotion without grant and release or fencing proof. |
| Standby host crash | Owner loses consent and withdraws. VIP is unavailable. | Owner continues. | Owner continues only while its grant remains valid. |
| Full partition | Owner withdraws. No takeover. Safe but unavailable. | Both nodes may own. This is unsafe for exclusive resources. | No grant from silence, so unavailable. |
| A to B loss | Consent stops and the owner withdraws. No takeover. | B may promote while A still owns. | No new grant from packet loss. |
| B to A loss | Consent stops and the owner withdraws. No takeover. | A continues while B sees A. | No new grant from packet loss. |
| Gruezi freeze or crash with host up | Watchdog may self-fence. Peer still does not promote. | Peer may promote after the deadline. A residual VIP is possible until fencing. | Promotion waits for grant and fencing or release. |
| VIP withdrawal fails | No released state is advertised. Successor is blocked and watchdog stays armed. | Cooperative transfer blocks, but timeout takeover can overlap. | No release or fencing proof means no acquire. |
| Watchdog arm fails | Required mode makes acquisition fail. | Automatic mode may run unprotected only when conclusively unarmed. | Grant does not override a required watchdog failure. |
| Recovery or failback | Fresh consent and handoff are required. | Visible incumbent is honored when `preempt: false`. | Grant and fencing rules still apply. |

## Watchdog semantics

Watchdog modes are `off`, `automatic`, and `required`. The planned controller
arms only after authority is valid and before adding the VIP. It feeds only
from a coordinator iteration where authority is valid, the resource is
confirmed present, and the generation is unchanged. It disarms only after
confirmed absence and no mutation is in flight.

`nowayout: reject` refuses a required configuration unless the device supports
disarm. `nowayout: reboot` explicitly accepts a device that will reboot on
expiry. Automatic mode may proceed unprotected only when the device is
conclusively unarmed. Unknown or partial arm state blocks acquisition.

The planned timing check is `A + J + U + S < W`, where `A` is advert interval,
`J` is command delay, `U` is withdrawal timeout, `S` is the safety margin, and
`W` is the effective watchdog timeout. Do not enable systemd
`RuntimeWatchdogSec` on the same device. Gruezi must be the only watchdog
owner, and hook children must not inherit its descriptor.

This watchdog does not prevent ownership overlap in `available`. A peer can
promote after the deadline while a frozen node has not yet reset.

## Database fencing

Gruezi decides VIP ownership only. Removing a VIP does not stop direct
connections or conflicting database writes. MariaDB master-master keeps both
servers writable unless another system changes that behavior. Strict
single-writer operation needs separate application or database fencing, such as
an `on_promote` or `on_demote` hook that toggles `read_only`. Gruezi does not
provide that fencing.

## Operations

Key rotation is a coordinated restart of both nodes with the new key. Stop
both nodes, update both configurations, then start both nodes. An upgrade uses
the same procedure: stop both, upgrade both, start both. A strict pair has an
outage during that procedure.

Everything in this document that is not already identified as current
prototype behavior is Planned.
