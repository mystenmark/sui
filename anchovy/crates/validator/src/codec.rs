// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The reference's `BcsCodec` (`application/grpc+bcs`): a message is its
//! BCS bytes in the gRPC frame, uncompressed.

use std::marker::PhantomData;

use bytes::{Buf, BufMut, Bytes};
use messages::fast::{Bump, Writer};
use messages::grpc::{
    CheckpointRequest, CheckpointRequestV2, ObjectInfoRequest, SystemStateRequest,
    TransactionInfoRequest,
};
use tonic::Status;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};

/// A message already serialized as BCS; handlers write responses into one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Encoded(pub Bytes);

pub trait BcsEncode {
    fn encode(&self, dst: &mut EncodeBuf<'_>);
}

pub trait BcsDecode: Sized {
    fn decode(src: &mut DecodeBuf<'_>) -> Result<Self, Status>;
}

impl BcsEncode for Encoded {
    fn encode(&self, dst: &mut EncodeBuf<'_>) {
        dst.put_slice(&self.0);
    }
}

impl BcsDecode for Encoded {
    fn decode(src: &mut DecodeBuf<'_>) -> Result<Self, Status> {
        Ok(Encoded(src.copy_to_bytes(src.remaining())))
    }
}

macro_rules! bcs_request {
    ($($ty:ident),*) => {$(
        impl BcsEncode for $ty {
            fn encode(&self, dst: &mut EncodeBuf<'_>) {
                let bump = Bump::with_capacity(64);
                let mut w = Writer::new_in(&bump, 64);
                self.write(&mut w);
                dst.put_slice(w.finish_bytes());
            }
        }

        impl BcsDecode for $ty {
            fn decode(src: &mut DecodeBuf<'_>) -> Result<Self, Status> {
                // tonic hands a decoder the whole message in one buffer.
                debug_assert_eq!(src.chunk().len(), src.remaining());
                let request = $ty::parse(src.chunk()).map_err(|e| {
                    Status::invalid_argument(format!(concat!(stringify!($ty), ": {:?}"), e))
                })?;
                src.advance(src.remaining());
                Ok(request)
            }
        }
    )*};
}

bcs_request!(
    ObjectInfoRequest,
    TransactionInfoRequest,
    CheckpointRequest,
    CheckpointRequestV2,
    SystemStateRequest
);

/// Encodes `T` and decodes `U`: on a server `T` is the response, on a
/// client the request.
pub struct BcsCodec<T, U>(PhantomData<(T, U)>);

impl<T, U> Default for BcsCodec<T, U> {
    fn default() -> Self {
        BcsCodec(PhantomData)
    }
}

impl<T, U> Codec for BcsCodec<T, U>
where
    T: BcsEncode + Send + 'static,
    U: BcsDecode + Send + 'static,
{
    type Encode = T;
    type Decode = U;
    type Encoder = BcsEncoder<T>;
    type Decoder = BcsDecoder<U>;

    fn encoder(&mut self) -> BcsEncoder<T> {
        BcsEncoder(PhantomData)
    }

    fn decoder(&mut self) -> BcsDecoder<U> {
        BcsDecoder(PhantomData)
    }
}

pub struct BcsEncoder<T>(PhantomData<T>);

impl<T: BcsEncode> Encoder for BcsEncoder<T> {
    type Item = T;
    type Error = Status;

    fn encode(&mut self, item: T, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        item.encode(dst);
        Ok(())
    }
}

pub struct BcsDecoder<U>(PhantomData<U>);

impl<U: BcsDecode> Decoder for BcsDecoder<U> {
    type Item = U;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<U>, Status> {
        U::decode(src).map(Some)
    }
}
