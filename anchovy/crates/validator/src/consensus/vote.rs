// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Voting on a peer's block: the reference's `SuiTxValidator::
//! verify_and_vote_batch`. A block whose transactions break a rule every
//! honest validator applies alike is invalid as a whole; otherwise each user
//! transaction gets this validator's vote, and the decoded transactions are
//! kept for the block's commit.

use std::sync::Arc;

use ::consensus::{Block, BlockRef, TransactionIndex};
use containers::Bump;
use messages::Message;
use messages::base::{ObjectId, SuiAddress};
use messages::consensus::{
    ConsensusTransaction, ConsensusTransactionKind, PlainTransactionWithClaims, TransactionClaim,
    TransactionDenyRules,
};
use messages::object::Owner;
use messages::transaction::TransactionExpiration;
use tokio::sync::oneshot;
use validation::inputs::{InputKind, input_objects};
use validation::verify::SignerIndices;
use workqueue::{Processor, Refusal, Refuse};

use crate::checks::{self, SignatureChecks, Unchecked, VerifiedTransaction};
use crate::consensus::cache::{BlockEntries, ConsensusTxCache};
use crate::epoch::EpochState;

/// The reference's `dkg_v1::DKG_MESSAGES_MAX_SIZE`.
const DKG_MESSAGES_MAX_SIZE: usize = 400_000;

/// The reference's `TransactionDenyRules::MAX_SHARE_ENTRIES`,
/// `MAX_ZKLOGIN_PROVIDER_LENGTH` and `MAX_SHARE_SERIALIZED_BYTES`.
const MAX_SHARE_ENTRIES: usize = 5_000;
const MAX_ZKLOGIN_PROVIDER_LENGTH: usize = 256;
const MAX_SHARE_SERIALIZED_BYTES: usize = 200 * 1024;

/// A block's verdict: the indices of the transactions this validator votes
/// to reject, or why the block is invalid.
pub type Verdict = Result<Vec<TransactionIndex>, InvalidBlock>;

/// A block to vote on, in the epoch it belongs to.
pub struct VoteRequest {
    pub epoch: Arc<EpochState>,
    pub block: Block,
    pub reply: oneshot::Sender<Verdict>,
}

/// Why a block is invalid: the first offending transaction and the rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidBlock {
    pub index: usize,
    pub reason: String,
}

/// A full queue leaves the block unvoted, as if the voter were slow.
impl Refuse for VoteRequest {
    fn refuse(self, _: Refusal) {}
}

/// Why this validator votes to reject a user transaction.
#[derive(Debug)]
pub enum RejectReason {
    Invalid(validation::Error),
    MissingAliasClaim,
    AliasesChanged,
    ImmutableClaim(String),
}

impl From<validation::Error> for RejectReason {
    fn from(e: validation::Error) -> RejectReason {
        RejectReason::Invalid(e)
    }
}

/// Votes on blocks against the store, filling the cache of decoded
/// transactions.
pub struct BlockVoter {
    store: Arc<store::Store>,
    cache: Arc<ConsensusTxCache>,
    signatures: SignatureChecks,
    bump: Bump,
}

/// Enough for the temporaries of nearly every transaction.
const ARENA_BYTES: usize = 64 * 1024;

impl BlockVoter {
    pub fn new(store: Arc<store::Store>, cache: Arc<ConsensusTxCache>) -> BlockVoter {
        BlockVoter {
            store,
            cache,
            signatures: SignatureChecks::new(checks::GENERATION),
            bump: Bump::with_capacity(ARENA_BYTES),
        }
    }

