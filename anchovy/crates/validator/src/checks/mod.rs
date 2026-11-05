// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The checks a transaction's type records, and the only code that records
//! them. A transaction reaches [`Valid`] only through [`validate`], and
//! [`Verified`] only through [`SignatureChecks::verify`] (or, committed by
//! consensus, [`sequenced_by_consensus`] and [`certified`]): their witnesses
//! cannot be made anywhere else. What a `Message<Transaction<'static, Verified>>`
//! promises rests on this module and its signature cache.
//!
//! ```compile_fail,E0423
//! use std::marker::PhantomData;
//! use validator::checks::{Verified, Witness};
//! let _ = Witness::<Verified>(PhantomData);
//! ```

mod signature_cache;

use std::marker::PhantomData;
use std::sync::Arc;

use containers::Bump;
use messages::Message;
use messages::base::{Hashed, ObjectId};
use messages::object::Object;
use messages::transaction::{Attested, DigestPending, HasDigest, Transaction, TxState};
use validation::{sender_signed, verify};

pub use signature_cache::GENERATION;
use signature_cache::SignatureCache;
use validation::verify::SignerIndices;

use crate::epoch::EpochState;

/// Passed everything the reference checks before signatures
/// (`SenderSignedData::validity_check`); the digest is computed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Valid;

/// Valid, and its signatures verified. What needs verified signatures
/// takes this, and nothing less will do:
///
/// ```compile_fail,E0308
/// use validator::checks::{ValidTransaction, VerifiedTransaction};
/// fn submit_to_consensus(_: VerifiedTransaction) {}
/// fn skip_verification(transaction: ValidTransaction) {
///     submit_to_consensus(transaction);
/// }
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Verified;

/// What relabels a transaction into state `S`. Its field is private: only
/// this module makes one, after the check `S` stands for.
pub struct Witness<S>(PhantomData<S>);

impl TxState for Valid {}
impl HasDigest for Valid {}
impl Attested for Valid {
    type Witness = Witness<Valid>;
}

impl TxState for Verified {}
impl HasDigest for Verified {}
impl Attested for Verified {
    type Witness = Witness<Verified>;
}

pub type Unchecked = Message<Transaction<'static, DigestPending>>;
pub type ValidTransaction = Message<Transaction<'static, Valid>>;
pub type VerifiedTransaction = Message<Transaction<'static, Verified>>;

/// Validates each transaction in order until one fails, then hashes those
/// before it: they come back `Valid`, with the failure. `bump` is reset for
/// every transaction.
pub fn validate(
    mut transactions: Vec<Unchecked>,
    context: &validation::Context<'_>,
    bump: &Bump,
) -> (Vec<ValidTransaction>, Option<validation::Error>) {
    let failed = transactions
        .iter()
        .enumerate()
        .find_map(|(i, transaction)| {
            sender_signed::validity_check(&transaction.get().0, context, bump)
                .err()
                .map(|e| (i, e))
        });
    let failure = failed.map(|(i, e)| {
        transactions.truncate(i);
        e
    });
    let hashed = Message::with_digests(transactions);
    (Message::relabel_all(hashed, &Witness(PhantomData)), failure)
}

/// `validate` for one transaction.
pub fn validate_one(
    transaction: Unchecked,
    context: &validation::Context<'_>,
    bump: &Bump,
) -> Result<ValidTransaction, validation::Error> {
    sender_signed::validity_check(&transaction.get().0, context, bump)?;
    Ok(transaction.with_digest().relabel(&Witness(PhantomData)))
}

/// `transaction`, from a checkpoint a quorum certified: verified as the
/// quorum verified it. Its signatures are not checked again (R3.6 of
/// `docs/state_sync_reference.md`).
pub fn certified(transaction: Unchecked) -> VerifiedTransaction {
    transaction.with_digest().relabel(&Witness(PhantomData))
}

