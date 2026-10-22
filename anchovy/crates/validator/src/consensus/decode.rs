// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Readying a commit for its handler: each committed block's transactions
//! taken from the voting cache, or decoded if it missed. Commit handling
//! takes only decoded transactions, so no bytes are parsed on its thread.

use std::sync::Arc;

use ::consensus::{BlockRef, CommittedSubDag, TransactionIndex};
use messages::Message;
use messages::base::{ObjectId, ObjectRef};
use messages::build::system_transaction::{Jwk, JwkId};
use messages::consensus::{ConsensusTransaction, ConsensusTransactionKind, TransactionClaim};
use tokio::sync::oneshot;
use validation::inputs::{InputKind, input_objects};
use workqueue::{Processor, Refusal, Refuse};

use crate::checks::{self, VerifiedTransaction};

use crate::consensus::cache::{CachedTransaction, ConsensusTxCache};
use crate::consensus::execution_time::{Key, SharedObservations, duration};
use crate::consensus::jwk::JwkVote;
use crate::epoch::EpochParams;
use crate::pipeline::items::{BuiltWaitItem, BuiltWaits, CheckpointSignature};
use crate::pipeline::keys::CheckpointBuilt;

/// A commit to decode, from the epoch's consensus.
pub struct CommitRequest {
    pub commit: CommittedSubDag,
    /// Answered once the commit is handled and the pipeline can take the
    /// next. Dropped, the commit was not handled: the pipeline stopped.
    pub handled: Option<oneshot::Sender<()>>,
}

/// A full queue drops the commit, and its answer; consensus replays commits
/// not handled.
impl Refuse for CommitRequest {
    fn refuse(self, _: Refusal) {}
}

/// A committed sub-dag with its accepted user transactions decoded, in
/// commit order: what commit handling takes.
#[derive(Debug)]
pub struct DecodedCommit {
    pub commit_index: u32,
    pub leader: BlockRef,
    pub timestamp_ms: u64,
    pub digest: [u8; 32],
    pub transactions: Vec<CommittedTransaction>,
    /// The execution time observations it carries, in commit order.
    pub observations: Vec<SharedObservations>,
    /// The JSON web keys validators voted for, in commit order.
    pub jwks: Vec<JwkVote>,
    /// Where the transactions a quorum voted to reject were.
    pub rejected: Vec<(BlockRef, TransactionIndex)>,
    /// Transactions taken from the cache, and decoded again.
    pub cache_hits: usize,
    pub cache_misses: usize,
    /// The commit's answer to consensus, once handled.
    pub handled: Option<oneshot::Sender<()>>,
}

/// A user transaction a commit carries, and where it was in the commit.
#[derive(Debug)]
pub struct CommittedTransaction {
    pub block: BlockRef,
    pub index: TransactionIndex,
    pub transaction: VerifiedTransaction,
    /// The owned inputs it locks (see `owned_inputs_to_lock`).
    pub owned: OwnedInputs,
}

impl CommittedTransaction {
    /// `transaction`, at `index` in `block`, whose submitter claimed the
    /// inputs `immutable` immutable.
    pub fn new(
        block: BlockRef,
        index: TransactionIndex,
        transaction: VerifiedTransaction,
        immutable: &[ObjectId],
    ) -> CommittedTransaction {
        let owned = owned_inputs_to_lock(&transaction, immutable);
        CommittedTransaction {
            block,
            index,
            transaction,
            owned,
        }
    }
}

/// The object references a transaction locks, or why its inputs are
/// invalid.
pub type OwnedInputs = Result<Vec<ObjectRef>, validation::Error>;

/// The reference's `owned_object_refs_to_lock`: the owned object inputs,
/// gas included, less those whose ids the submitter claimed immutable.
/// Voters verify the claim, so the handler need not read the store to tell
/// immutable inputs apart. A function of the consensus transaction alone.
pub fn owned_inputs_to_lock(
    transaction: &VerifiedTransaction,
    immutable: &[ObjectId],
) -> OwnedInputs {
    let inputs = input_objects(transaction.get().0.data())?;
    Ok(inputs
        .iter()
        .filter_map(|input| match input {
            InputKind::ImmOrOwned(r) if !immutable.contains(&r.id) => Some(**r),
            _ => None,
        })
        .collect())
}