    /// Votes on `block`: `verify_and_vote_batch`. A valid block's decoded
    /// transactions are cached.
    pub fn vote(&mut self, epoch: &Arc<EpochState>, block: Block) -> Verdict {
        let Block {
            reference,
            transactions,
        } = block;

        // 1. Decode every transaction, as sui decodes it; one that does not
        //    decode invalidates the block.
        let mut decoded = Vec::with_capacity(transactions.len());
        for (index, bytes) in transactions.into_iter().enumerate() {
            let undecodable = |reason: String| InvalidBlock {
                index,
                reason: format!("undecodable: {reason}"),
            };
            let transaction = Message::<ConsensusTransaction<'static>>::parse(bytes)
                .map_err(|(e, _)| undecodable(format!("{e:?}")))?;
            self.bump.reset();
            validation::consensus::decode_checks(transaction.get(), &self.bump)
                .map_err(|e| undecodable(format!("{e:?}")))?;
            decoded.push(transaction);
        }

        // 2. The rules every honest validator applies to the block alike.
        validate_transactions(epoch, &reference, &decoded, &self.bump)?;

        // 3. A vote on each user transaction; other kinds are accepted.
        let mut rejects = Vec::new();
        let mut entries: BlockEntries = Vec::with_capacity(decoded.len());
        for (index, transaction) in decoded.into_iter().enumerate() {
            let ConsensusTransactionKind::UserTransactionV2(user) = transaction.get().kind() else {
                entries.push(None);
                continue;
            };
            let claims = Claims::of(user).copied();
            let transaction = transaction
                .into_user_transaction()
                .expect("a user transaction");
            let (entry, vote) = self.vote_transaction(epoch, transaction, &claims);
            if vote.is_err() {
                rejects.push(index as TransactionIndex);
            }
            entries.push(entry);
        }

        // 4. Keep the decoded transactions for the block's commit.
        self.cache.insert(reference, entries);
        Ok(rejects)
    }

    /// `vote_transaction`: the transaction once its validity and signatures
    /// passed, whatever the vote, and the vote.
    fn vote_transaction(
        &mut self,
        epoch: &Arc<EpochState>,
        transaction: Unchecked,
        claims: &CopiedClaims,
    ) -> (Option<VerifiedTransaction>, Result<(), RejectReason>) {
        let (mut valid, failure) =
            checks::validate(vec![transaction], &epoch.context(), &mut self.bump);
        if let Some(e) = failure {
            return (None, Err(e.into()));
        }
        let valid = valid.pop().expect("one transaction validated");
        let (verified, indices) = match self.signatures.verify_one(epoch, valid) {
            Ok(verified) => verified,
            Err(e) => return (None, Err(e.into())),
        };
        let vote = self.vote_verified(epoch, &verified, indices, claims);
        (Some(verified), vote)
    }

    /// The checks after signatures: the alias claim, then
    /// `handle_vote_transaction`, then the immutable-object claim.
    fn vote_verified(
        &mut self,
        epoch: &EpochState,
        transaction: &VerifiedTransaction,
        indices: SignerIndices,
        claims: &CopiedClaims,
    ) -> Result<(), RejectReason> {
        if epoch.config.address_aliases() {
            let indices = indices.as_slice();
            // No signer has an alias: aliases are object state anchovy does
            // not read yet, so each signer's alias version is `None`.
            let matches = if epoch.config.fix_checkpoint_signature_mapping() {
                let claimed = claims
                    .aliases_v2
                    .as_ref()
                    .ok_or(RejectReason::MissingAliasClaim)?;
                claimed.matches(indices.iter().map(|index| (*index, None)))
            } else {
                let claimed = claims
                    .aliases_v1
                    .as_ref()
                    .ok_or(RejectReason::MissingAliasClaim)?;
                let data = transaction.get().0.data();
                let sponsor =
                    (data.gas_data().owner != data.sender()).then_some(data.gas_data().owner);
                claimed.matches(
                    [Some(data.sender()), sponsor]
                        .into_iter()
                        .flatten()
                        .map(|signer| (*signer, None)),
                )
            };
            if !matches {
                return Err(RejectReason::AliasesChanged);
            }
        }

        // `handle_vote_transaction`: an executed transaction is accepted.
        let digest = transaction.get().0.digest().bytes;
        if execution::executed(&self.store, &digest)
            .expect("the store reads")
            .is_some()
        {
            return Ok(());
        }
        checks::inputs_pass(epoch, &self.store, transaction)?;

        if !claims.immutable.is_empty() {
            verify_immutable_object_claims(&self.store, transaction, &claims.immutable)?;
        }
        Ok(())
    }
}

