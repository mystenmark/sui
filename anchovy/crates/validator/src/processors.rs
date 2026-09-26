// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The processors transaction work runs on, off the RPC runtime: validation,
//! then signature verification. A request passes from one to the other and
//! the last replies; for now both run on one worker thread.

use std::sync::Arc;

use containers::Bump;
use messages::Message;
use messages::transaction::{DigestPending, DigestReady, Transaction};
use tokio::sync::oneshot;
use validation::{sender_signed, verify};
use workqueue::{Inbox, Processor, PushError, Queue, Worker, WorkerHandle};

use crate::epoch::EpochState;
use crate::signature_cache::{self, SignatureCache};

/// Where a request's verdict goes.
pub type Reply = oneshot::Sender<Result<Validated, Rejected>>;

/// A request's decoded transactions to validate. One item per request, as
/// the handoff costs far more than validating a transaction does.
pub struct ValidateTransactions {
    pub transactions: Vec<Message<Transaction<'static, DigestPending>>>,
    pub reply: Reply,
}

/// A request's validated transactions, with their digests, whose signatures
/// are next. Only `TransactionValidator` makes one: verification (and so the
/// signature cache) relies on validation having passed, as
/// `verify_signatures` does not repeat it.
pub struct VerifySignatures {
    /// The epoch the transactions were validated in, which they are verified
    /// in too.
    epoch: Arc<EpochState>,
    transactions: Vec<Message<Transaction<'static, DigestReady>>>,
    /// Why the transaction after these failed validation, if one did. The
    /// reference checks each transaction's signatures before validating the
    /// next, so a bad signature among these is the request's error first.
    then: Option<validation::Error>,
    reply: Reply,
}

/// Transactions that passed validation and signature verification, with
/// their digests. A struct, not an alias: a future holding
/// `Message<Transaction<'static, _>>` across an `await` fails `Send`
/// inference, which erases the `'static`.
pub struct Validated(pub Vec<Message<Transaction<'static, DigestReady>>>);

/// Why a request was refused.
#[derive(Debug)]
pub enum Rejected {
    /// The first of its transactions to fail, in order, failed this.
    Invalid(validation::Error),
    /// A queue between processors was full.
    Overloaded,
    /// A queue between processors was closed.
    ShuttingDown,
}

/// Enough for the temporaries of nearly every transaction.
const ARENA_BYTES: usize = 64 * 1024;

/// Runs `SenderSignedData::validity_check`, everything the reference checks
/// before signatures, then computes the digests of the transactions that
/// pass and hands them to signature verification. The arena is reused for
/// every transaction.
pub struct TransactionValidator {
    epoch: Arc<EpochState>,
    bump: Bump,
    signatures: Queue<VerifySignatures>,
}

impl TransactionValidator {
    pub fn new(
        epoch: Arc<EpochState>,
        signatures: Queue<VerifySignatures>,
    ) -> TransactionValidator {
        TransactionValidator {
            epoch,
            bump: Bump::with_capacity(ARENA_BYTES),
            signatures,
        }
    }
}

impl Processor<ValidateTransactions> for TransactionValidator {
    fn process(&mut self, item: ValidateTransactions) {
        let ValidateTransactions {
            mut transactions,
            reply,
        } = item;
        let context = self.epoch.context();
        let failed = transactions
            .iter()
            .enumerate()
            .find_map(|(i, transaction)| {
                // Reset first, so an item that panicked leaves nothing behind.
                self.bump.reset();
                sender_signed::validity_check(&transaction.get().0, &context, &self.bump)
                    .err()
                    .map(|e| (i, e))
            });
        let then = match failed {
            Some((0, e)) => {
                // The handler may have given up; nothing to do then.
                let _ = reply.send(Err(Rejected::Invalid(e)));
                return;
            }
            // Those before it go on, for their signatures to be checked first.
            Some((i, e)) => {
                transactions.truncate(i);
                Some(e)
            }
            None => None,
        };
        // Hashing is left until here, off the RPC runtime, and until the
        // cheap checks have passed.
        let next = VerifySignatures {
            epoch: self.epoch.clone(),
            transactions: Message::with_digests(transactions),
            then,
            reply,
        };
        if let Err(e) = self.signatures.try_push(next) {
            let (next, why) = match e {
                PushError::Full(next) => (next, Rejected::Overloaded),
                PushError::Closed(next) => (next, Rejected::ShuttingDown),
            };
            let _ = next.reply.send(Err(why));
        }
    }
}

/// Verifies each transaction's signatures, unless they verified before in
/// the same epoch, then replies. The signatures are parsed again, into this
/// processor's arena: validation's is reset for every transaction.
pub struct SignatureVerifier {
    bump: Bump,
    cache: SignatureCache,
}

impl SignatureVerifier {
    pub fn new(epoch: Arc<EpochState>) -> SignatureVerifier {
        SignatureVerifier::with_cache(epoch, signature_cache::GENERATION)
    }

    /// With a cache of `2 * generation` entries.
    pub fn with_cache(epoch: Arc<EpochState>, generation: usize) -> SignatureVerifier {
        SignatureVerifier {
            cache: SignatureCache::new(epoch, generation),
            bump: Bump::with_capacity(ARENA_BYTES),
        }
    }

    /// Signature cache hits and misses so far.
    pub fn cache_stats(&self) -> (u64, u64) {
        self.cache.stats()
    }
}

impl Processor<VerifySignatures> for SignatureVerifier {
    fn process(&mut self, item: VerifySignatures) {
        let (epoch, bump, cache) = (&item.epoch, &mut self.bump, &mut self.cache);
        let verified = item.transactions.iter().try_for_each(|transaction| {
            cache.verify(epoch, transaction, |signed| {
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
            })
        });
        let result = match (verified, item.then) {
            (Err(e), _) | (Ok(()), Some(e)) => Err(Rejected::Invalid(e)),
            (Ok(()), None) => Ok(Validated(item.transactions)),
        };
        let _ = item.reply.send(result);
    }
}

/// The processors, and the queue that feeds them.
pub struct Processors {
    pub transactions: Queue<ValidateTransactions>,
    // Dropped last: stops and joins the thread.
    _worker: WorkerHandle,
}

/// Queued requests beyond which submissions are refused.
pub const VALIDATION_QUEUE: usize = 4096;

/// Validated requests awaiting signature verification beyond which requests
/// are refused.
pub const SIGNATURE_QUEUE: usize = 4096;

impl Processors {
    /// Validation and signature verification, on one thread.
    pub fn start(epoch: &Arc<EpochState>, validation_queue: usize) -> Processors {
        let (transactions, validation) = workqueue::queue(validation_queue);
        let worker = Processors::worker(epoch, validation, SIGNATURE_QUEUE).spawn();
        Processors {
            transactions,
            _worker: worker,
        }
    }

    /// A worker running both processors, validation draining `validation`.
    pub fn worker(
        epoch: &Arc<EpochState>,
        validation: Inbox<ValidateTransactions>,
        signature_queue: usize,
    ) -> Worker {
        let (signatures, verification) = workqueue::queue(signature_queue);
        let validator = {
            let epoch = epoch.clone();
            move || TransactionValidator::new(epoch, signatures)
        };
        let verifier = {
            let epoch = epoch.clone();
            move || SignatureVerifier::new(epoch)
        };
        Worker::new("transactions")
            .run(validation, validator)
            .run(verification, verifier)
    }
}