/// The first `ImmutableInputObjects` claim, as the reference's
/// `TransactionWithClaims::get_immutable_objects` finds it; none if absent.
pub fn claimed_immutable<'a>(claims: &[TransactionClaim<'a>]) -> &'a [ObjectId] {
    claims
        .iter()
        .find_map(|c| match c {
            TransactionClaim::ImmutableInputObjects(ids) => Some(*ids),
            _ => None,
        })
        .unwrap_or(&[])
}

/// A full queue drops the commit; consensus replays commits not handled.
impl Refuse for DecodedCommit {
    fn refuse(self, _: Refusal) {}
}

/// Decodes commits, taking what voting cached.
///
/// Inward:
/// - consensus (black box): `CommitRequest`.
///
/// Outward:
/// - `CommitHandler` / `CommitStage`: `DecodedRequest`;
/// - built waiter, wait for: each committed checkpoint signature
///   (`CheckpointSignature`), keyed by `CheckpointBuilt(seq)`, once routed.
///
/// Reads and evicts: the decoded-transaction cache.
pub struct CommitDecoder {
    epoch: Arc<EpochParams>,
    cache: Arc<ConsensusTxCache>,
    /// Where committed checkpoint signatures go; unrouted until set.
    signatures: Option<BuiltWaits>,
}

impl CommitDecoder {
    pub fn new(epoch: Arc<EpochParams>, cache: Arc<ConsensusTxCache>) -> CommitDecoder {
        CommitDecoder {
            epoch,
            cache,
            signatures: None,
        }
    }

    /// Routes committed checkpoint signatures to the built waiter.
    #[must_use]
    pub fn with_signatures(mut self, signatures: BuiltWaits) -> CommitDecoder {
        self.signatures = Some(signatures);
        self
    }

    /// The commit's accepted user transactions: what a quorum rejected is
    /// skipped, checkpoint signatures go to the built waiter if routed,
    /// and other kinds are left out. Then the cache drops the blocks of
    /// rounds consensus no longer commits.
    pub fn decode(&mut self, commit: CommittedSubDag) -> DecodedCommit {
        let CommittedSubDag {
            commit_index,
            digest,
            leader,
            timestamp_ms,
            blocks,
            rejected,
        } = commit;
        let mut decoded = DecodedCommit {
            commit_index,
            leader,
            timestamp_ms,
            digest,
            transactions: Vec::new(),
            observations: Vec::new(),
            jwks: Vec::new(),
            rejected: rejected
                .iter()
                .flat_map(|(block, indices)| indices.iter().map(|i| (*block, *i)))
                .collect(),
            cache_hits: 0,
            cache_misses: 0,
            handled: None,
        };
        let mut signatures = waiter::WaitBatch::new();
        for block in blocks {
            // The block's decoded transactions, if this validator voted on it.
            let mut cached = self.cache.take(&block.reference);
            let rejected_here = rejected
                .iter()
                .find(|(b, _)| *b == block.reference)
                .map_or(&[][..], |(_, r)| r);
            for (index, bytes) in block.transactions.into_iter().enumerate() {
                let index = index as TransactionIndex;
                if rejected_here.contains(&index) {
                    continue;
                }
                let entry = cached
                    .as_mut()
                    .and_then(|entries| entries.get_mut(usize::from(index)))
                    .and_then(Option::take);
                let (transaction, owned) = if let Some(entry) = entry {
                    decoded.cache_hits += 1;
                    (entry.transaction, entry.owned)
                } else {
                    // Not voted on here, rejected here, or not a user
                    // transaction.
                    match decode_transaction(bytes) {
                        Decoded::User(entry) => {
                            decoded.cache_misses += 1;
                            (entry.transaction, entry.owned)
                        }
                        Decoded::CheckpointSignature(signature) => {
                            if self.signatures.is_some() {
                                let key = CheckpointBuilt(signature.seq);
                                signatures.push(BuiltWaitItem::Signature(signature), [key]);
                            }
                            continue;
                        }
                        Decoded::Observations(mut observations) => {
                            observations.author = block.reference.author;
                            decoded.observations.push(observations);
                            continue;
                        }
                        Decoded::Jwk(mut vote) => {
                            vote.author = block.reference.author;
                            decoded.jwks.push(vote);
                            continue;
                        }
                        Decoded::Other => continue,
                    }
                };
                decoded.transactions.push(CommittedTransaction {
                    block: block.reference,
                    index,
                    transaction,
                    owned,
                });
            }
        }

        if let Some(queue) = &self.signatures
            && !signatures.is_empty()
        {
            // Inside the pipeline a full queue is a sizing bug, not load to
            // shed; a closed one means the pipeline is shutting down.
            if let Err(workqueue::PushError::Full(_)) =
                queue.try_push(waiter::Command::Wait(signatures))
            {
                panic!("the built waiter refused checkpoint signatures");
            }
        }

        let gc_depth = self.epoch.config.gc_depth();
        if gc_depth > 0 {
            self.cache
                .evict_through(leader.round.saturating_sub(gc_depth));
        }
        decoded
    }
}