impl Processor for BlockVoter {
    type Input = VoteRequest;
    type Output = ();

    fn process(&mut self, request: VoteRequest) -> Option<()> {
        let VoteRequest {
            epoch,
            block,
            reply,
        } = request;
        let verdict = self.vote(&epoch, block);
        // Consensus may have stopped waiting; nothing to do then.
        let _ = reply.send(verdict);
        None
    }
}

/// A user transaction's claims, each the first of its kind, as the
/// reference's `TransactionWithClaims` accessors find them.
struct Claims<'a> {
    aliases_v2: Option<&'a [(u8, Option<u64>)]>,
    aliases_v1: Option<&'a [(&'a messages::base::SuiAddress, Option<u64>)]>,
    immutable: &'a [ObjectId],
}

impl<'a> Claims<'a> {
    fn of(user: &PlainTransactionWithClaims<'a>) -> Claims<'a> {
        let claims = user.claims();
        Claims {
            aliases_v2: claims.iter().find_map(|c| match c {
                TransactionClaim::AddressAliasesV2(a) => Some(*a),
                _ => None,
            }),
            aliases_v1: claims.iter().find_map(|c| match c {
                TransactionClaim::AddressAliases(a) => Some(*a),
                _ => None,
            }),
            immutable: claims
                .iter()
                .find_map(|c| match c {
                    TransactionClaim::ImmutableInputObjects(ids) => Some(*ids),
                    _ => None,
                })
                .unwrap_or(&[]),
        }
    }
}

impl Claims<'_> {
    /// The claims voting reads, copied out of the consensus transaction so
    /// that its transaction can take over its buffers.
    fn copied(&self) -> CopiedClaims {
        CopiedClaims {
            aliases_v2: self.aliases_v2.map(|a| ShortList::of(a.iter().copied())),
            aliases_v1: self
                .aliases_v1
                .map(|a| ShortList::of(a.iter().map(|(s, v)| (**s, *v)))),
            immutable: self.immutable.to_vec(),
        }
    }
}

/// `Claims`, owned.
struct CopiedClaims {
    aliases_v2: Option<ShortList<(u8, Option<u64>)>>,
    aliases_v1: Option<ShortList<(SuiAddress, Option<u64>)>>,
    immutable: Vec<ObjectId>,
}

/// A list kept only as far as it can match a transaction's signers (a
/// sender and a sponsor): its first two entries and its length.
struct ShortList<T> {
    len: usize,
    items: [Option<T>; 2],
}

impl<T: Copy + PartialEq> ShortList<T> {
    fn of(mut items: impl ExactSizeIterator<Item = T>) -> ShortList<T> {
        ShortList {
            len: items.len(),
            items: [items.next(), items.next()],
        }
    }

    /// Whether the list is exactly `expected`.
    fn matches(&self, expected: impl Iterator<Item = T>) -> bool {
        let mut n = 0;
        for item in expected {
            if self.items.get(n).copied().flatten() != Some(item) {
                return false;
            }
            n += 1;
        }
        n == self.len
    }
}

