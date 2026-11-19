# HA safety

This document describes the safety boundary of a two-node pair. It does not
claim split-brain prevention. The protocol and configuration are prototype v1.
Linux is the implementation target. FreeBSD is the next platform step, and
macOS and Windows have no product behavior promise.

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

Authenticated HMAC packets, ownership confirmation, and all three policies are
**Implemented**. The Linux watchdog is **Experimental** because the acceptance
depends on a real device and host reset. The shipped Rocky helper passes,
including a real VM reset. External grant behavior is **Experimental**, with
two-node acquisition, renewal, stale-grant withdrawal, and lone-node fencing
covered by task 26.

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
| Gruezi freeze or crash with host up | Watchdog may self-fence. Peer still does not promote. | Peer may promote after the deadline. A residual VIP is possible until fencing. A resumed owner that stalled past the follower deadline yields with `local_stall`; the visible incumbent keeps the VIP when it has already promoted. | Promotion waits for grant and fencing or release. |
| VIP withdrawal fails | No released state is advertised. Successor is blocked and watchdog stays armed. | Cooperative transfer blocks, but timeout takeover can overlap. | No release or fencing proof means no acquire. |
| Watchdog arm fails | Required mode makes acquisition fail. | Automatic mode may run unprotected only when conclusively unarmed. | Grant does not override a required watchdog failure. |
| Recovery or failback | Fresh consent and handoff are required. | Visible incumbent is honored when `preempt: false`. | Grant and fencing rules still apply. |

## Watchdog semantics, Experimental

Watchdog modes are `off`, `automatic`, and `required`. The controller
arms only after authority is valid and before adding the VIP. It feeds only
from a coordinator iteration where authority is valid, the resource is present
or unmanaged, the generation is unchanged, and the feed budget is adequate.
The resource term is the same one used by `owned`, so a no-VIP node feeds
instead of resetting. It disarms only after
confirmed absence and no mutation is in flight.

`nowayout: reject` refuses a required configuration unless the device supports
disarm. `nowayout: reboot` explicitly accepts a device that will reboot on
expiry. Automatic mode may proceed unprotected only when the device is
conclusively unarmed. Unknown or partial arm state blocks acquisition.

The timing check is `A + J + U + S < W`, where `A` is advert interval,
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

## Validation status

Acceptance run on two Proxmox VMs (Rocky Linux 10, kernel 6.12.0-264.16.1.el10_3,
same L2 segment, VIP on eth0) at repository commit 2d531e7. Each live row
deployed the policy with Ansible, injected the fault with ansible ad-hoc
commands or the ha-netem and ha-drop playbooks, and waited (bounded) until
both nodes matched the expected kernel VIP check (`ip -o addr show dev eth0`)
and the /status fields `state`, `owned`, `authority.source`, `safety.level`
and `resource.state`. The original 28 of 30 matrix run exposed lab and runtime
defects. The landed fixes in commits `ac18c75`, `1ba5659`, `464f896`,
`3c5f5cd`, `d39e3bf`, `2f00aba`, and `7ac2ba2` corrected those findings.

Live rows (A is gruezi-a, priority 150; B is gruezi-b, priority 100):

