// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The protobuf messages of the validator API, field for field (names,
//! tags, `bytes` fields) as `sui_types::messages_grpc` declares them. The
//! `bytes` fields carry BCS, left undecoded here.

use bytes::Bytes;

#[derive(Clone, PartialEq, prost::Message)]
pub struct RawSubmitTxRequest {
    /// BCS `Transaction`s. Empty for a ping.
    #[prost(bytes = "bytes", repeated, tag = "1")]
    pub transactions: Vec<Bytes>,
    #[prost(enumeration = "SubmitTxType", tag = "2")]
    pub submit_type: i32,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum SubmitTxType {
    /// Transactions may be included separately and out of order.
    Default = 0,
    /// No transactions; measures latency.
    Ping = 1,
    /// Transactions are included together, in order, if possible.
    SoftBundle = 2,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RawSubmitTxResponse {
    /// One per submitted transaction, in order.
    #[prost(message, repeated, tag = "1")]
    pub results: Vec<RawSubmitTxResult>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RawSubmitTxResult {
    #[prost(oneof = "RawValidatorSubmitStatus", tags = "1, 2, 3")]
    pub inner: Option<RawValidatorSubmitStatus>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub enum RawValidatorSubmitStatus {
    /// BCS `ConsensusPosition`.
    #[prost(bytes = "bytes", tag = "1")]
    Submitted(Bytes),
    #[prost(message, tag = "2")]
    Executed(RawExecutedStatus),
    #[prost(message, tag = "3")]
    Rejected(RawRejectedStatus),
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RawWaitForEffectsRequest {
    #[prost(bytes = "bytes", optional, tag = "1")]
    pub transaction_digest: Option<Bytes>,
    /// BCS `ConsensusPosition`.
    #[prost(bytes = "bytes", optional, tag = "2")]
    pub consensus_position: Option<Bytes>,
    #[prost(bool, tag = "3")]
    pub include_details: bool,
    #[prost(enumeration = "PingType", optional, tag = "4")]
    pub ping_type: Option<i32>,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum PingType {
    /// From a block including the ping to that block's commit.
    Consensus = 0,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RawWaitForEffectsResponse {
    /// Always set in a valid response; protobuf's oneof is optional.
    #[prost(oneof = "RawValidatorTransactionStatus", tags = "1, 2, 3")]
    pub inner: Option<RawValidatorTransactionStatus>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub enum RawValidatorTransactionStatus {
    #[prost(message, tag = "1")]
    Executed(RawExecutedStatus),
    #[prost(message, tag = "2")]
    Rejected(RawRejectedStatus),
    #[prost(message, tag = "3")]
    Expired(RawExpiredStatus),
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RawExecutedStatus {
    #[prost(bytes = "bytes", tag = "1")]
    pub effects_digest: Bytes,
    #[prost(message, optional, tag = "2")]
    pub details: Option<RawExecutedData>,
}

/// Each field is BCS of the named type.
#[derive(Clone, PartialEq, prost::Message)]
pub struct RawExecutedData {
    #[prost(bytes = "bytes", tag = "1")]
    pub effects: Bytes,
    #[prost(bytes = "bytes", optional, tag = "2")]
    pub events: Option<Bytes>,
    #[prost(bytes = "bytes", repeated, tag = "3")]
    pub input_objects: Vec<Bytes>,
    #[prost(bytes = "bytes", repeated, tag = "4")]
    pub output_objects: Vec<Bytes>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RawRejectedStatus {
    /// BCS `SuiError`.
    #[prost(bytes = "bytes", optional, tag = "1")]
    pub error: Option<Bytes>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RawExpiredStatus {
    /// The validator's current epoch.
    #[prost(uint64, tag = "1")]
    pub epoch: u64,
    /// The validator's current round; 0 if not yet checked.
    #[prost(uint32, optional, tag = "2")]
    pub round: Option<u32>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RawValidatorHealthRequest {}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RawValidatorHealthResponse {
    #[prost(uint64, optional, tag = "1")]
    pub pending_certificates: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub inflight_consensus_messages: Option<u64>,
    #[prost(uint64, optional, tag = "3")]
    pub consensus_round: Option<u64>,
    #[prost(uint64, optional, tag = "4")]
    pub checkpoint_sequence: Option<u64>,
}