/// `validate_transactions`: the block's transactions break no rule that
/// makes the block invalid. Checkpoint signatures are not verified: there is
/// no committee to verify them against yet.
fn validate_transactions(
    epoch: &EpochState,
    block: &BlockRef,
    transactions: &[Message<ConsensusTransaction<'static>>],
    bump: &Bump,
) -> Result<(), InvalidBlock> {
    let config = &epoch.config;
    for (index, transaction) in transactions.iter().enumerate() {
        let invalid = |reason: &str| {
            Err(InvalidBlock {
                index,
                reason: reason.to_owned(),
            })
        };
        match transaction.get().kind() {
            ConsensusTransactionKind::CertifiedTransaction(_) => {
                return invalid(
                    "CertifiedTransaction cannot be used when preconsensus locking is disabled",
                );
            }
            ConsensusTransactionKind::CheckpointSignature(_) => {
                return invalid("CheckpointSignature V1 is no longer supported");
            }
            ConsensusTransactionKind::CheckpointSignatureV2(_)
            | ConsensusTransactionKind::EndOfPublish(_)
            | ConsensusTransactionKind::NewJwkFetched(_)
            | ConsensusTransactionKind::CapabilityNotificationV2(_) => {}
            ConsensusTransactionKind::RandomnessDkgMessage(dkg)
            | ConsensusTransactionKind::RandomnessDkgConfirmation(dkg) => {
                if dkg.byte_len() > DKG_MESSAGES_MAX_SIZE {
                    return invalid("DKG message too large");
                }
            }
            ConsensusTransactionKind::CapabilityNotification(_) => {
                return invalid("CapabilityNotification V1 is no longer supported");
            }
            ConsensusTransactionKind::RandomnessStateUpdate { .. } => {
                return invalid("RandomnessStateUpdate is no longer supported");
            }
            ConsensusTransactionKind::UserTransaction(_) => {
                return invalid(
                    "UserTransaction cannot be used when preconsensus locking is disabled",
                );
            }
            ConsensusTransactionKind::UserTransactionV2(user) => {
                let claims = Claims::of(user);
                if config.address_aliases() {
                    let has_aliases = if config.fix_checkpoint_signature_mapping() {
                        claims.aliases_v2.is_some()
                    } else {
                        claims.aliases_v1.is_some()
                    };
                    if !has_aliases {
                        return invalid("UserTransactionV2 must contain an aliases claim");
                    }
                }
                if let Some(aliases) = claims.aliases_v2 {
                    let signatures = user.transaction().0.tx_signatures().len();
                    if aliases.iter().any(|(i, _)| usize::from(*i) >= signatures) {
                        return invalid("UserTransactionV2 alias names a signature it lacks");
                    }
                }
                // Proposing a transaction from outside its allowed proposers
                // is byzantine, so it invalidates the block.
                if config.allowed_proposers()
                    && !is_allowed_proposer(
                        user.transaction().0.data().expiration(),
                        block.author,
                        epoch.epoch,
                    )
                {
                    return invalid("proposer not allowed");
                }
            }
            ConsensusTransactionKind::ExecutionTimeObservation(observation) => {
                if observation.estimate_count() > config.max_programmable_tx_commands() as usize {
                    return invalid("ExecutionTimeObservation contains too many estimates");
                }
            }
            ConsensusTransactionKind::UpdateTransactionDenyConfig(update) => {
                if !config.share_transaction_deny_config_in_consensus() {
                    return invalid(
                        "UpdateTransactionDenyConfig is not supported by the protocol version",
                    );
                }
                if let Some(rules) = &update.rules
                    && let Err(e) = check_share_limits(rules, bump)
                {
                    return invalid(&e);
                }
            }
        }
    }
    Ok(())
}

/// `TransactionExpiration::is_allowed_proposer`: an allowed-proposer set for
/// another epoch is ignored. The set is strictly increasing, so the search
/// stops once it passes `proposer`; an unsorted set (which validity checks
/// reject) reads as a miss.
fn is_allowed_proposer(expiration: &TransactionExpiration<'_>, proposer: u32, epoch: u64) -> bool {
    match expiration {
        TransactionExpiration::Validity(_, Some(allowed)) if allowed.epoch == epoch => allowed
            .proposers
            .iter()
            .map(|p| p.get())
            .find(|p| *p >= proposer)
            .is_some_and(|p| p == proposer),
        _ => true,
    }
}

/// `TransactionDenyRules::check_share_limits`, over the rules as the
/// reference holds them: its lists are sets, so repeats count once, in the
/// entry count and in the encoded size alike.
fn check_share_limits(rules: &TransactionDenyRules<'_>, bump: &Bump) -> Result<(), String> {
    fn distinct<'b, T: Ord + Copy>(items: &[T], bump: &'b Bump) -> containers::Vec<'b, T> {
        let mut sorted = containers::Vec::with_capacity_in(items.len(), bump);
        sorted.extend_from_slice(items);
        sorted.sort_unstable();
        sorted.dedup();
        sorted
    }
    fn uleb128_len(n: usize) -> usize {
        let bits = usize::BITS - n.max(1).leading_zeros();
        bits.div_ceil(7) as usize
    }
    let objects = distinct(rules.object_deny_list, bump);
    let packages = distinct(rules.package_deny_list, bump);
    let addresses = distinct(rules.address_deny_list, bump);
    let providers = distinct(rules.zklogin_disabled_providers, bump);

    let entries = objects.len() + packages.len() + addresses.len() + providers.len();
    if entries > MAX_SHARE_ENTRIES {
        return Err(format!(
            "rules entry count {entries} exceeds limit ({MAX_SHARE_ENTRIES})"
        ));
    }
    if let Some(p) = providers
        .iter()
        .find(|p| p.len() > MAX_ZKLOGIN_PROVIDER_LENGTH)
    {
        return Err(format!(
            "zklogin provider name too long: {} bytes (max {MAX_ZKLOGIN_PROVIDER_LENGTH})",
            p.len()
        ));
    }
    let ids = |n: usize| uleb128_len(n) + 32 * n;
    let size = ids(objects.len())
        + ids(packages.len())
        + ids(addresses.len())
        + 7
        + uleb128_len(providers.len())
        + providers
            .iter()
            .map(|p| uleb128_len(p.len()) + p.len())
            .sum::<usize>();
    if size > MAX_SHARE_SERIALIZED_BYTES {
        return Err(format!(
            "rules serialized size {size} bytes exceeds limit ({MAX_SHARE_SERIALIZED_BYTES})"
        ));
    }
    Ok(())
}

