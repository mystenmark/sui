// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The processors transaction work runs on, off the RPC runtime. A request
//! passes through them, each taking it in one state and emitting it in the
//! next: `Request<DigestPending>` → [`TransactionValidator`] →
//! `Request<Valid>` → [`SignatureVerifier`] → `Request<Verified>` →
//! [`answer`]. For now all run on one worker thread.

use std::sync::Arc;

use containers::Bump;
use messages::Message;
use messages::transaction::{DigestPending, Transaction, TxState};
use tokio::sync::oneshot;
use workqueue::{Inbox, Processor, Queue, Refusal, Refuse, Worker, WorkerHandle};

use crate::checks::{self, SignatureChecks, Valid, Verified};
use crate::epoch::EpochState;

/// Where a request's verdict goes.
pub type Reply = oneshot::Sender<Result<Validated, Rejected>>;

/// A submission's transactions, in state `S`, on their way through the
/// processors. One per request, as a handoff costs far more than checking
/// a transaction does.
pub struct Request<S: TxState> {
    /// The epoch the request is checked in, every stage alike.
    epoch: Arc<EpochState>,
    transactions: Vec<Message<Transaction<'static, S>>>,
    /// Why the transaction after these failed validation, if one did. The
    /// reference checks each transaction's signatures before validating the
    /// next, so a bad signature among these is the request's error first.
    then: Option<validation::Error>,
    reply: Reply,
}

impl Request<DigestPending> {
    /// Freshly decoded transactions.
    pub fn new(
        epoch: Arc<EpochState>,
        transactions: Vec<Message<Transaction<'static, DigestPending>>>,
        reply: Reply,
    ) -> Request<DigestPending> {
        Request {
            epoch,
            transactions,
            then: None,
            reply,
        }
    }
}

impl<S: TxState> Request<S> {
    fn reject(self, why: Rejected) {
        // The handler may have given up; nothing to do then.
        let _ = self.reply.send(Err(why));
    }
}

/// A full queue between processors refuses the request.
impl<S: TxState> Refuse for Request<S> {
    fn refuse(self, why: Refusal) {
        self.reject(match why {
            Refusal::Full => Rejected::Overloaded,
            Refusal::Closed => Rejected::ShuttingDown,
        });
    }
}

/// Transactions whose signatures verified, with their digests. A struct,
/// not an alias: a future holding `Message<Transaction<'static, _>>` across
/// an `await` fails `Send` inference, which erases the `'static`.
pub struct Validated(pub Vec<Message<Transaction<'static, Verified>>>);

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

/// Validates a request's transactions (`checks::validate`) and passes the
/// valid ones on; a request whose first transaction fails ends here.
pub struct TransactionValidator {
    bump: Bump,
}

impl TransactionValidator {
    pub fn new() -> TransactionValidator {
        TransactionValidator {
            bump: Bump::with_capacity(ARENA_BYTES),
        }
    }
}

impl Default for TransactionValidator {
    fn default() -> TransactionValidator {
        TransactionValidator::new()
    }
}

impl Processor for TransactionValidator {
    type Input = Request<DigestPending>;
    type Output = Request<Valid>;

    fn process(&mut self, request: Request<DigestPending>) -> Option<Request<Valid>> {
        let Request {
            epoch,
            transactions,
            then: _,
            reply,
        } = request;
        let (valid, failure) = checks::validate(transactions, &epoch.context(), &mut self.bump);
        let request = Request {
            epoch,
            transactions: valid,
            then: failure,
            reply,
        };
        if request.transactions.is_empty()
            && let Some(e) = request.then
        {
            let _ = request.reply.send(Err(Rejected::Invalid(e)));
            return None;
        }
        Some(request)
    }
}

/// Verifies a request's signatures (`SignatureChecks::verify`) and passes
/// it on; a request with a bad signature ends here.
pub struct SignatureVerifier {
    checks: SignatureChecks,
}

impl SignatureVerifier {
    pub fn new() -> SignatureVerifier {
        SignatureVerifier::with_cache(checks::GENERATION)
    }

    /// With a signature cache of `2 * generation` entries.
    pub fn with_cache(generation: usize) -> SignatureVerifier {
        SignatureVerifier {
            checks: SignatureChecks::new(generation),
        }
    }

    /// Signature cache hits and misses so far.
    pub fn cache_stats(&self) -> (u64, u64) {
        self.checks.cache_stats()
    }
}

impl Default for SignatureVerifier {
    fn default() -> SignatureVerifier {
        SignatureVerifier::new()
    }
}

impl Processor for SignatureVerifier {
    type Input = Request<Valid>;
    type Output = Request<Verified>;

    fn process(&mut self, request: Request<Valid>) -> Option<Request<Verified>> {
        let Request {
            epoch,
            transactions,
            then,
            reply,
        } = request;
        match self.checks.verify(&epoch, transactions) {
            Ok(verified) => Some(Request {
                epoch,
                transactions: verified,
                then,
                reply,
            }),
            Err(e) => {
                let _ = reply.send(Err(Rejected::Invalid(e)));
                None
            }
        }
    }
}

/// The end of the pipeline: answers the handler, with the verified
/// transactions, or with the validation failure that followed them.
pub fn answer(request: Request<Verified>) {
    let result = match request.then {
        Some(e) => Err(Rejected::Invalid(e)),
        None => Ok(Validated(request.transactions)),
    };
    let _ = request.reply.send(result);
}

/// The processors, and the queue that feeds them.
pub struct Processors {
    pub transactions: Queue<Request<DigestPending>>,
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
    pub fn start(validation_queue: usize) -> Processors {
        let (transactions, validation) = workqueue::queue(validation_queue);
        let worker = Processors::worker(validation, SIGNATURE_QUEUE).spawn();
        Processors {
            transactions,
            _worker: worker,
        }
    }

    /// A worker running the pipeline, validation draining `validation`.
    pub fn worker(validation: Inbox<Request<DigestPending>>, signature_queue: usize) -> Worker {
        let (signatures, verification) = workqueue::queue(signature_queue);
        Worker::new("transactions")
            .run(validation, TransactionValidator::new, signatures)
            .run(verification, SignatureVerifier::new, answer)
    }
}
