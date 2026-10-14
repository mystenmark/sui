# Phase 11: commit handling assigns shared-object ordinals

Requested after phase 10 (not in `PRD.md`): the consensus handler's shared
version assignment, with a different strategy from the reference.

The reference works out each shared input's exact version when it handles a
commit. That takes per-epoch state: each object's next version, initialized
from the live object. Instead, the commit handler records only an
**ordinal**, which execution later maps to an exact version. Assignment then
needs no database state at all.

Decisions (asked):
- The ordinal counts the exclusive writes before the access in its commit.
- The commit is ordered as the reference orders it (by gas price) and opens
  with the commit prologue.
- Mapping ordinals to exact versions comes later, when execution is driven by
  commits.

## The ordinal

`SharedOrdinal { round, writes_before }`, for one access by one transaction
to one shared-object stream. A stream is identified, as in the reference, by
`(object id, initial shared version)`.
- `round` is the commit's leader round. It strictly increases from commit to
  commit.
- `writes_before` is the number of the commit's earlier transactions that
  write the stream exclusively (mutability `Mutable`).

It names a version of the stream: generation `writes_before` of `round`.
- `(round, 0)` is the stream as the earlier commits left it.
- A transaction that writes the stream exclusively at `(round, k)` reads
  generation `k` and produces generation `k + 1`.
- Readers (`Immutable`) and non-exclusive writers (`NonExclusiveWrite`)
  between two exclusive writers share a generation, so they need not wait on
  each other.

  This matches the reference, where only exclusive accesses advance an
  object's next version.

Ordinals order accesses across commits too: compare `round`, then
`writes_before`.

**Mapping, later:** execution resolves generation `k` of `round` to the
version the stream's `k`-th exclusive writer of that round produced, or the
version left by earlier commits for `k = 0`. That needs some care:
- a writer that does not execute (dropped for a consumed owned input) leaves
  the generation unchanged;
- a writer that executes and aborts still produces a new version, as in Sui.

## Ordering a commit (`schedule`)

As the reference's `collect_transactions_to_schedule`, minus deferral and
cancellation:

1. **The prologue first.** The consensus commit prologue writes the clock
   (`0x6`, initial version 1) exclusively. It records what building the
   prologue transaction later needs:
   - the epoch;
   - the round;
   - the commit index;
   - the timestamp;
   - the commit digest.

   `CommittedSubDag` gains the commit digest for this.
2. **User transactions**, after deduplication, sorted as the protocol config
   says (`ByGasPrice`: stable, highest gas price first).
3. **Transactions using randomness** (a shared input `0x8`) last, sorted
   the same way. This is where the reference puts them, after the
   randomness state update.

Then each transaction's shared inputs get their ordinals in that order. The
handler keeps one per-commit table, from stream to exclusive writes so far,
cleared after each commit.

The output is a `Schedule`: the ordered transactions, and each one's shared
accesses (`id`, initial shared version, mutability, ordinal), laid out flat.

## Decoding, then handling

Deserialization happens before commit handling; the handler only handles
messages already decoded.

- **`CommitDecoder`**, on its own thread:
  - takes each committed block's transactions from the voting cache, or
    decodes those that missed;
  - skips what a quorum rejected, and other kinds;
  - evicts the cache below the GC round.

  It hands on a `DecodedCommit`: the commit's accepted user transactions in
  commit order, with where each came from.
- **`CommitHandler`** takes `DecodedCommit`s and parses nothing:
  1. it skips replayed commits;
  2. deduplicates;
  3. builds the schedule;
  4. executes the user transactions serially in schedule order, as before.

  Executing serially in that order gives the versions the ordinals will map
  to. Exceptions: the prologue is not executed yet, so the clock doesn't
  advance; execution at the mapped versions comes with the mapping.

## Not now

- Mapping ordinals to versions, and executing at them; probably with the
  waiter keyed by `(stream, ordinal)`.
- The prologue transaction itself: its `ConsensusDeterminedVersionAssignments`
  (cancelled transactions only) and the additional state digest.
- Deferral and cancellation (congestion control), randomness state updates,
  and the accumulator root version.
