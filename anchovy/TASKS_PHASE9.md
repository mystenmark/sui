# Phase 9 progress: processors for consensus

See `IMPLEMENTATION_PLAN_PHASE9.md`.

## Status: the planned steps are done

## Done

1. `messages::consensus`: `ConsensusTransaction` views for all 14 kinds,
   claims, and build mirrors. Tested against sui-types BCS with
   `sui-oracle --consensus-vectors` (499 vectors).
2. `validation::consensus::decode_checks`: what sui's decoding checks beyond
   the wire format. These are BLS points, signer bitmaps, non-empty alias
   claims, `Duration` overflow, and embedded transactions' deserialization
   checks. With parsing, they accept exactly what sui decodes, over the
   same vectors.
3. `validation::verify::signer_signature_indices`: each required signer's
   signature index, as alias claims carry it. Tested against the reference's
   `verify_sender_signed_data_message_signatures`.
4. `crates/consensus`: `BlockRef`, `Block`, `CommittedSubDag`.
5. `ConsensusTxCache`: entries by block, taken on commit and evicted by round.
6. `BlockVoter`, the reference's `verify_and_vote_batch`:
   - decode with the decode checks;
   - block-level rules: deprecated kinds, the alias claim requirement,
     alias indices, allowed proposers, DKG size, observation count, deny
     config gate and share limits;
   - per-transaction votes: validity, signatures, the alias claim against
     the signatures, executed-means-accept, input checks, immutable claims;
   - caching.
7. `CommitHandler`:
   - skips replayed commits and rejected transactions;
   - takes transactions from the cache or decodes them and trusts consensus
     (`checks::sequenced_by_consensus`);
   - deduplicates;
   - checks inputs, then executes and commits serially. The first spender
     of an owned input wins.
   - evicts the cache below the GC round.
8. `ConsensusProcessors`: voting and commits on their own threads, sharing
   the cache.
9. Tests over hand-built blocks and commits (`validator/tests/consensus.rs`).

## Known gaps

- Alias objects are not read: every signer's alias version is taken as
  `None`. A signer with an on-chain alias gets a reject vote from us, and
  is not verified against its aliases.
- Checkpoint signatures in blocks are not verified (there is no committee
  yet), so a block with a bad one is accepted.
- Any input-check failure at commit time drops the transaction. The
  reference only drops for owned-object lock conflicts; other failures
  surface in execution.
- The handler's processed-digest set grows without bound, and its state is
  not persisted.
- Ordering by gas price, shared-version assignment, the commit prologue,
  congestion control: see the plan's "Not now".
