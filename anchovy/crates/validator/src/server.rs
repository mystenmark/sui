// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The handlers. They decode, hand work to processors and await it; none
//! does transaction work on the RPC runtime. Health answers.
//!
//! Submission checks each transaction on the processors. Over the pipeline
//! (`Validator::with_consensus`) it then submits the rest to consensus and
//! answers with their positions; a client then waits for each with
//! `wait_for_effects`. Without consensus, the processors execute each
//! transaction and answer with its effects.
//!
//! The rest need more of storage, which does not exist yet. Pings are among
//! them, since a ping's answer is a consensus position and then that
//! position's commit.
//!
//! The server is perpetual; the epoch it serves is not. Each request takes
//! the running epoch once and works against it. If that epoch ends first,
//! the request waits for the next and starts again there, so clients see a
//! pause across reconfiguration, not an error.

use std::sync::Arc;

use messages::Message;
use messages::transaction::{DigestPending, Transaction};
use tokio::sync::oneshot;
use workqueue::{PushError, Queue};

use messages::grpc::{
    CheckpointRequest, CheckpointRequestV2, ObjectInfoRequest, SystemStateRequest,
    TransactionInfoRequest,
};
use tonic::{Request, Response, Status};

use ::consensus::{Priority, TransactionClient};
use waiter::{Command, WaitBatch};

use crate::codec::Encoded;
use crate::consensus::status::{
    ConsensusPosition, ConsensusTxStatus, ConsensusTxStatusCache, StatusResult,
};
use crate::current::{CurrentEpoch, EpochHandle};
use crate::epoch::EpochState;
use crate::pipeline::items::EffectsWaitItem;
use crate::pipeline::keys::EffectsKey;
use crate::processors::{self, Outcome, Rejected};
use crate::proto::{
    RawExecutedData, RawExecutedStatus, RawExpiredStatus, RawRejectedStatus, RawSubmitTxRequest,
    RawSubmitTxResponse, RawSubmitTxResult, RawValidatorHealthRequest, RawValidatorHealthResponse,
    RawValidatorSubmitStatus, RawValidatorTransactionStatus, RawWaitForEffectsRequest,
    RawWaitForEffectsResponse, SubmitTxType,
};
use crate::service::validator_server::{self, ValidatorServer};

pub struct Validator {
    current: CurrentEpoch,
}

/// Why an attempt in one epoch did not answer.
enum Failure {
    Status(Status),
    /// Its epoch's pipeline went away: try in the next.
    EpochEnded,
}

impl From<Status> for Failure {
    fn from(status: Status) -> Failure {
        Failure::Status(status)
    }
}

/// What submission and waiting take from the pipeline.
pub struct Consensus {
    pub client: TransactionClient,
    pub statuses: Arc<ConsensusTxStatusCache>,
    /// The effects waiter's queue.
    pub effects: Queue<Command<EffectsKey, EffectsWaitItem>>,
    pub store: Arc<store::Store>,
}

impl Validator {
    /// A standalone validator's server, over processors that execute, in
    /// one epoch for good.
    pub fn new(
        epoch: Arc<EpochState>,
        transactions: Queue<processors::Request<DigestPending>>,
    ) -> Validator {
        Validator::over(CurrentEpoch::fixed(EpochHandle {
            epoch,
            transactions,
            consensus: None,
        }))
    }

    /// The server over whichever epoch runs.
    pub fn over(current: CurrentEpoch) -> Validator {
        Validator { current }
    }

    /// What to do after `failure` in `epoch`: answer, or retry once the
    /// next epoch runs.
    async fn retry(&self, failure: Failure, epoch: &Arc<EpochHandle>) -> Result<(), Status> {
        match failure {
            Failure::Status(status) => Err(status),
            Failure::EpochEnded if !self.current.reconfigures() => {
                Err(Status::unavailable("shutting down"))
            }
            Failure::EpochEnded => {
                self.current.ended(epoch).await;
                Ok(())
            }
        }
    }

