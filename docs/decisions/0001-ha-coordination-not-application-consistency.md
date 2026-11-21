# 1. Gruezi provides HA coordination, not application-level consistency

Status: Accepted, November 21, 2026. Supersedes no earlier decision.

## Context

The two-node MariaDB strict HA investigation produced enough evidence to
characterise the architectural limits of what a generic coordinator can
promise for a replicated database. Pursuing automatic, lossless MariaDB
failover was consuming the project's capacity without converging, and the
remaining obligations require enforcement authority that Gruezi does not own.

## Decision

Gruezi is a generic HA coordination engine. Its responsibilities are leader
election and coordination, authenticated peer communication, explicit
ownership authority, deterministic resource lifecycle, VIP acquisition and
withdrawal, watchdog and fencing integration, defined failure policies with
stated limitations, and conservative behaviour when safety cannot be
established.

Gruezi operates only within the guarantees its distributed-system model and
stated assumptions permit. It does not manufacture stronger guarantees from
arbitrary delays, gateway reachability checks, health markers, scripts, or
application-specific recovery heuristics.

The `strict`, `available`, and `external` policies keep their explicit
trade-offs. Peer silence is not proof that a previous owner stopped acting, and
`strict` does not claim exclusive resource safety without the enforcement or
fencing assumptions that would make such a claim true.

## Consequences

* MariaDB remains an optional, Experimental integration.
* MariaDB-specific GTID recovery and dirty-history reconciliation are not core
  Gruezi requirements.
* Safe automatic database failover is not a v1 release requirement.
* F-06 implementation and production-grade database fencing remain deferred.
  The accepted F-06 offline design stays design-only.
* Gruezi's core development is not blocked by the MariaDB experiment.
* Future database integrations may use the generic lifecycle commands, but each
  must establish its own application-specific safety guarantees.

The investigation record is [MARIADB_HA_LESSONS.md](../MARIADB_HA_LESSONS.md).
