# Phase 9: processors for consensus (voting and commits)

Requested after phase 8 (not in `PRD.md`): prepare the processors consensus
needs, without connecting consensus yet. Blocks and commits are built by hand
in tests.

- **Voting**: verify a peer's block and vote to reject transactions, as
  sui's `SuiTxValidator::verify_and_vote_batch` does.
- **Commit handling**: a minimal handler that takes a committed sub-dag and
  executes its accepted user transactions serially, as `SubmitTransaction`
  executes today. The reference's ordering, shared-version assignment,
  prologue and congestion control come later.
- **A cache of decoded transactions** between the two, keyed by block and
  position in the block, so a committed transaction is taken without looking
  at its bytes again. Not in the reference, which decodes at vote time and
  again at commit.

Decisions (asked):
- The commit handler is minimal and executes serially.
- The cache is shared between the processors and keyed by block.
- Consensus state stays in memory for now.
- Only user transactions (`UserTransactionV2`) are voted on and committed.
  Other kinds are decoded and get the reference's block-level rules.

## Consensus inputs (`crates/consensus`)

Our own minimal mirror of what consensus-core hands the application. It
takes no dependency on consensus-core, per `PRD.md`, and replaces it when
consensus is connected.

- `BlockRef { round: u32, author: u32, digest: [u8; 32] }`, which is 40
  bytes. `TransactionIndex = u16`.
- `Block { reference: BlockRef, transactions: Vec<Vec<u8>> }`: a verified
  block's reference and its raw `ConsensusTransaction` bytes. Structural
  block checks (signature, ancestors, limits) are consensus-core's.
- `CommittedSubDag { commit_index: u32, leader: BlockRef, timestamp_ms,
  blocks: Vec<Block>, rejected: Vec<(BlockRef, Vec<TransactionIndex>)> }`.

## Wire types (`messages::consensus`)

`ConsensusTransaction { tracking_id: [u8; 8], kind }` is zero-copy, with
every `ConsensusTransactionKind` variant in order (tags 0–13).
`UserTransactionV2` carries `PlainTransactionWithClaims { tx, claims }`. The
view records the transaction's byte span, so the voter can make it a
`Message<Transaction>` with one copy and reuse the existing validation,
signature and input-check code.

`TransactionClaim` covers `AddressAliases` (deprecated),
`ImmutableInputObjects` and `AddressAliasesV2`. Payloads of the kinds we
don't act on are skipped over, not interpreted.

None of these types are in sui's format snapshot. The build mirror is
checked against `bcs::to_bytes` of the sui-types values.

## Voting (`BlockVoter`)

Input: a block, its epoch, and a reply. Output: the indices to reject, or a
block error. The steps follow `SuiTxValidator`:

1. Decode every transaction. A decode failure rejects the block.
2. Block-level rules (`validate_transactions`):
   - deprecated kinds reject the block: `CertifiedTransaction`,
     `CheckpointSignature`, `CapabilityNotification`, `RandomnessStateUpdate`,
     `UserTransaction`;
   - for `UserTransactionV2`: an alias claim when `address_aliases` is on,
     alias signature indices in range, and the allowed proposer;
   - other kinds' size checks.
3. Vote on each `UserTransactionV2` (`vote_transaction`):
   - the validity check;
   - signature verification, with the claimed aliases equal to the ones used;
   - already executed means accept;
   - input checks against the store, which include owned-object liveness
     (no locks are taken, as in the reference);
   - the immutable-object claims.

   Any failure votes to reject that index.
4. Cache the decoded transactions under the block's reference. A user
   transaction is cached once its validity check and signatures passed, in
   state `Verified`, even if its input checks failed: a quorum may still
   accept it.

## The cache (`ConsensusTxCache`)

A mutex over a map from `BlockRef` to that block's entries, indexed by
transaction index. Each block is one lookup, then a vector index.

- Voters insert a block's entries after voting.
- The commit handler takes a block's entries when the block is committed,
  removing them.
- Blocks that are never committed are evicted by round
  (`evict_through(round)`). The commit handler calls it with the leader round
  minus a GC depth, the way consensus GCs.
- A miss is not an error: the commit handler decodes the bytes, for
  instance for a block we voted on before a restart, or a transaction we
  rejected that a quorum accepted.

## Commit handling (`CommitHandler`)

Single-threaded, in commit order. Minimal for now:

1. Skip a commit whose index is at or below the last handled.
2. For each block, in the order given, take the block's cached entries.
3. For each transaction:
   - skip it if it is rejected;
   - take it from the cache, or decode it;
   - skip kinds other than `UserTransactionV2`.

   A decode failure is an invariant violation: every committed block passed
   our voting, which decodes it.
4. Deduplicate by digest: within the handler's memory, and against the
   store's executed transactions.
5. Check inputs against the store, then execute and commit it, one at a time.
   A transaction whose owned input was consumed by an earlier one is dropped.
   This is the reference's post-consensus owned-object lock, where the first
   in consensus order wins.
6. Report each transaction's outcome (executed, dropped, duplicate) for the
   commit.
7. Evict cache entries below the GC round.

A transaction that comes from consensus but missed the cache never passed
our signature check. Like the reference's `new_from_consensus`, it is
trusted because a quorum accepted it: a constructor for this case moves it
to `Verified`, and nothing else does.

## Not now

- Gas-price ordering.
- Shared-object version assignment, and execution at the assigned versions.
- The consensus commit prologue, so the clock does not advance.
- Congestion control and deferral, randomness, end of epoch, JWKs,
  capabilities, checkpoint signatures, deny-config sharing.
- Persisting consensus state and replaying commits after a crash.
- The RPC submit path going through consensus.