/// `verify_immutable_object_claims`: the claimed ids are exactly the owned
/// inputs that are immutable, each input still live at its given reference.
fn verify_immutable_object_claims(
    store: &store::Store,
    transaction: &VerifiedTransaction,
    claimed: &[ObjectId],
) -> Result<(), RejectReason> {
    let inputs = input_objects(transaction.get().0.data())?;
    let owned: Vec<_> = inputs
        .iter()
        .filter_map(|kind| match kind {
            InputKind::ImmOrOwned(r) => Some(*r),
            _ => None,
        })
        .collect();
    if let Some(id) = claimed
        .iter()
        .find(|id| !owned.iter().any(|r| r.id == **id))
    {
        return Err(RejectReason::ImmutableClaim(format!(
            "{id:?} is not an owned input"
        )));
    }
    let mut immutable = Vec::new();
    for input in &owned {
        let Some(object) = store.live_object(&input.id).expect("the store reads") else {
            return Err(RejectReason::ImmutableClaim(format!(
                "{:?} not found",
                input.id
            )));
        };
        let object = object.get();
        if object.version() != input.version.get() || object.digest() != input.digest {
            return Err(RejectReason::ImmutableClaim(format!(
                "{:?} is no longer at its given version",
                input.id
            )));
        }
        if matches!(object.owner, Owner::Immutable) {
            immutable.push(input.id);
        }
    }
    let mut claimed: Vec<_> = claimed.to_vec();
    claimed.sort_unstable();
    claimed.dedup();
    immutable.sort_unstable();
    immutable.dedup();
    if claimed != immutable {
        return Err(RejectReason::ImmutableClaim(
            "the claimed immutable objects are not the immutable inputs".to_owned(),
        ));
    }
    Ok(())
}
