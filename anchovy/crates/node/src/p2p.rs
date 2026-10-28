// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The validator's p2p endpoint, as sui validators reach it: an anemo
//! server on its p2p address, under its network key, serving only sui's
//! `Randomness` service, through which they push the random beacon's
//! signatures (`docs/randomness_reference.md` R5.3). Other routes answer
//! not found.

use std::convert::Infallible;
use std::io::Read;
use std::marker::PhantomData;
use std::net::SocketAddr;

use anemo::rpc::Status;
use anemo::rpc::codec::{Codec, Decoder, Encoder};
use anemo::types::response::StatusCode;
use anemo::{Network, Request, Response, Router};
use bytes::Bytes;
use messages::base::Digest;
use tokio::sync::oneshot;
use validator::current::CurrentEpoch;
use validator::randomness::{BeaconInput, SendSignaturesRequest};

/// sui's limit on a p2p request frame.
const MAX_REQUEST: usize = 1 << 20;

/// Starts the endpoint at `address`, under the Ed25519 network key
/// `network_key`, on the chain `chain_identifier` names. Pushes go to the
/// running epoch's collector. It serves until dropped.
///
/// # Errors
/// If `address` cannot be bound.
pub fn start(
    address: SocketAddr,
    network_key: [u8; 32],
    chain_identifier: &Digest,
    current: CurrentEpoch,
) -> anemo::Result<Network> {
    let route = tower::service_fn(move |request: Request<Bytes>| {
        let current = current.clone();
        async move {
            let mut rpc = anemo::rpc::server::Rpc::new(
                BcsSnappy::<(), SendSignaturesRequest>::default(),
                BcsSnappy::<(), SendSignaturesRequest>::default(),
            );
            let service =
                tower::service_fn(move |request| send_signatures(current.clone(), request));
            Ok::<_, Infallible>(rpc.unary(service, request).await)
        }
    });
    let router = Router::new().route("/sui.Randomness/SendSignatures", route);
    let mut config = anemo::Config::default();
    config.max_request_frame_size = Some(MAX_REQUEST);
    // sui's `ChainIdentifier` displays as its first four bytes.
    let server_name = format!(
        "sui-{}",
        chain_identifier.bytes[..4]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    Network::bind(address)
        .server_name(server_name)
        .private_key(network_key)
        .config(config)
        .start(router)
}

/// Pushes a peer's signatures to the running epoch's collector, answering
/// once it has taken them; dropped if the peer is not a member.
async fn send_signatures(
    current: CurrentEpoch,
    request: Request<SendSignaturesRequest>,
) -> Result<Response<()>, Status> {
    let not_member = || Status::new(StatusCode::NotFound);
    let peer = request.peer_id().ok_or_else(not_member)?.0;
    let epoch = current.now().ok_or_else(not_member)?;
    let party = epoch
        .network_keys
        .iter()
        .position(|key| *key == peer)
        .ok_or_else(not_member)?;
    let (taken, answer) = oneshot::channel();
    let input = BeaconInput::Signatures {
        party: u16::try_from(party).expect("fewer than 2^16 members"),
        request: request.into_body(),
        taken,
    };
    if epoch.beacon.try_push(input).is_err() {
        return Err(Status::new(StatusCode::InternalServerError));
    }
    answer
        .await
        .map_err(|_| Status::new(StatusCode::InternalServerError))?;
    Ok(Response::new(()))
}

/// BCS framed with snappy, sui's p2p codec (`BcsSnappyCodec`).
struct BcsSnappy<T, U>(PhantomData<(T, U)>);

impl<T, U> Default for BcsSnappy<T, U> {
    fn default() -> Self {
        BcsSnappy(PhantomData)
    }
}

struct SnappyEncoder<T>(PhantomData<T>);
struct SnappyDecoder<U>(PhantomData<U>);

impl<T: serde::Serialize> Encoder for SnappyEncoder<T> {
    type Item = T;
    type Error = bcs::Error;

    fn encode(&mut self, item: T) -> Result<Bytes, bcs::Error> {
        let mut buf = Vec::new();
        let mut encoder = snap::write::FrameEncoder::new(&mut buf);
        bcs::serialize_into(&mut encoder, &item)?;
        drop(encoder);
        Ok(buf.into())
    }
}

impl<U: serde::de::DeserializeOwned> Decoder for SnappyDecoder<U> {
    type Item = U;
    type Error = std::io::Error;

    fn decode(&mut self, buf: Bytes) -> Result<U, std::io::Error> {
        let mut bytes = Vec::new();
        snap::read::FrameDecoder::new(&buf[..])
            .take(MAX_REQUEST as u64)
            .read_to_end(&mut bytes)?;
        bcs::from_bytes(&bytes).map_err(std::io::Error::other)
    }
}

impl<T, U> Codec for BcsSnappy<T, U>
where
    T: serde::Serialize + Send + 'static,
    U: serde::de::DeserializeOwned + Send + 'static,
{
    type Encode = T;
    type Decode = U;
    type Encoder = SnappyEncoder<T>;
    type Decoder = SnappyDecoder<U>;

    fn encoder(&mut self) -> SnappyEncoder<T> {
        SnappyEncoder(PhantomData)
    }

    fn decoder(&mut self) -> SnappyDecoder<U> {
        SnappyDecoder(PhantomData)
    }

    fn format_name(&self) -> &'static str {
        "bcs"
    }
}
