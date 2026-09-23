// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The handlers. Only health answers; the rest need consensus, storage or
//! execution, which do not exist yet. Pings are among them, since a ping's
//! answer is a consensus position and then that position's commit.

use messages::grpc::{
    CheckpointRequest, CheckpointRequestV2, ObjectInfoRequest, SystemStateRequest,
    TransactionInfoRequest,
};
use tonic::{Request, Response, Status};

use crate::codec::Encoded;
use crate::proto::{
    RawSubmitTxRequest, RawSubmitTxResponse, RawValidatorHealthRequest, RawValidatorHealthResponse,
    RawWaitForEffectsRequest, RawWaitForEffectsResponse,
};
use crate::service::validator_server::{self, ValidatorServer};

#[derive(Default)]
pub struct Validator {}

impl Validator {
    pub fn into_service(self) -> ValidatorServer<Validator> {
        ValidatorServer::new(self)
    }
}

#[tonic::async_trait]
impl validator_server::Validator for Validator {
    async fn submit_transaction(
        &self,
        _request: Request<RawSubmitTxRequest>,
    ) -> Result<Response<RawSubmitTxResponse>, Status> {
        todo!("submit_transaction")
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