    /// Checks, then submits, `request`'s transactions in `epoch`.
    async fn submit_in(
        epoch: &EpochHandle,
        request: &RawSubmitTxRequest,
    ) -> Result<RawSubmitTxResponse, Failure> {
        let outcomes = Self::enqueue(epoch, request)?
            .await
            .map_err(|_| Failure::EpochEnded)?
            .map_err(|e| match e {
                Rejected::ShuttingDown => Failure::EpochEnded,
                e => Failure::Status(rejection(&e)),
            })?;
        Ok(RawSubmitTxResponse {
            results: Self::submit(epoch, outcomes).await?,
        })
    }

    /// Submits the transactions the processors readied, as one bundle, and
    /// answers each with its position.
    async fn submit(
        epoch: &EpochHandle,
        outcomes: Vec<Outcome>,
    ) -> Result<Vec<RawSubmitTxResult>, Failure> {
        let bundle: Vec<Vec<u8>> = outcomes
            .iter()
            .filter_map(|o| match o {
                Outcome::Submit(bytes) => Some(bytes.clone()),
                _ => None,
            })
            .collect();
        let mut positions = Vec::new();
        if !bundle.is_empty() {
            let consensus = epoch
                .consensus
                .as_ref()
                .expect("only the pipeline's processors ready transactions to submit");
            // What becomes of the block is the status cache's to report, for
            // any position, so its receiver is not kept.
            let (block, indices, _) = consensus
                .client
                .submit(bundle, Priority::Normal)
                .await
                .map_err(|_| Failure::EpochEnded)?;
            positions = indices
                .into_iter()
                .map(|index| ConsensusPosition {
                    epoch: epoch.epoch.params.epoch,
                    block,
                    index,
                })
                .collect();
        }
        let mut positions = positions.into_iter();
        Ok(outcomes
            .into_iter()
            .map(|outcome| match outcome {
                Outcome::Submit(_) => RawSubmitTxResult {
                    inner: Some(RawValidatorSubmitStatus::Submitted(
                        positions
                            .next()
                            .expect("a position per transaction submitted")
                            .to_bcs()
                            .into(),
                    )),
                },
                outcome => result(outcome),
            })
            .collect())
    }

    /// The transaction's effects, once executed.
    async fn executed(
        consensus: &Consensus,
        digest: [u8; 32],
        include_details: bool,
    ) -> Result<RawValidatorTransactionStatus, Failure> {
        let read = || {
            execution::executed(&consensus.store, &digest)
                .map_err(|e| Status::internal(format!("{e:?}")))
        };
        let executed = if let Some(executed) = read()? {
            executed
        } else {
            let (reply, done) = oneshot::channel();
            let mut batch = WaitBatch::with_capacity(1, 1);
            batch.push(EffectsWaitItem::Executed(reply), [EffectsKey(digest)]);
            consensus
                .effects
                .try_push(Command::Wait(batch))
                .map_err(|e| match e {
                    PushError::Full(_) => {
                        Failure::Status(Status::resource_exhausted("effects waiter full"))
                    }
                    PushError::Closed(_) => Failure::EpochEnded,
                })?;
            done.await.map_err(|_| Failure::EpochEnded)?;
            read()?.expect("effects notified once stored")
        };
        let mut status = executed_status(executed);
        if !include_details {
            status.details = None;
        }
        Ok(RawValidatorTransactionStatus::Executed(status))
    }

    /// What consensus decided about the transaction at `position`, unless it
    /// was finalized: then its effects answer.
    async fn decided(
        epoch: &EpochState,
        statuses: &ConsensusTxStatusCache,
        position: ConsensusPosition,
    ) -> RawValidatorTransactionStatus {
        match statuses.notify_read(position).await {
            StatusResult::Status(ConsensusTxStatus::Rejected | ConsensusTxStatus::Dropped) => {
                RawValidatorTransactionStatus::Rejected(RawRejectedStatus { error: None })
            }
            StatusResult::Status(ConsensusTxStatus::Finalized) => std::future::pending().await,
            StatusResult::Expired(round) => {
                RawValidatorTransactionStatus::Expired(RawExpiredStatus {
                    epoch: epoch.params.epoch,
                    round: Some(round),
                })
            }
        }
    }

    pub fn into_service(self) -> ValidatorServer<Validator> {
        ValidatorServer::new(self)
    }

