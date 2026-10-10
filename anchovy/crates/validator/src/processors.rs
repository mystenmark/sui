// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The processors transaction work runs on, off the RPC runtime. A request
//! passes through them, each taking it in one state and emitting it in the
//! next: `Request<DigestPending>` → [`TransactionValidator`] →
//! `Request<Valid>` → [`SignatureVerifier`] → `Request<Verified>` →
//! [`InputChecker`] → [`Checked`] → [`TransactionExecutor`], which answers.
//! A transaction failing validation or signature verification fails its
//! request, as in the reference; one failing the input checks fails only
//! its own result. For now all run on one worker thread.

use std::sync::Arc;

use containers::Bump;
use messages::Message;
use messages::transaction::{DigestPending, Transaction, TxState};
use tokio::sync::oneshot;
use workqueue::{Inbox, Processor, Queue, Refusal, Refuse, Worker, WorkerHandle};

use crate::checks::{self, InputsCheckedTransaction, SignatureChecks, Valid, Verified};
use crate::epoch::EpochState;

/// Where a request's verdict goes: a result per transaction, in order, or
/// why the request as a whole was refused.
pub type Reply = oneshot::Sender<Result<Vec<Outcome>, Rejected>>;

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
    /// Executed, now or before.
    Executed(Box<execution::Executed>),
    /// Its inputs failed the checks.
    Rejected(validation::Error),
    /// Execution failed: the input checks passed what execution could not
    /// run, or the store failed.
    Failed(String),
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
/// it on; a request with a bad signature, or whose validation failed after
/// these transactions, ends here. What it emits is wholly verified.
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
        match (self.checks.verify(&epoch, transactions), then) {
            (Ok(verified), None) => Some(Request {
                epoch,
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

/// A request whose inputs were checked: each transaction to execute, or
/// already answered.
pub struct Checked {
    epoch: Arc<EpochState>,
    transactions: Vec<Step>,
    reply: Reply,
}

// Most steps are executions; boxing them would allocate for each.
#[allow(clippy::large_enum_variant)]
enum Step {
    Execute(InputsCheckedTransaction),
    Done(Outcome),
}

impl Refuse for Checked {
    fn refuse(self, why: Refusal) {
        let _ = self.reply.send(Err(match why {
            Refusal::Full => Rejected::Overloaded,
            Refusal::Closed => Rejected::ShuttingDown,
        }));
    }
}

/// Checks each transaction's inputs against the store (`check_inputs`); a
/// transaction executed before is answered from the store instead, as the
/// reference does.
pub struct InputChecker {
    store: Arc<store::Store>,
}

impl InputChecker {
    pub fn new(store: Arc<store::Store>) -> InputChecker {
        InputChecker { store }
    }
}

impl Processor for InputChecker {
    type Input = Request<Verified>;
    type Output = Checked;

    fn process(&mut self, request: Request<Verified>) -> Option<Checked> {
        let Request {
            epoch,
            transactions,
            then: _,
            reply,
        } = request;
        let transactions = transactions
            .into_iter()
            .map(|transaction| {
                let digest = transaction.get().0.digest().bytes;
                match executed(&self.store, &digest) {
                    Some(outcome) => Step::Done(outcome),
                    None => match checks::check_inputs(&epoch, &self.store, transaction) {
                        Ok(checked) => Step::Execute(checked),
                        Err(e) => Step::Done(Outcome::Rejected(e)),
                    },
                }
            })
            .collect();
        Some(Checked {
            epoch,
            transactions,
            reply,
        })
    }
}

/// The transaction's results, if it executed.
pub(crate) fn executed(store: &store::Store, digest: &[u8; 32]) -> Option<Outcome> {
    match execution::executed(store, digest) {
        Ok(executed) => executed.map(|e| Outcome::Executed(Box::new(e))),
        Err(e) => Some(Outcome::Failed(format!("{e:?}"))),
    }
}

/// Executes a request's transactions one at a time, each committed before
/// the next, so each reads what those before it wrote; then answers.
///
/// # Panics
/// On equivocation: an owned input consumed since its check (`Execution::
/// execute`). Consensus will rule it out; until then nothing locks inputs.
pub struct TransactionExecutor {
    store: Arc<store::Store>,
}

impl TransactionExecutor {
    pub fn new(store: Arc<store::Store>) -> TransactionExecutor {
        TransactionExecutor { store }
    }

    fn execute(&self, epoch: &EpochState, transaction: &InputsCheckedTransaction) -> Outcome {
        execute_and_commit(&self.store, epoch, transaction)
    }
}

/// Executes `transaction` against `store` and commits its outputs, then
/// answers from the store.
pub(crate) fn execute_and_commit(
    store: &store::Store,
    epoch: &EpochState,
    transaction: &InputsCheckedTransaction,
) -> Outcome {
    let digest = transaction.get().0.digest().bytes;
    // `Transaction` BCS is `SenderSignedData`'s: its empty signature
    // info has no bytes.
    let outcome = match epoch.execution.execute(store, transaction.wire_bytes()) {
        Ok(outcome) => outcome,
        Err(e) => return Outcome::Failed(format!("{e:?}")),
    };
    if let Err(e) = store.commit(outcome.commit) {
        return Outcome::Failed(format!("{e:?}"));
    }
    executed(store, &digest)
        .unwrap_or_else(|| Outcome::Failed("committed, but not in the store".to_owned()))
}

impl Processor for TransactionExecutor {
    type Input = Checked;
    type Output = ();

    fn process(&mut self, request: Checked) -> Option<()> {
        let Checked {
            epoch,
            transactions,
            reply,
        } = request;
        let outcomes = transactions
            .into_iter()
            .map(|step| match step {
                Step::Execute(transaction) => self.execute(&epoch, &transaction),
                Step::Done(outcome) => outcome,
            })
            .collect();
        let _ = reply.send(Ok(outcomes));
        None
    }
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
/// are refused; likewise between the later stages.
pub const SIGNATURE_QUEUE: usize = 4096;

impl Processors {
    /// The pipeline, on one thread, over `store`.
    pub fn start(validation_queue: usize, store: Arc<store::Store>) -> Processors {
        let (transactions, validation) = workqueue::queue(validation_queue);
        let worker = Processors::worker(validation, SIGNATURE_QUEUE, store).spawn();
        Processors {
            transactions,
            _worker: worker,
        }
    }

    /// A worker running the pipeline, validation draining `validation`.
    pub fn worker(
        validation: Inbox<Request<DigestPending>>,
        queue: usize,
        store: Arc<store::Store>,
    ) -> Worker {
        let (signatures, verification) = workqueue::queue(queue);
        let (verified, input_checks) = workqueue::queue(queue);
        let (checked, executions) = workqueue::queue(queue);
        let checker_store = store.clone();
        Worker::new("transactions")
            .run(validation, TransactionValidator::new, signatures)
            .run(verification, SignatureVerifier::new, verified)
            .run(
                input_checks,
                move || InputChecker::new(checker_store),
                checked,
            )
            .run(executions, move || TransactionExecutor::new(store), |()| {})
    }
}