/// Signature verification in one epoch, remembering what verified.
pub struct SignatureChecks {
    epoch: Arc<EpochState>,
    cache: SignatureCache,
}

impl SignatureChecks {
    /// In `epoch`, with a cache of `2 * generation` entries.
    pub fn new(epoch: Arc<EpochState>, generation: usize) -> SignatureChecks {
        SignatureChecks {
            cache: SignatureCache::new(epoch.params.epoch, generation),
            epoch,
        }
    }

    /// Signature cache hits and misses so far.
    pub fn cache_stats(&self) -> (u64, u64) {
        self.cache.stats()
    }

    /// Verifies each transaction's signatures, unless they verified before:
    /// all `Verified`, or the first failure. Each must have been validated
    /// in the same epoch.
    pub fn verify(
        &mut self,
        transactions: Vec<ValidTransaction>,
        bump: &Bump,
    ) -> Result<Vec<VerifiedTransaction>, validation::Error> {
        for transaction in &transactions {
            self.verify_signatures(transaction, bump)?;
        }
        Ok(Message::relabel_all(transactions, &Witness(PhantomData)))
    }

    /// `verify` for one transaction, also giving each required signer's
    /// signature index, with no aliases: what a consensus transaction's alias
    /// claim must name.
    pub fn verify_one(
        &mut self,
        transaction: ValidTransaction,
        bump: &Bump,
    ) -> Result<(VerifiedTransaction, SignerIndices), validation::Error> {
        let indices = self.verify_signatures(&transaction, bump)?;
        Ok((transaction.relabel(&Witness(PhantomData)), indices))
    }

    /// `verify_one` for each transaction, verifying their simple Ed25519
    /// signatures in one batch, which costs less than verifying each. Each
    /// transaction verifies exactly when it would alone: if the batch fails,
    /// each transaction's Ed25519 signatures are verified on their own.
    pub fn verify_batch<'b>(
        &mut self,
        transactions: containers::Vec<'b, ValidTransaction>,
        bump: &'b Bump,
    ) -> containers::Vec<'b, Result<(VerifiedTransaction, SignerIndices), validation::Error>> {
        enum Prepared {
            Cached(SignerIndices),
            /// Verified but for its Ed25519 checks, `deferred[checks]`.
            Pending {
                key: Hashed,
                indices: SignerIndices,
                checks: std::ops::Range<usize>,
            },
            Failed(validation::Error),
        }
        let epoch = self.epoch.clone();
        // 1. Cache hits stand; the rest are checked, but for their Ed25519
        //    signatures' verification.
        let mut deferred = containers::Vec::new_in(bump);
        let mut prepared = containers::Vec::with_capacity_in(transactions.len(), bump);
        prepared.extend(transactions.iter().map(|transaction| {
            let (key, cached) = self.cache.lookup(transaction);
            if let Some(indices) = cached {
                return Prepared::Cached(indices);
            }
            let signed = &transaction.get().0;
            let start = deferred.len();
            let prepared =
                sender_signed::deserialization_checks(signed, bump).and_then(|(signatures, _)| {
                    verify::verify_signatures_deferring_ed25519(
                        signed,
                        signatures,
                        epoch.params.epoch,
                        &epoch.verifier,
                        &[],
                        bump,
                        &mut deferred,
                    )
                });
            match prepared {
                Ok(indices) => Prepared::Pending {
                    key,
                    indices,
                    checks: start..deferred.len(),
                },
                Err(e) => {
                    deferred.truncate(start);
                    Prepared::Failed(e)
                }
            }
        }));

        // 2. One batch; if it fails, each transaction's checks alone.
        let batch_verified = verify::verify_ed25519_batch(&deferred);

        // 3. The verified enter the cache. `prepared` has an entry per
        // transaction.
        let mut verified = containers::Vec::with_capacity_in(transactions.len(), bump);
        verified.extend(
            transactions
                .into_iter()
                .zip(prepared)
                .map(|(transaction, prepared)| {
                    let indices = match prepared {
                        Prepared::Cached(indices) => indices,
                        Prepared::Pending {
                            key,
                            indices,
                            checks,
                        } => {
                            let verified = batch_verified
                                || deferred[checks].iter().all(verify::verify_ed25519);
                            if !verified {
                                return Err(reference_error(&epoch, &transaction, None, bump));
                            }
                            self.cache.insert(key, indices);
                            indices
                        }
                        Prepared::Failed(e) => {
                            return Err(reference_error(&epoch, &transaction, Some(e), bump));
                        }
                    };
                    Ok((transaction.relabel(&Witness(PhantomData)), indices))
                }),
        );
        verified
    }

    fn verify_signatures(
        &mut self,
        transaction: &ValidTransaction,
        bump: &Bump,
    ) -> Result<SignerIndices, validation::Error> {
        let epoch = &self.epoch;
        self.cache.verify(transaction, |signed| {
            let (signatures, _) = sender_signed::deserialization_checks(signed, bump)?;
            // No aliases: they are object state, which does not exist yet.
            verify::verify_signatures(
                signed,
                signatures,
                epoch.params.epoch,
                &epoch.verifier,
                &[],
                bump,
            )
        })
    }
}

