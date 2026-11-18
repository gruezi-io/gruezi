# Raft storage

This is a planned evaluation. No storage choice here is implemented or proven.

## Raft library

Reuse OpenRaft rather than writing a second consensus implementation. Its
license options include MIT and Apache-2.0, it is asynchronous, and it
separates `RaftLogStorage` from `RaftStateMachine`. The integration still needs
review against the exact OpenRaft release selected for implementation.

## Storage candidates

| Candidate | Planned role | Evidence still required |
| --- | --- | --- |
| raft-engine | Dedicated Raft log | Crash recovery, truncation, and disk reclamation measurements. |
| RocksDB | Applied state and MVCC | Write stalls, compaction, quota behavior, and operational footprint. |
| fjall | Log or applied state | Recovery and pressure behavior at the target dataset size. |
| redb | Applied state | Transaction and compaction behavior under interrupted writes. |
| LMDB or heed | Applied state | Reader growth, map sizing, and recovery measurements. |
| SQLite | Applied state or small deployment log | WAL, locking, compaction, and quota measurements. |

## Three different operations

Raft log truncation removes committed log entries that are no longer needed
after a snapshot. MVCC compaction removes superseded versions while preserving
the current view and required history. Physical disk reclamation returns space
to the filesystem after files or pages are rewritten and old storage is
unlinked. One operation does not prove either of the others happened.

The implementation must report each separately and measure bytes reclaimed,
not only logical key count. All storage behavior, benchmarks, and operational
claims remain Planned and unproven until the roadmap acceptance criteria pass.
