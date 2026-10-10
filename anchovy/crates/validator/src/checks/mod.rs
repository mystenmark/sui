// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The checks a transaction's type records, and the only code that records
//! them. A transaction reaches [`Valid`] only through [`validate`],
//! [`Verified`] only through [`SignatureChecks::verify`], and
//! [`InputsChecked`] only through [`check_inputs`]: their witnesses cannot
//! be made anywhere else. What a `Message<Transaction<'static, Verified>>`
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
use messages::base::ObjectId;
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

/// Verified, and its input objects passed the stateful checks against the
/// store (`validation::inputs::check`): what execution takes.
///
/// ```compile_fail,E0308
/// use validator::checks::{InputsCheckedTransaction, VerifiedTransaction};
/// fn execute(_: InputsCheckedTransaction) {}
/// fn skip_input_checks(transaction: VerifiedTransaction) {
///     execute(transaction);
/// }
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InputsChecked;

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

impl TxState for InputsChecked {}
impl HasDigest for InputsChecked {}
impl Attested for InputsChecked {
    type Witness = Witness<InputsChecked>;
}

pub type Unchecked = Message<Transaction<'static, DigestPending>>;
pub type ValidTransaction = Message<Transaction<'static, Valid>>;
pub type VerifiedTransaction = Message<Transaction<'static, Verified>>;
pub type InputsCheckedTransaction = Message<Transaction<'static, InputsChecked>>;

/// Validates each transaction in order until one fails, then hashes those
/// before it: they come back `Valid`, with the failure. `bump` is reset for
/// every transaction.
pub fn validate(
    mut transactions: Vec<Unchecked>,
    context: &validation::Context<'_>,
    bump: &mut Bump,
) -> (Vec<ValidTransaction>, Option<validation::Error>) {
    let failed = transactions
        .iter()
        .enumerate()
        .find_map(|(i, transaction)| {
            bump.reset();
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

/// Signature verification, remembering what verified in the current epoch.
pub struct SignatureChecks {
    cache: SignatureCache,
    bump: Bump,
}

/// Enough for the temporaries of nearly every transaction.
const ARENA_BYTES: usize = 64 * 1024;

impl SignatureChecks {
    /// With a cache of `2 * generation` entries.
    pub fn new(generation: usize) -> SignatureChecks {
        SignatureChecks {
            cache: SignatureCache::new(generation),
            bump: Bump::with_capacity(ARENA_BYTES),
        }
    }

    /// Signature cache hits and misses so far.
    pub fn cache_stats(&self) -> (u64, u64) {
        self.cache.stats()
    }

    /// Verifies each transaction's signatures in `epoch`, which must be the
    /// one it was validated in, unless they verified before there: all
    /// `Verified`, or the first failure.
    pub fn verify(
        &mut self,
        epoch: &Arc<EpochState>,
        transactions: Vec<ValidTransaction>,
    ) -> Result<Vec<VerifiedTransaction>, validation::Error> {
        for transaction in &transactions {
            self.verify_signatures(epoch, transaction)?;
        }
        Ok(Message::relabel_all(transactions, &Witness(PhantomData)))
    }

    /// `verify` for one transaction, also giving each required signer's
    /// signature index, with no aliases: what a consensus transaction's alias
    /// claim must name.
    pub fn verify_one(
        &mut self,
        epoch: &Arc<EpochState>,
        transaction: ValidTransaction,
    ) -> Result<(VerifiedTransaction, SignerIndices), validation::Error> {
        let indices = self.verify_signatures(epoch, &transaction)?;
        Ok((transaction.relabel(&Witness(PhantomData)), indices))
    }

    fn verify_signatures(
        &mut self,
        epoch: &Arc<EpochState>,
        transaction: &ValidTransaction,
    ) -> Result<SignerIndices, validation::Error> {
        let bump = &mut self.bump;
        self.cache.verify(epoch, transaction, |signed| {
            bump.reset();
            let (signatures, _) = sender_signed::deserialization_checks(signed, bump)?;
            // No aliases: they are object state, which does not exist yet.
            verify::verify_signatures(signed, signatures, epoch.epoch, &epoch.verifier, &[], bump)
        })
    }
}

/// Checks a verified transaction's inputs against the store's live
/// objects. Nothing is locked: until voting exists, another transaction
/// may consume an input before this one executes.
pub fn check_inputs(
    epoch: &EpochState,
    store: &store::Store,
    transaction: VerifiedTransaction,
) -> Result<InputsCheckedTransaction, validation::Error> {
    validation::inputs::check(&transaction.get().0, &epoch.context(), &StoreObjects(store))?;
    Ok(transaction.relabel(&Witness(PhantomData)))
}

/// A transaction consensus committed that this validator did not verify:
/// its signatures are taken as verified, as the reference's
/// `VerifiedExecutableTransaction::new_from_consensus` takes them. A quorum
/// accepted it, so at least one honest validator verified it. Only the commit
/// handler may call this, and only for a transaction it took from a commit.
pub fn sequenced_by_consensus(transaction: Unchecked) -> VerifiedTransaction {
    transaction.with_digest().relabel(&Witness(PhantomData))
}

/// `check_inputs`, keeping the transaction: voting checks the inputs of a
/// transaction it caches either way.
pub fn inputs_pass(
    epoch: &EpochState,
    store: &store::Store,
    transaction: &VerifiedTransaction,
) -> Result<(), validation::Error> {
    validation::inputs::check(&transaction.get().0, &epoch.context(), &StoreObjects(store))
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
}
