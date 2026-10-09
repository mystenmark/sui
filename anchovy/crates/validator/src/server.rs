// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The handlers. They decode, hand work to processors and await it; none
//! does transaction work on the RPC runtime. Health answers; submission
//! checks and executes, with no consensus yet; the rest need consensus or
//! more of storage, which do not exist yet. Pings are among them, since a
//! ping's answer is a consensus position and then that position's commit.

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

use crate::codec::Encoded;
use crate::epoch::EpochState;
use crate::processors::{self, Outcome, Rejected};
use crate::proto::{
    RawExecutedData, RawExecutedStatus, RawRejectedStatus, RawSubmitTxRequest, RawSubmitTxResponse,
    RawSubmitTxResult, RawValidatorHealthRequest, RawValidatorHealthResponse,
    RawValidatorSubmitStatus, RawWaitForEffectsRequest, RawWaitForEffectsResponse, SubmitTxType,
};
use crate::service::validator_server::{self, ValidatorServer};

pub struct Validator {
    epoch: Arc<EpochState>,
    transactions: Queue<processors::Request<DigestPending>>,
}

impl Validator {
    pub fn new(
        epoch: Arc<EpochState>,
        transactions: Queue<processors::Request<DigestPending>>,
    ) -> Validator {
        Validator {
            epoch,
            transactions,
        }
    }

    pub fn into_service(self) -> ValidatorServer<Validator> {
        ValidatorServer::new(self)
    }

    /// The request's shape, as the reference checks it before decoding.
    fn check_request(&self, request: &RawSubmitTxRequest) -> Result<SubmitTxType, Status> {
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
        let config = &self.epoch.config;
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
        &self,
        request: &RawSubmitTxRequest,
    ) -> Result<oneshot::Receiver<Result<Vec<Outcome>, Rejected>>, Status> {
        let mut transactions = Vec::with_capacity(request.transactions.len());
        for bytes in &request.transactions {
            let transaction = Message::<Transaction<DigestPending>>::parse(bytes.to_vec())
                .map_err(|(e, _)| {
                    Status::invalid_argument(format!("TransactionDeserializationError: {e:?}"))
                })?;
            transactions.push(transaction);
        }
        let (reply, verdict) = oneshot::channel();
        self.transactions
            .try_push(processors::Request::new(
                self.epoch.clone(),
                transactions,
                reply,
            ))
            .map_err(|e| match e {
                PushError::Full(_) => Status::resource_exhausted("validation queue full"),
                PushError::Closed(_) => Status::unavailable("shutting down"),
            })?;
        Ok(verdict)
    }
}

/// A transaction's result as the reference's `SubmitTxResult`.
fn result(outcome: Outcome) -> RawSubmitTxResult {
    let status = match outcome {
        Outcome::Executed(executed) => RawValidatorSubmitStatus::Executed(RawExecutedStatus {
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
        }),
        // Without the reason: it would be BCS of the reference's `SuiError`,
        // which anchovy's errors do not map to yet.
        Outcome::Rejected(_) | Outcome::Failed(_) => {
            RawValidatorSubmitStatus::Rejected(RawRejectedStatus { error: None })
        }
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
    /// invalid, as the reference does. Then, with no consensus yet, checks
    /// each one's inputs and executes it, answering with its effects.
    async fn submit_transaction(
        &self,
        request: Request<RawSubmitTxRequest>,
    ) -> Result<Response<RawSubmitTxResponse>, Status> {
        let request = request.into_inner();
        if self.check_request(&request)? == SubmitTxType::Ping {
            todo!("ping: a consensus position")
        }

        let outcomes = self
            .enqueue(&request)?
            .await
            .map_err(|_| Status::internal("processing did not finish"))?
            .map_err(|e| rejection(&e))?;
        Ok(Response::new(RawSubmitTxResponse {
            results: outcomes.into_iter().map(result).collect(),
        }))
    }

    async fn wait_for_effects(
        &self,
        _request: Request<RawWaitForEffectsRequest>,
    ) -> Result<Response<RawWaitForEffectsResponse>, Status> {
        todo!("wait_for_effects")
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
