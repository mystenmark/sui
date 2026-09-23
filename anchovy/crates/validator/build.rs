// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The `sui.validator.Validator` service, method for method as
//! `sui-network/build.rs` declares it: same routes, same codec per route.

use tonic_build::manual::{Builder, Method, Service};

fn main() {
    const BCS: &str = "crate::codec::BcsCodec";
    const PROST: &str = "tonic_prost::ProstCodec";
    // (method, route, request, response, codec)
    let methods = [
        (
            "submit_transaction",
            "SubmitTransaction",
            "crate::proto::RawSubmitTxRequest",
            "crate::proto::RawSubmitTxResponse",
            PROST,
        ),
        (
            "wait_for_effects",
            "WaitForEffects",
            "crate::proto::RawWaitForEffectsRequest",
            "crate::proto::RawWaitForEffectsResponse",
            PROST,
        ),
        (
            "object_info",
            "ObjectInfo",
            "messages::grpc::ObjectInfoRequest",
            "crate::codec::Encoded",
            BCS,
        ),
        (
            "transaction_info",
            "TransactionInfo",
            "messages::grpc::TransactionInfoRequest",
            "crate::codec::Encoded",
            BCS,
        ),
        (
            "checkpoint",
            "Checkpoint",
            "messages::grpc::CheckpointRequest",
            "crate::codec::Encoded",
            BCS,
        ),
        (
            "checkpoint_v2",
            "CheckpointV2",
            "messages::grpc::CheckpointRequestV2",
            "crate::codec::Encoded",
            BCS,
        ),
        (
            "get_system_state_object",
            "GetSystemStateObject",
            "messages::grpc::SystemStateRequest",
            "crate::codec::Encoded",
            BCS,
        ),
        (
            "validator_health",
            "ValidatorHealth",
            "crate::proto::RawValidatorHealthRequest",
            "crate::proto::RawValidatorHealthResponse",
            PROST,
        ),
    ];

    let mut service = Service::builder()
        .name("Validator")
        .package("sui.validator")
        .comment("The Validator interface");
    for (name, route, request, response, codec) in methods {
        service = service.method(
            Method::builder()
                .name(name)
                .route_name(route)
                .input_type(request)
                .output_type(response)
                .codec_path(codec)
                .build(),
        );
    }
    Builder::new().compile(&[service.build()]);
    println!("cargo:rerun-if-changed=build.rs");
}
