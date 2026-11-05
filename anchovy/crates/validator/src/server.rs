// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The handlers. They decode, hand work to processors and await it; none
//! does transaction work on the RPC runtime. Health answers.
//!
//! Submission checks each transaction on the epoch's processors, then
//! submits the rest to consensus and answers with their positions; a client
//! then waits for each with `wait_for_effects`.
//!
//! A ping submits no transactions: its answer is a consensus position, and
//! then that position's block committing.
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
use tonic::{Code, Request, Response, Status};
use validation::SuiErrorKind;

use ::consensus::{PING_TRANSACTION_INDEX, Priority, TransactionClient};
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
    PingType, RawExecutedData, RawExecutedStatus, RawExpiredStatus, RawRejectedStatus,
    RawSubmitTxRequest, RawSubmitTxResponse, RawSubmitTxResult, RawValidatorHealthRequest,
    RawValidatorHealthResponse, RawValidatorSubmitStatus, RawValidatorTransactionStatus,
    RawWaitForEffectsRequest, RawWaitForEffectsResponse, SubmitTxType,
};
use crate::service::validator_server::{self, ValidatorServer};

const PING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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
    /// The server over whichever epoch runs.
    pub fn over(current: CurrentEpoch) -> Validator {
        Validator { current }
    }

    /// What to do after `failure` in `epoch`: answer, or retry once the
    /// next epoch runs.
    async fn retry(&self, failure: Failure, epoch: &Arc<EpochHandle>) -> Result<(), Status> {
        match failure {
            Failure::Status(status) => Err(status),
            Failure::EpochEnded => {
                self.current.ended(epoch).await;
                Ok(())
            }
        }
    }

    /// Checks, then submits, `request`'s transactions in `epoch`; or, for a
    /// ping, submits none and answers with the ping's position.
    async fn submit_in(
        epoch: &EpochHandle,
        request: &RawSubmitTxRequest,
        ping: bool,
    ) -> Result<RawSubmitTxResponse, Failure> {
        if ping {
            let position = Self::submit_ping(epoch).await?;
            return Ok(RawSubmitTxResponse {
                results: vec![RawSubmitTxResult {
                    inner: Some(RawValidatorSubmitStatus::Submitted(
                        position.to_bcs().into(),
                    )),
                }],
            });
        }
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
            let consensus = &epoch.consensus;
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

    /// Submits no transactions: consensus acknowledges that with the ping
    /// position of the block it will propose next.
    async fn submit_ping(epoch: &EpochHandle) -> Result<ConsensusPosition, Failure> {
        let (block, indices, _) = epoch
            .consensus
            .client
            .submit(Vec::new(), Priority::Normal)
            .await
            .map_err(|_| Failure::EpochEnded)?;
        debug_assert_eq!(indices, [PING_TRANSACTION_INDEX]);
        Ok(ConsensusPosition {
            epoch: epoch.epoch.params.epoch,
            block,
            index: PING_TRANSACTION_INDEX,
        })
    }

    /// What became of a ping's block, as the reference's `ping_response`:
    /// its commit answers as executed with no effects.
    async fn pinged(
        epoch: &EpochState,
        statuses: &ConsensusTxStatusCache,
        position: ConsensusPosition,
        include_details: bool,
    ) -> RawValidatorTransactionStatus {
        match statuses.notify_read(position).await {
            StatusResult::Status(ConsensusTxStatus::Finalized) => {
                RawValidatorTransactionStatus::Executed(RawExecutedStatus {
                    effects_digest: bcs::to_bytes(
                        &sui_types::digests::TransactionEffectsDigest::ZERO,
                    )
                    .expect("a digest serializes")
                    .into(),
                    // A client decodes the effects: the reference sends
                    // default ones.
                    details: include_details.then(|| RawExecutedData {
                        effects: bcs::to_bytes(&sui_types::effects::TransactionEffects::default())
                            .expect("effects serialize")
                            .into(),
                        ..RawExecutedData::default()
                    }),
                })
            }
            status => Self::undecided(epoch, statuses, position, status),
        }
    }

    /// Answers a ping's `wait_for_effects` within ten seconds, as the
    /// reference does.
    async fn ping(
        &self,
        position: Option<ConsensusPosition>,
        include_details: bool,
    ) -> Result<Response<RawWaitForEffectsResponse>, Status> {
        let position = position.ok_or_else(|| {
            failed(&SuiErrorKind::InvalidRequest(
                "Consensus position is required for Ping requests".to_owned(),
            ))
        })?;
        let epoch = self.current.get().await;
        if position.epoch < epoch.epoch.params.epoch {
            return Err(epoch_ended());
        }
        let statuses = &epoch.consensus.statuses;
        if statuses.too_far_ahead(&position) {
            return Err(Status::unavailable("ValidatorConsensusLagging"));
        }
        let pinged = Self::pinged(&epoch.epoch, statuses, position, include_details);
        let status = tokio::select! {
            status = tokio::time::timeout(PING_TIMEOUT, pinged) => {
                status.map_err(|_| Status::deadline_exceeded("TimeoutError"))?
            }
            () = self.current.ended(&epoch) => return Err(epoch_ended()),
        };
        Ok(Response::new(RawWaitForEffectsResponse {
            inner: Some(status),
        }))
    }

    /// The transaction's effects, once executed.
    async fn executed(
        consensus: &Consensus,
        digest: [u8; 32],
        include_details: bool,
    ) -> Result<RawValidatorTransactionStatus, Failure> {
        let read = || {
            execution::executed_named(&consensus.store, &messages::base::Digest::new(digest))
                .map_err(|e| Status::internal(format!("{e:?}")))
        };
        let executed = if let Some(executed) = read()? {
            executed
        } else {
            let (reply, done) = oneshot::channel();
            let mut batch = WaitBatch::with_capacity(1, 1);
            batch.push(
                EffectsWaitItem::Executed(reply),
                [EffectsKey(messages::base::Digest::new(digest))],
            );
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
            StatusResult::Status(ConsensusTxStatus::Finalized) => std::future::pending().await,
            status => Self::undecided(epoch, statuses, position, status),
        }
    }

    /// A status other than finalized, as the reference answers it: a
    /// rejection with the reason recorded for it, if any.
    fn undecided(
        epoch: &EpochState,
        statuses: &ConsensusTxStatusCache,
        position: ConsensusPosition,
        status: StatusResult,
    ) -> RawValidatorTransactionStatus {
        match status {
            StatusResult::Status(ConsensusTxStatus::Rejected | ConsensusTxStatus::Dropped) => {
                RawValidatorTransactionStatus::Rejected(RawRejectedStatus {
                    error: statuses
                        .reason(&position)
                        .map(|reason| reason_bcs(&reason).into()),
                })
            }
            StatusResult::Status(ConsensusTxStatus::Finalized) => {
                unreachable!("finalized is answered by the caller")
            }
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
        let submit_type = SubmitTxType::try_from(request.submit_type).map_err(|e| {
            failed(&SuiErrorKind::GrpcMessageDeserializeError {
                type_info: "RawSubmitTxRequest.submit_type".to_owned(),
                error: e.to_string(),
            })
        })?;
        let invalid = |message: String| failed(&SuiErrorKind::InvalidRequest(message));
        let count = request.transactions.len();
        match submit_type {
            SubmitTxType::Ping if count > 0 => {
                return Err(invalid(format!(
                    "Ping request cannot contain {count} transactions"
                )));
            }
            SubmitTxType::Default | SubmitTxType::SoftBundle if count == 0 => {
                return Err(invalid(
                    "At least one transaction needs to be submitted".to_owned(),
                ));
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
            return Err(invalid(format!(
                "Too many transactions in request: {count} vs {max}"
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
        for (i, bytes) in request.transactions.iter().enumerate() {
            let transaction = Message::<Transaction<DigestPending>>::parse(bytes.to_vec())
                .map_err(|(e, _)| {
                    failed(&SuiErrorKind::TransactionDeserializationError {
                        error: format!("Failed to deserialize transaction at index {i}: {e:?}"),
                    })
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
        Outcome::Rejected(e) => RawValidatorSubmitStatus::Rejected(RawRejectedStatus {
            error: Some(reason_bcs(&e.reason).into()),
        }),
        Outcome::Failed(e) => RawValidatorSubmitStatus::Rejected(RawRejectedStatus {
            error: Some(reason_bcs(&SuiErrorKind::Storage(e)).into()),
        }),
        Outcome::Submit(_) => unreachable!("submitted, then answered with its position"),
    };
    RawSubmitTxResult {
        inner: Some(status),
    }
}

/// The reference's `SuiErrorKind::EpochEnded`.
fn epoch_ended() -> Status {
    Status::unavailable("EpochEnded")
}

/// BCS of the reference's `SuiError`, which encodes as its kind.
fn reason_bcs(reason: &SuiErrorKind) -> Vec<u8> {
    bcs::to_bytes(reason).expect("an error's strings and lists fit BCS's bounds")
}

/// A failed request as the reference fails one: clients decode its
/// `SuiError` from the status's details.
fn failed(reason: &SuiErrorKind) -> Status {
    Status::with_details(
        Code::Internal,
        format!("{reason:?}"),
        reason_bcs(reason).into(),
    )
}

fn rejection(rejected: &Rejected) -> Status {
    match rejected {
        Rejected::Invalid(e) => failed(&e.reason),
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
    /// position.
    async fn submit_transaction(
        &self,
        request: Request<RawSubmitTxRequest>,
    ) -> Result<Response<RawSubmitTxResponse>, Status> {
        let request = request.into_inner();
        loop {
            let epoch = self.current.get().await;
            let ping = Self::check_request(&epoch.epoch, &request)? == SubmitTxType::Ping;
            let failure = tokio::select! {
                result = Self::submit_in(&epoch, &request, ping) => match result {
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
    /// transaction committed at another position. A position whose epoch
    /// ends is answered `EpochEnded`, as the reference's: consensus may have
    /// dropped the transaction, so the client resubmits.
    async fn wait_for_effects(
        &self,
        request: Request<RawWaitForEffectsRequest>,
    ) -> Result<Response<RawWaitForEffectsResponse>, Status> {
        let request = request.into_inner();
        // Decoded as the reference's client encodes them, in its order.
        let undecodable = |type_info: &str, error: String| {
            failed(&SuiErrorKind::GrpcMessageDeserializeError {
                type_info: type_info.to_owned(),
                error,
            })
        };
        let digest = request
            .transaction_digest
            .as_deref()
            .map(|d| {
                bcs::from_bytes::<sui_types::digests::TransactionDigest>(d)
                    .map(sui_types::digests::TransactionDigest::into_inner)
                    .map_err(|e| {
                        undecodable("RawWaitForEffectsRequest.transaction_digest", e.to_string())
                    })
            })
            .transpose()?;
        let position = request
            .consensus_position
            .as_deref()
            .map(|p| {
                ConsensusPosition::from_bcs(p)
                    .map_err(|e| undecodable("ConsensusPosition", e.to_string()))
            })
            .transpose()?;
        if let Some(ping) = request.ping_type {
            PingType::try_from(ping)
                .map_err(|e| undecodable("RawWaitForEffectsRequest.ping_type", e.to_string()))?;
            return self.ping(position, request.include_details).await;
        }
        let digest = digest.ok_or_else(|| {
            failed(&SuiErrorKind::InvalidRequest(
                "Transaction digest is required for wait for effects requests".to_owned(),
            ))
        })?;
        loop {
            let epoch = self.current.get().await;
            let consensus = &epoch.consensus;
            if position.is_some_and(|p| p.epoch < epoch.epoch.params.epoch) {
                return Err(epoch_ended());
            }
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
                () = self.current.ended(&epoch) => match position {
                    Some(_) => return Err(epoch_ended()),
                    None => Failure::EpochEnded,
                },
            };
            self.retry(failure, &epoch).await?;
        }
    }

    // The reads below serve only diagnostic tools (sui-tool) in the
    // reference; not yet ported.
    async fn object_info(
        &self,
        _: Request<ObjectInfoRequest>,
    ) -> Result<Response<Encoded>, Status> {
        Err(Status::unimplemented("object_info"))
    }

    async fn transaction_info(
        &self,
        _: Request<TransactionInfoRequest>,
    ) -> Result<Response<Encoded>, Status> {
        Err(Status::unimplemented("transaction_info"))
    }

    async fn checkpoint(&self, _: Request<CheckpointRequest>) -> Result<Response<Encoded>, Status> {
        Err(Status::unimplemented("checkpoint"))
    }

    async fn checkpoint_v2(
        &self,
        _: Request<CheckpointRequestV2>,
    ) -> Result<Response<Encoded>, Status> {
        Err(Status::unimplemented("checkpoint_v2"))
    }

    async fn get_system_state_object(
        &self,
        _: Request<SystemStateRequest>,
    ) -> Result<Response<Encoded>, Status> {
        Err(Status::unimplemented("get_system_state_object"))
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
