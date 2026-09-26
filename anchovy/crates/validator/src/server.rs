// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The handlers. They decode, hand work to processors and await it; none
//! does transaction work on the RPC runtime. Health answers; submission
//! validates and stops short of consensus; the rest need consensus, storage
//! or execution, which do not exist yet. Pings are among them, since a
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
use crate::processors::{ValidateTransactions, Validated};
use crate::proto::{
    RawSubmitTxRequest, RawSubmitTxResponse, RawValidatorHealthRequest, RawValidatorHealthResponse,
    RawWaitForEffectsRequest, RawWaitForEffectsResponse, SubmitTxType,
};
use crate::service::validator_server::{self, ValidatorServer};

pub struct Validator {
    epoch: Arc<EpochState>,
    transactions: Queue<ValidateTransactions>,
}

impl Validator {
    pub fn new(epoch: Arc<EpochState>, transactions: Queue<ValidateTransactions>) -> Validator {
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
    ) -> Result<oneshot::Receiver<Result<Validated, validation::Error>>, Status> {
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
            .try_push(ValidateTransactions {
                transactions,
                reply,
            })
            .map_err(|e| match e {
                PushError::Full(_) => Status::resource_exhausted("validation queue full"),
                PushError::Closed(_) => Status::unavailable("shutting down"),
            })?;
        Ok(verdict)
    }
}

fn validation_failure(e: &validation::Error) -> Status {
    Status::invalid_argument(format!("{:?}: {}", e.kind, e.detail))
}

#[tonic::async_trait]
impl validator_server::Validator for Validator {
    /// Decodes each transaction here, without its digest; validates and
    /// hashes them on a processor, and fails the request if any is invalid,
    /// as the reference does. What passes has no consensus to go to yet.
    async fn submit_transaction(
        &self,
        request: Request<RawSubmitTxRequest>,
    ) -> Result<Response<RawSubmitTxResponse>, Status> {
        let request = request.into_inner();
        if self.check_request(&request)? == SubmitTxType::Ping {
            todo!("ping: a consensus position")
        }

        self.enqueue(&request)?
            .await
            .map_err(|_| Status::internal("validation did not finish"))?
            .map_err(|e| validation_failure(&e))?;
        Err(Status::unimplemented("consensus submission"))
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