    /// The request's shape, as the reference checks it before decoding.
    fn check_request(
        epoch: &EpochState,
        request: &RawSubmitTxRequest,
    ) -> Result<SubmitTxType, Status> {
        let submit_type = SubmitTxType::try_from(request.submit_type)
            .map_err(|_| Status::invalid_argument("unknown submit type"))?;
        let count = request.transactions.len();
        match submit_type {
            SubmitTxType::Ping if count > 0 => {
                return Err(Status::invalid_argument("a ping carries no transactions"));
            }
            SubmitTxType::Default | SubmitTxType::SoftBundle if count == 0 => {
                return Err(Status::invalid_argument("no transactions"));
            }
            _ => {}
        }
        let config = &epoch.params.config;
        let max = if submit_type == SubmitTxType::SoftBundle {
            config.max_soft_bundle_size()
        } else {
            config.max_num_transactions_in_block()
        };
        if count as u64 > max {
            return Err(Status::invalid_argument(format!(
                "{count} transactions, at most {max}"
            )));
        }
        Ok(submit_type)
    }

    /// Decodes the request's transactions and queues them for validation.
    /// Not inlined into the handler: holding decoded messages across an
    /// `await` defeats the future's `Send` inference.
    fn enqueue(
        epoch: &EpochHandle,
        request: &RawSubmitTxRequest,
    ) -> Result<oneshot::Receiver<Result<Vec<Outcome>, Rejected>>, Failure> {
        let mut transactions = Vec::with_capacity(request.transactions.len());
        for bytes in &request.transactions {
            let transaction = Message::<Transaction<DigestPending>>::parse(bytes.to_vec())
                .map_err(|(e, _)| {
                    Status::invalid_argument(format!("TransactionDeserializationError: {e:?}"))
                })?;
            transactions.push(transaction);
        }
        let (reply, verdict) = oneshot::channel();
        epoch
            .transactions
            .try_push(processors::Request::new(transactions, reply))
            .map_err(|e| match e {
                PushError::Full(_) => {
                    Failure::Status(Status::resource_exhausted("validation queue full"))
                }
                PushError::Closed(_) => Failure::EpochEnded,
            })?;
        Ok(verdict)
    }
}

/// An executed transaction as the reference's `ExecutedStatus`, with its
/// details.
fn executed_status(executed: execution::Executed) -> RawExecutedStatus {
    RawExecutedStatus {
        effects_digest: executed.effects_digest_bcs().into(),
        details: Some(RawExecutedData {
            effects: executed.effects.into(),
            events: executed.events.map(Into::into),
            input_objects: executed.input_objects.into_iter().map(Into::into).collect(),
            output_objects: executed
                .output_objects
                .into_iter()
                .map(Into::into)
                .collect(),
        }),
    }
}

/// A transaction's result as the reference's `SubmitTxResult`.
fn result(outcome: Outcome) -> RawSubmitTxResult {
    let status = match outcome {
        Outcome::Executed(executed) => {
            RawValidatorSubmitStatus::Executed(executed_status(*executed))
        }
        // Without the reason: it would be BCS of the reference's `SuiError`,
        // which anchovy's errors do not map to yet.
        Outcome::Rejected(_) | Outcome::Failed(_) => {
            RawValidatorSubmitStatus::Rejected(RawRejectedStatus { error: None })
        }
        Outcome::Submit(_) => unreachable!("submitted, then answered with its position"),
    };
    RawSubmitTxResult {
        inner: Some(status),
    }
}

fn rejection(rejected: &Rejected) -> Status {
    match rejected {
        Rejected::Invalid(e) => Status::invalid_argument(format!("{:?}: {}", e.kind, e.detail)),
        Rejected::Overloaded => Status::resource_exhausted("signature verification queue full"),
        Rejected::ShuttingDown => Status::unavailable("shutting down"),
    }
}