/// A committed transaction that missed the cache, by the kinds routed.
// Returned and matched at once, and nearly always a user transaction.
enum Decoded {
    User(CachedTransaction),
    CheckpointSignature(CheckpointSignature),
    /// Their author not yet set.
    Observations(SharedObservations),
    /// Its author not yet set.
    Jwk(JwkVote),
    Other,
}

/// The checkpoint signature `transaction` carries, if it is one.
pub(crate) fn checkpoint_signature(
    transaction: &Message<ConsensusTransaction<'static>>,
) -> Option<CheckpointSignature> {
    let (ConsensusTransactionKind::CheckpointSignature(message)
    | ConsensusTransactionKind::CheckpointSignatureV2(message)) = transaction.get().kind()
    else {
        return None;
    };
    let summary = &message.get().summary.data;
    let wire = transaction.wire_bytes();
    // The signature message is the transaction's last field, and starts
    // with its summary, whose bytes are a slice of the wire.
    let start = summary.bytes.as_ptr() as usize - wire.as_ptr() as usize;
    Some(CheckpointSignature {
        seq: summary.sequence_number,
        message: wire[start..].into(),
    })
}

/// Decodes a committed transaction that missed the cache.
///
/// # Panics
/// If it does not decode: every committed block passed this validator's
/// voting, which decodes each of its transactions.
fn decode_transaction(bytes: Vec<u8>) -> Decoded {
    let transaction = Message::<ConsensusTransaction<'static>>::parse(bytes)
        .map_err(|(e, _)| e)
        .expect("a committed transaction decodes");
    if let Some(signature) = checkpoint_signature(&transaction) {
        return Decoded::CheckpointSignature(signature);
    }
    let immutable = match transaction.get().kind() {
        ConsensusTransactionKind::UserTransactionV2(user) => {
            claimed_immutable(user.claims()).to_vec()
        }
        ConsensusTransactionKind::ExecutionTimeObservation(observation) => {
            return Decoded::Observations(SharedObservations {
                author: 0,
                authority: observation.authority.bytes,
                generation: observation.generation,
                estimates: observation
                    .estimates
                    .iter()
                    .map(|(key, d)| {
                        let d = duration(*d).expect("voting rejects unrepresentable durations");
                        (Key::of_observation(key), d)
                    })
                    .collect(),
            });
        }
        ConsensusTransactionKind::NewJwkFetched(fetched) => {
            return Decoded::Jwk(JwkVote {
                author: 0,
                authority: fetched.authority.bytes,
                id: JwkId::from(&fetched.jwk_id),
                jwk: Jwk::from(&fetched.jwk),
            });
        }
        _ => return Decoded::Other,
    };
    let transaction = checks::sequenced_by_consensus(
        transaction
            .into_user_transaction()
            .expect("a user transaction"),
    );
    Decoded::User(CachedTransaction::new(transaction, &immutable))
}

impl Processor for CommitDecoder {
    type Input = CommitRequest;
    type Output = DecodedCommit;

    fn process(
        &mut self,
        request: CommitRequest,
        _bump: &mut workqueue::Bump,
    ) -> Option<DecodedCommit> {
        let mut decoded = self.decode(request.commit);
        decoded.handled = request.handled;
        Some(decoded)
    }
}