| Policy | Scenario | Live | Netem matrix | Kernel VIP owner observed | Evidence (local, gitignored) |
| --- | --- | --- | --- | --- | --- |
| strict | startup | PASS | PASS | A | `strict-startup-after/observations.txt` |
| strict | graceful-failover | PASS | PASS | none | `strict-graceful-failover-after/observations.txt` |
| strict | active-crash | PASS | PASS | A | `strict-active-crash-after/observations.txt` |
| strict | standby-crash | PASS | PASS | none | `strict-standby-crash-after/observations.txt` |
| strict | full-partition | PASS | PASS | none | `strict-full-partition-after/observations.txt` |
| strict | a-to-b-loss | PASS | PASS | none | `strict-a-to-b-loss-after/observations.txt` |
| strict | b-to-a-loss | PASS | PASS | none | `strict-b-to-a-loss-after/observations.txt` |
| strict | process-freeze | PASS | PASS | A | `strict-process-freeze-after/observations.txt` |
| strict | failed-withdrawal | PASS | PASS | A | `strict-failed-withdrawal-after/observations.txt` |
| strict | recovery-failback | PASS | PASS | A | `strict-recovery-failback-after/observations.txt` |
| available | startup | PASS | PASS | A | `available-startup-after/observations.txt` |
| available | graceful-failover | PASS | PASS | B | `available-graceful-failover-after/observations.txt` |
| available | active-crash | PASS | PASS | A+B | `available-active-crash-after/observations.txt` |
| available | standby-crash | PASS | PASS | A | `available-standby-crash-after/observations.txt` |
| available | full-partition | PASS | PASS | A+B | `available-full-partition-after/observations.txt` |
| available | a-to-b-loss | PASS | PASS | B | `available-a-to-b-loss-after/observations.txt` |
| available | b-to-a-loss | PASS | PASS | A | `available-b-to-a-loss-after/observations.txt` |
| available | recovery-failback | PASS | PASS | A | `available-recovery-failback-after/observations.txt` |
| available | process-freeze | PASS after incumbent fix | PASS after deterministic thaw | B | `task-24-gruezi-ha-safety-roadmap.txt`, `task-28-gruezi-ha-safety-roadmap.txt` |
| available | failed-withdrawal | PASS | PASS | A | `task-25-gruezi-ha-safety-roadmap.txt`, `task-27-gruezi-ha-safety-roadmap.txt` |
| external | startup | PASS | PASS | A | `external-startup-after/observations.txt` |
| external | graceful-failover | PASS | PASS | B | `external-graceful-failover-after/observations.txt` |
| external | active-crash | PASS | PASS | B | `external-active-crash-after/observations.txt` |
| external | standby-crash | PASS | PASS | A | `external-standby-crash-after/observations.txt` |
| external | full-partition | PASS | PASS | A | `external-full-partition-after/observations.txt` |
| external | a-to-b-loss | PASS | PASS | A | `external-a-to-b-loss-after/observations.txt` |
| external | b-to-a-loss | PASS | PASS | A | `external-b-to-a-loss-after/observations.txt` |
| external | process-freeze | PASS | PASS | A | `external-process-freeze-after/observations.txt` |
| external | recovery-failback | PASS | PASS | A | `external-recovery-failback-after/observations.txt` |
| external | failed-withdrawal, A restarted after SIGKILL | PASS | PASS | A | `external-failed-withdrawal-sigkill-restart-after/observations.txt` |
| external | failed-withdrawal, A restarted after SIGTERM | PASS after release-proof fix | PASS | A | `task-23-gruezi-ha-safety-roadmap.txt` |

In the Kernel VIP owner column "none" means neither kernel holds the VIP. For
strict that is a PASS: withdrawal and the absent VIP are the intended outcome.
After a crash or freeze of an owner the VIP stays in the host kernel (a
residual VIP) while its status is unavailable. The available partition row
shows dual ownership (A+B), the documented unsafe overlap.

Observed results that differ from the table above or from the design:

* **available, frozen owner resumed (fixed by todo 24).** At 2d531e7, after
  SIGCONT both nodes yielded at the same moment, the VIP was absent for about
  one second, then the higher priority node reacquired. Now the resumed owner
  detects its own stall, withdraws without advertising its stale claim, and
  waits until the peer acknowledges that; the visible incumbent B keeps the
  VIP with no gap. `test-ha-netem.sh process-freeze available` PASSes 3/3 on
  gruezi-a (kernel 264), and an 80-sample trace after SIGCONT never shows
  both kernels holding the VIP.
* **External grant lifetime and release proof.** Configuration rejects a grant
  TTL that cannot cover withdrawal and safety margin. A release proof is
  discarded when the peer restarts or reacquires, so a successor requires
  confirmed fencing or a fresh release. The tests cover requested versus
  confirmed fencing and stale-grant withdrawal. Evidence:
  `task-21-gruezi-ha-safety-roadmap.txt`, `task-23-gruezi-ha-safety-roadmap.txt`,
  and `task-26-gruezi-ha-safety-roadmap.txt`.
* **Watchdog (softdog) on gruezi-b.** The shipped helper passes: it arms after
  ownership, feeds beyond the 15 second timeout, disarms after confirmed VIP
  absence, and resets the VM after SIGSTOP. Evidence:
  `task-22-gruezi-ha-safety-roadmap.txt` and
  `task-17-gruezi-ha-safety-roadmap.md`. The watchdog does not prevent overlap
  under `available`.
* **MariaDB master-master lab on the VMs.** 9 of 9 PASS (podman 6.1.1,
  rootless), including the unfenced direct write limitation and the partition
  overlap. See the Database fencing section; this does not fence the database.
* **Lab tooling.** Ansible values, iptables-nft setup, quoting, dead-node
  collection, and kernel-module checks are fixed. Evidence:
  `task-20-gruezi-ha-safety-roadmap.txt`.

Full per-row observations, the discarded first-pass results and the cleanup
verification are in `.omo/evidence/task-17-gruezi-ha-safety-roadmap.md`.

The current claim map is: HA election over UDP is **Implemented** by
`tests/ha_mode.rs`; the status API is **Implemented** by
`src/gruezi/api.rs:76`, including
`status_json_preserves_every_existing_field_and_health`; transition hooks are
**Implemented** by `tests/ha_hooks.rs`; VIP ownership, authenticated packets,
policies, status, and watchdog behavior are **Implemented** or **Experimental**
as labelled above, with evidence in tasks 6 through 17 and 20 through 28.
`gruezi peers` remains **Experimental** because
`src/gruezi/peers.rs:list_peers` returns an empty list.