#[tonic::async_trait]
impl validator_server::Validator for Validator {
    /// Decodes each transaction here, without its digest; validates, hashes
    /// and verifies them on the processors, and fails the request if any is
    /// invalid, as the reference does. Then answers each: executed already,
    /// rejected by its input checks, or submitted to consensus, with its
    /// position (without consensus, executed now).
    async fn submit_transaction(
        &self,
        request: Request<RawSubmitTxRequest>,
    ) -> Result<Response<RawSubmitTxResponse>, Status> {
        let request = request.into_inner();
        loop {
            let epoch = self.current.get().await;
            if Self::check_request(&epoch.epoch, &request)? == SubmitTxType::Ping {
                todo!("ping: a consensus position")
            }
            let failure = tokio::select! {
                result = Self::submit_in(&epoch, &request) => match result {
                    Ok(response) => return Ok(Response::new(response)),
                    Err(failure) => failure,
                },
                () = self.current.ended(&epoch) => Failure::EpochEnded,
            };
            self.retry(failure, &epoch).await?;
        }
    }

    /// As the reference's: the transaction's effects once it executes, or,
    /// given the position it was submitted at, whatever consensus decided
    /// there first if that was not to finalize it: rejected or dropped, or
    /// expired, never committed. Effects can come first, from the same
    /// transaction committed at another position.
    async fn wait_for_effects(
        &self,
        request: Request<RawWaitForEffectsRequest>,
    ) -> Result<Response<RawWaitForEffectsResponse>, Status> {
        let request = request.into_inner();
        if request.ping_type.is_some() {
            todo!("ping: a consensus position's commit")
        }
        let digest: [u8; 32] = request
            .transaction_digest
            .as_deref()
            .and_then(|d| d.try_into().ok())
            .ok_or_else(|| Status::invalid_argument("a 32-byte transaction digest is required"))?;
        let position = match &request.consensus_position {
            Some(bytes) => Some(
                ConsensusPosition::from_bcs(bytes)
                    .ok_or_else(|| Status::invalid_argument("not a consensus position"))?,
            ),
            None => None,
        };
        loop {
            let epoch = self.current.get().await;
            let Some(consensus) = &epoch.consensus else {
                return Err(Status::unimplemented("waiting needs consensus"));
            };
            // A position from an earlier epoch has no status here: its
            // effects answer.
            let position = position.filter(|p| p.epoch == epoch.epoch.params.epoch);
            if let Some(position) = &position
                && consensus.statuses.too_far_ahead(position)
            {
                return Err(Status::unavailable("ValidatorConsensusLagging"));
            }
            let decided = async {
                match position {
                    Some(position) => {
                        Ok(Self::decided(&epoch.epoch, &consensus.statuses, position).await)
                    }
                    None => std::future::pending().await,
                }
            };
            let failure = tokio::select! {
                status = Self::executed(consensus, digest, request.include_details) => match status {
                    Ok(status) => return Ok(Response::new(RawWaitForEffectsResponse {
                        inner: Some(status),
                    })),
                    Err(failure) => failure,
                },
                status = decided => match status {
                    Ok(status) => return Ok(Response::new(RawWaitForEffectsResponse {
                        inner: Some(status),
                    })),
                    Err(failure) => failure,
                },
                () = self.current.ended(&epoch) => Failure::EpochEnded,
            };
            self.retry(failure, &epoch).await?;
        }
    }

    async fn object_info(
        &self,
        _request: Request<ObjectInfoRequest>,
    ) -> Result<Response<Encoded>, Status> {
        todo!("object_info")
    }

    async fn transaction_info(
        &self,
        _request: Request<TransactionInfoRequest>,
    ) -> Result<Response<Encoded>, Status> {
        todo!("transaction_info")
    }

    async fn checkpoint(
        &self,
        _request: Request<CheckpointRequest>,
    ) -> Result<Response<Encoded>, Status> {
        todo!("checkpoint")
    }

    async fn checkpoint_v2(
        &self,
        _request: Request<CheckpointRequestV2>,
    ) -> Result<Response<Encoded>, Status> {
        todo!("checkpoint_v2")
    }

    async fn get_system_state_object(
        &self,
        _request: Request<SystemStateRequest>,
    ) -> Result<Response<Encoded>, Status> {
        todo!("get_system_state_object")
    }

    /// Every field is optional in the reference; absent until the
    /// components that know them exist.
    async fn validator_health(
        &self,
        _request: Request<RawValidatorHealthRequest>,
    ) -> Result<Response<RawValidatorHealthResponse>, Status> {
        Ok(Response::new(RawValidatorHealthResponse::default()))
    }
}
