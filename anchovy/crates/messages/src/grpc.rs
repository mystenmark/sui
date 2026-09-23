// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The requests of the validator gRPC API that travel as BCS
//! (`sui_types::messages_grpc` and `messages_checkpoint`). They are small
//! and fixed-shape, so they are parsed into plain values with no arena.

use crate::base::{Digest, ObjectId};
use crate::error::{ParseError, Result};
use crate::fast::Writer;
use crate::reader::Reader;

/// Parses a whole buffer as `T`, rejecting trailing bytes.
fn parse_exact<T>(buf: &[u8], parse: impl FnOnce(&mut Reader<'_>) -> Result<T>) -> Result<T> {
    let mut r = Reader::new(buf);
    let v = parse(&mut r)?;
    r.finish()?;
    Ok(v)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjectInfoRequestKind {
    LatestObjectInfo,
    PastObjectInfoDebug(u64),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ObjectInfoRequest {
    pub object_id: ObjectId,
    pub generate_layout: bool,
    pub request_kind: ObjectInfoRequestKind,
}

impl ObjectInfoRequest {
    pub fn parse(buf: &[u8]) -> Result<ObjectInfoRequest> {
        parse_exact(buf, |r| {
            let object_id = *ObjectId::parse(r)?;
            // `LayoutGenerationOption`: `Generate` is variant 0.
            let generate_layout = match r.variant()? {
                0 => true,
                1 => false,
                tag => {
                    return Err(ParseError::UnknownVariant {
                        ty: "LayoutGenerationOption",
                        tag,
                    });
                }
            };
            let request_kind = match r.variant()? {
                0 => ObjectInfoRequestKind::LatestObjectInfo,
                1 => ObjectInfoRequestKind::PastObjectInfoDebug(r.u64()?),
                tag => {
                    return Err(ParseError::UnknownVariant {
                        ty: "ObjectInfoRequestKind",
                        tag,
                    });
                }
            };
            Ok(ObjectInfoRequest {
                object_id,
                generate_layout,
                request_kind,
            })
        })
    }

    pub fn write(&self, w: &mut Writer<'_>) {
        w.raw(&self.object_id.0);
        w.u8(u8::from(!self.generate_layout));
        match self.request_kind {
            ObjectInfoRequestKind::LatestObjectInfo => w.u8(0),
            ObjectInfoRequestKind::PastObjectInfoDebug(version) => {
                w.u8(1);
                w.u64(version);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TransactionInfoRequest {
    pub transaction_digest: Digest,
}

impl TransactionInfoRequest {
    pub fn parse(buf: &[u8]) -> Result<TransactionInfoRequest> {
        parse_exact(buf, |r| {
            Ok(TransactionInfoRequest {
                transaction_digest: *Digest::parse(r)?,
            })
        })
    }

    pub fn write(&self, w: &mut Writer<'_>) {
        w.digest(&self.transaction_digest);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CheckpointRequest {
    /// The latest checkpoint when absent.
    pub sequence_number: Option<u64>,
    pub request_content: bool,
}

impl CheckpointRequest {
    pub fn parse(buf: &[u8]) -> Result<CheckpointRequest> {
        parse_exact(buf, |r| {
            Ok(CheckpointRequest {
                sequence_number: r.option_u64()?,
                request_content: r.bool()?,
            })
        })
    }

    pub fn write(&self, w: &mut Writer<'_>) {
        w.option_u64(self.sequence_number);
        w.bool(self.request_content);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CheckpointRequestV2 {
    /// The latest checkpoint when absent.
    pub sequence_number: Option<u64>,
    pub request_content: bool,
    /// The certified checkpoint if true, else the locally built one.
    pub certified: bool,
}

impl CheckpointRequestV2 {
    pub fn parse(buf: &[u8]) -> Result<CheckpointRequestV2> {
        parse_exact(buf, |r| {
            Ok(CheckpointRequestV2 {
                sequence_number: r.option_u64()?,
                request_content: r.bool()?,
                certified: r.bool()?,
            })
        })
    }

    pub fn write(&self, w: &mut Writer<'_>) {
        w.option_u64(self.sequence_number);
        w.bool(self.request_content);
        w.bool(self.certified);
    }
}

/// Carries a single ignored bool in the reference, so that it is not empty.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SystemStateRequest {
    pub unused: bool,
}

impl SystemStateRequest {
    pub fn parse(buf: &[u8]) -> Result<SystemStateRequest> {
        parse_exact(buf, |r| Ok(SystemStateRequest { unused: r.bool()? }))
    }

    pub fn write(&self, w: &mut Writer<'_>) {
        w.bool(self.unused);
    }
}
