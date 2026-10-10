# Phase 9 progress: processors for consensus

See `IMPLEMENTATION_PLAN_PHASE9.md`.

## Status: planning done

## Remaining

1. `messages::consensus`: `ConsensusTransaction` view, claims, and a build
   mirror checked against sui-types BCS.
2. `crates/consensus`: `BlockRef`, `Block`, `CommittedSubDag`, and helpers
   for building them in tests.
3. `ConsensusTxCache`.
4. `BlockVoter`: block-level rules, per-transaction votes, filling the cache.
5. `CommitHandler`: minimal serial execution of committed transactions.
6. Tests over hand-built blocks and commits:
   - votes;
   - cache hits and misses;
   - rejected and duplicate transactions;
   - an owned-object conflict decided by commit order.
