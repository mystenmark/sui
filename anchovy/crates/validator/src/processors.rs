// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The processors a submission runs on, off the RPC runtime. A request
//! passes through them, each taking it in one state and emitting it in the
//! next: `Request<DigestPending>` → [`TransactionValidator`] →
//! `Request<Valid>` → [`SignatureVerifier`] → `Request<Verified>` →
//! `SubmitChecker` (`submit`), which answers. A transaction failing
//! validation or signature verification fails its request, as in the
//! reference; one failing the input checks fails only its own result.

use std::sync::Arc;

use containers::Bump;
use messages::Message;
use messages::transaction::{DigestPending, Transaction, TxState};
use tokio::sync::oneshot;
use workqueue::{Processor, Refusal, Refuse};

use crate::checks::{self, SignatureChecks, Valid, Verified};
use crate::epoch::EpochState;

/// Where a request's verdict goes: a result per transaction, in order, or
/// why the request as a whole was refused.
pub type Reply = oneshot::Sender<Result<Vec<Outcome>, Rejected>>;

/// A submission's transactions, in state `S`, on their way through the
/// processors. One per request, as a handoff costs far more than checking
/// a transaction does.
pub struct Request<S: TxState> {
    pub(crate) transactions: Vec<Message<Transaction<'static, S>>>,
    /// Why the transaction after these failed validation, if one did. The
    /// reference checks each transaction's signatures before validating the
    /// next, so a bad signature among these is the request's error first.
    pub(crate) then: Option<validation::Error>,
    pub(crate) reply: Reply,
}

impl Request<DigestPending> {
    /// Freshly decoded transactions.
    pub fn new(
        transactions: Vec<Message<Transaction<'static, DigestPending>>>,
        reply: Reply,
    ) -> Request<DigestPending> {
        Request {
            transactions,
            then: None,
            reply,
        }
    }
}

impl<S: TxState> Request<S> {
    pub fn transactions(&self) -> &[Message<Transaction<'static, S>>] {
        &self.transactions
    }

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

/// What became of one transaction of a request that passed validation and
/// signature verification.
#[derive(Debug)]
pub enum Outcome {
    /// Executed before.
    Executed(Box<execution::Executed>),
    /// Its inputs failed the checks.
    Rejected(validation::Error),
    /// The store could not be read.
    Failed(String),
    /// Passed the checks, for the caller to submit to consensus: its
    /// `ConsensusTransaction` BCS.
    Submit(Vec<u8>),
}

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
///
/// Inward: RPC handlers (black box): `Request<DigestPending>`.
/// Outward: `SignatureVerifier`: `Request<Valid>`; a refused request is
/// answered directly.
pub struct TransactionValidator {
    epoch: Arc<EpochState>,
    bump: Bump,
}

impl TransactionValidator {
    pub fn new(epoch: Arc<EpochState>) -> TransactionValidator {
        TransactionValidator {
            epoch,
            bump: Bump::with_capacity(ARENA_BYTES),
        }
    }
}

impl Processor for TransactionValidator {
    type Input = Request<DigestPending>;
    type Output = Request<Valid>;

    fn process(&mut self, request: Request<DigestPending>) -> Option<Request<Valid>> {
        let Request {
            transactions,
            then: _,
            reply,
        } = request;
        let (valid, failure) =
            checks::validate(transactions, &self.epoch.context(), &mut self.bump);
        let request = Request {
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
/// it on; a request with a bad signature, or whose validation failed after
/// these transactions, ends here. What it emits is wholly verified.
///
/// Inward: `TransactionValidator`: `Request<Valid>`.
/// Outward: `SubmitChecker` (in the pipeline) or `InputChecker` (today's
/// direct path): `Request<Verified>`.
pub struct SignatureVerifier {
    checks: SignatureChecks,
}

impl SignatureVerifier {
    pub fn new(epoch: Arc<EpochState>) -> SignatureVerifier {
        SignatureVerifier::with_cache(epoch, checks::GENERATION)
    }

    /// With a signature cache of `2 * generation` entries.
    pub fn with_cache(epoch: Arc<EpochState>, generation: usize) -> SignatureVerifier {
        SignatureVerifier {
            checks: SignatureChecks::new(epoch, generation),
        }
    }

    /// Signature cache hits and misses so far.
    pub fn cache_stats(&self) -> (u64, u64) {
        self.checks.cache_stats()
    }
}

impl Processor for SignatureVerifier {
    type Input = Request<Valid>;
    type Output = Request<Verified>;

    fn process(&mut self, request: Request<Valid>) -> Option<Request<Verified>> {
        let Request {
            transactions,
            then,
            reply,
        } = request;
        match (self.checks.verify(transactions), then) {
            (Ok(verified), None) => Some(Request {
                transactions: verified,
                then: None,
                reply,
            }),
            (Err(e), _) | (Ok(_), Some(e)) => {
                let _ = reply.send(Err(Rejected::Invalid(e)));
                None
            }
        }
    }
}

/// The transaction's results, if it executed.
pub(crate) fn executed(store: &store::Store, digest: &[u8; 32]) -> Option<Outcome> {
    match execution::executed(store, digest) {
        Ok(executed) => executed.map(|e| Outcome::Executed(Box::new(e))),
        Err(e) => Some(Outcome::Failed(format!("{e:?}"))),
    }
}