/// A transaction consensus committed that this validator did not verify:
/// its signatures are taken as verified, as the reference's
/// `VerifiedExecutableTransaction::new_from_consensus` takes them. A quorum
/// accepted it, so at least one honest validator verified it. Only the commit
/// handler may call this, and only for a transaction it took from a commit.
pub fn sequenced_by_consensus(transaction: Unchecked) -> VerifiedTransaction {
    transaction.with_digest().relabel(&Witness(PhantomData))
}

/// The error `verify::verify_signatures` gives a transaction whose
/// signatures failed when its Ed25519 checks were deferred: deferring
/// checks them in another order, the reference's is this one. `deferred` is
/// the deferred checks' error, if they gave one.
fn reference_error(
    epoch: &EpochState,
    transaction: &ValidTransaction,
    deferred: Option<validation::Error>,
    bump: &Bump,
) -> validation::Error {
    let signed = &transaction.get().0;
    let error = sender_signed::deserialization_checks(signed, bump)
        .and_then(|(signatures, _)| {
            verify::verify_signatures(
                signed,
                signatures,
                epoch.params.epoch,
                &epoch.verifier,
                &[],
                bump,
            )
        })
        .err();
    // Both fail alike; an error stands should they not.
    error.or(deferred).unwrap_or_else(|| {
        validation::Error::new(validation::SuiErrorKind::InvalidSignature {
            error: "Fail to verify user sig Invalid signature was given to the function".to_owned(),
        })
    })
}

/// Checks a verified transaction's inputs against the store's live
/// objects (`validation::inputs::check`), keeping the transaction: voting
/// checks the inputs of a transaction it caches either way.
pub fn inputs_pass(
    epoch: &EpochState,
    store: &store::Store,
    transaction: &VerifiedTransaction,
    bump: &Bump,
) -> Result<(), validation::Error> {
    validation::inputs::check(
        &transaction.get().0,
        &epoch.context(),
        &StoreObjects(store),
        bump,
    )
}

struct StoreObjects<'a>(&'a store::Store);

/// A store that cannot be read leaves nothing to check against.
impl validation::inputs::Objects for StoreObjects<'_> {
    fn live(&self, id: &ObjectId) -> Option<Message<Object<'static>>> {
        self.0.live_object(id).expect("the store reads")
    }

    fn at(&self, id: &ObjectId, version: u64) -> Option<Message<Object<'static>>> {
        self.0.object(id, version).expect("the store reads")
    }

    fn live_ref(&self, id: &ObjectId) -> Option<(u64, messages::base::ObjectDigest)> {
        let live = self.0.live(id).expect("the store reads")?;
        Some((live.version, live.digest))
    }
}
