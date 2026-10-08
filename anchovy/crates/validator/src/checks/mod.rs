// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The checks a transaction's type records, and the only code that records
//! them. A transaction reaches [`Valid`] only through [`validate`], and
//! [`Verified`] only through [`SignatureChecks::verify`]: their witnesses
//! cannot be made anywhere else. What a `Message<Transaction<'static,
//! Verified>>` promises rests on this module and its signature cache.
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
use messages::transaction::{Attested, DigestPending, HasDigest, Transaction, TxState};
use validation::{sender_signed, verify};

pub use signature_cache::GENERATION;
use signature_cache::SignatureCache;

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
        let bump = &mut self.bump;
        for transaction in &transactions {
            self.cache.verify(epoch, transaction, |signed| {
                bump.reset();
                let (signatures, _) = sender_signed::deserialization_checks(signed, bump)?;
                // No aliases: they are object state, which does not exist yet.
                verify::verify_signatures(
                    signed,
                    signatures,
                    epoch.epoch,
                    &epoch.verifier,
                    &[],
                    bump,
                )
            })?;
        }
        Ok(Message::relabel_all(transactions, &Witness(PhantomData)))
    }
}
