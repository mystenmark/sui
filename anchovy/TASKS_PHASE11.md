# Phase 11 progress: shared-object ordinals in commit handling

See `IMPLEMENTATION_PLAN_PHASE11.md`.

## Status: implementing the pipeline design (`docs/consensus_pipeline.md`)

## Done

1. `CommittedSubDag` carries the commit digest.
2. `consensus::sequence`: `SharedOrdinal { round, writes_before }`,
   `Schedule`, and `Sequencer::schedule`:
   - the prologue first, writing the clock;
   - user transactions in the protocol's order (by gas price, ties in
     commit order), those using randomness last;
   - ordinals counting exclusive writes, each transaction's accesses read
     before its own writes, a stream written once per transaction.
3. Tests:
   - rule by rule;
   - a randomized test against the reference's version assignment rule.
     Ordinals, resolved generation by generation, give exactly the versions
     the rule assigns. A mutation counting non-exclusive writes fails it.
4. `consensus::decode::CommitDecoder`: deserialization before commit
   handling. It takes cached transactions, decodes misses, skips rejected
   transactions, and evicts the cache. `CommitHandler` takes decoded commits
   only, deduplicates, schedules, and executes in schedule order. A test
   shows a higher gas price winning a contested coin.
5. Benchmark (`cargo bench -p validator --bench schedule`): scheduling a
   commit of 100 or 1000 transactions, each with zero to three shared
   inputs from 10 or 100,000 objects. About 55-60 ns per transaction and 4-5
   allocations per commit, independent of contention.

## Remaining

- Mapping ordinals to exact versions at execution, and executing at them.
- The prologue transaction.
- Deferral and cancellation, and randomness.

## Pipeline skeleton (done)

Every processor of the design exists with `process()` as `todo!()` and its
inward and outward edges documented; `crates/node` wires every queue,
waiter and worker. The shared tables (`validator::pipeline::tables`) are
implemented as locked maps.

## Pipeline implementation

Each processor implemented, tested against the reference where it builds
on-wire bytes, and benchmarked, by one sub-agent each (all done):

| Area | Bench (per tx unless noted) |
|---|---|
| A. `CommitStage`: chunks, checkpoint queue, prologue V4 | 260-335 ns, ~1 alloc |
| B. `VersionResolver`, `InputAvailability` (+`InputKey::Package`) | 69-106 ns; check ~150 ns/key |
| C. `Executor`, effects waiter/router, `SubmitChecker` | transfer ~91 us, shared ~149 us (sui's executor) |
| D. `SettlementScheduler`, `FundsReserver` (funds keyed by height) | 2.2-2.4 us |
| E. `ContentsBuilder`, `SummaryBuilder` (mainnet cp 325300367 byte-equal) | 2.2-2.9 us contents |
| F. `StateHasher` (ECMH, matches sui) | ~20 us (hash-to-curve) |
| G. `CheckpointAggregator`, decoder signature routing | 0.6-1.2 ms/checkpoint |

Done since:
- Resolver routes withdrawals to the funds waiter (amounts as the
  reference's `process_funds_withdrawals_for_execution`), and a stream's
  first live generation 0 makes later rounds wait on it.
- End-to-end tests of the wired pipeline (crates/node/tests/pipeline.rs):
  commits through voting, execution, settlement, checkpointing and
  certification with the pipeline's own signatures fed back; submission
  replies; back-to-back commits; multi-commit checkpoints; address
  balances. Two tests are ignored, pending decisions below.

- Funds reservation by lower and upper bounds per commit-counted version
  (spec in `docs/consensus_pipeline.md`): deposits and unwithdrawn
  reservations raise later lower bounds, settlement fixes them, cancelled
  transactions fail without running. A randomized test checks outcomes
  match the naive rule in any arrival order.

Open:
- No genesis checkpoint: checkpoint 1 waits for `CheckpointBuilt(0)`,
  which nothing creates (the tests insert a stand-in). Decide its source.
- Two transactions spending one owned object in a commit wedge the
  pipeline (no owned-object locks; the second panics in the executor).
- A worker panic drops the item silently, so whatever waits on it hangs.
- Prologue matches the reference only for commits with no user
  transactions (execution-time estimates belong to congestion control).
- Owned-object locks: decoding drops the submitter's claims.
- Executor reads the accumulator root's live version, not its chunk's
  generation.
- A failure outside a transaction leaves its submitter's reply hanging.
- Checkpoints certify in quorum order, not sequence order; buffered
  signatures are never pruned.
- `ContentsBuilder` re-reads transactions from the store for their
  signatures; carrying them in `ExecutedEffects` would avoid it.
- Epoch start (previous checkpoint, accumulator root version, state root,
  committee) is supplied for the first epoch only, pending reconfiguration.
- Restarts: commit replay, persisting the handler's state (processed
  transactions, owned-object locks, the prologue's commit timestamps) and
  most other restart issues wait for the data quarantining / writeback
  cache work.
- Agents sharing one git index swept files into each other's commits (e.g.
  79c858c07b holds Executor code); squash before the PR.
