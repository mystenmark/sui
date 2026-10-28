// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The validator's p2p endpoint, as sui validators reach it: an anemo
//! server on its p2p address, under its network key, serving only sui's
//! `Randomness` service, through which they push the random beacon's
//! signatures (`docs/randomness_reference.md` R5.3). Other routes answer
//! not found.
//!
//! Peers stop pushing a round once they certify it, so a round that
//! completed while this validator was down never arrives. For those, it
//! fetches the round's checkpoint from a connected peer through sui's state
//! sync service and takes the update's bytes, which the collector verifies
//! as any full signature.

use std::convert::Infallible;
use std::io::Read;
use std::marker::PhantomData;
use std::net::SocketAddr;

use anemo::rpc::Status;
use anemo::rpc::codec::{Codec, Decoder, Encoder};
use anemo::types::response::StatusCode;
use std::time::Duration;

use anemo::{Network, PeerId, Request, Response, Router};
use bytes::Bytes;
use fastcrypto::groups::bls12381::G1Element;
use messages::base::Digest;
use sui_types::messages_checkpoint::{CertifiedCheckpointSummary, VersionedFullCheckpointContents};
use sui_types::transaction::{TransactionDataAPI as _, TransactionKind};
use tokio::sync::oneshot;
use validator::current::CurrentEpoch;
use validator::pipeline::items::GateInput;
use validator::randomness::{BeaconInput, SendSignaturesRequest};

/// sui's limits on a p2p request frame, and a response frame.
const MAX_REQUEST: usize = 1 << 20;
const MAX_RESPONSE: usize = 128 << 20;

/// How often the gate is asked which round it waits for. A round waited
/// for at two asks in a row is fetched: pushes come within moments.
const ASK_INTERVAL: Duration = Duration::from_secs(1);

/// The p2p endpoint: serves until dropped.
pub struct P2p {
    pub network: Network,
    fetcher: tokio::task::JoinHandle<()>,
}

impl Drop for P2p {
    fn drop(&mut self) {
        self.fetcher.abort();
    }
}

/// Starts the endpoint at `address`, under the Ed25519 network key
/// `network_key`, on the chain `chain_identifier` names, in the current
/// runtime. Pushes, and fetched rounds, go to the running epoch's
/// collector.
///
/// # Errors
/// If `address` cannot be bound.
pub fn start(
    address: SocketAddr,
    network_key: [u8; 32],
    chain_identifier: &Digest,
    current: CurrentEpoch,
) -> anemo::Result<P2p> {
    let serving = current.clone();
    let route = tower::service_fn(move |request: Request<Bytes>| {
        let current = serving.clone();
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
    config.max_response_frame_size = Some(MAX_RESPONSE);
    // sui's `ChainIdentifier` displays as its first four bytes.
    let server_name = format!(
        "sui-{}",
        chain_identifier.bytes[..4]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let network = Network::bind(address)
        .server_name(server_name)
        .private_key(network_key)
        .config(config)
        .start(router)?;
    let fetcher = tokio::spawn(fetch_missing(network.clone(), current));
    Ok(P2p { network, fetcher })
}

/// Asks the running epoch's gate which round it waits for; a round waited
/// for at two asks in a row is fetched from a connected member.
async fn fetch_missing(network: Network, current: CurrentEpoch) {
    let mut last = None;
    let mut interval = tokio::time::interval(ASK_INTERVAL);
    loop {
        interval.tick().await;
        let Some(epoch) = current.now() else {
            continue;
        };
        let (answer, waiting) = oneshot::channel();
        if epoch
            .randomness_gate
            .try_push(GateInput::Waiting(answer))
            .is_err()
        {
            continue;
        }
        let Ok(waiting) = waiting.await else {
            continue;
        };
        let stuck = waiting.filter(|w| last == Some(*w));
        last = waiting;
        let Some((round, seq)) = stuck else {
            continue;
        };
        for peer in network.peers() {
            let Some(party) = epoch.network_keys.iter().position(|key| *key == peer.0) else {
                continue;
            };
            let Some(sig) = fetch_round(&network, peer, epoch.epoch.params.epoch, round, seq).await
            else {
                continue;
            };
            let (taken, answer) = oneshot::channel();
            let input = BeaconInput::Signatures {
                party: u16::try_from(party).expect("fewer than 2^16 members"),
                request: SendSignaturesRequest {
                    epoch: epoch.epoch.params.epoch,
                    round,
                    partial_sigs: Vec::new(),
                    sig: Some(sig),
                },
                taken,
            };
            if epoch.beacon.try_push(input).is_ok() {
                let _ = answer.await;
            }
            break;
        }
    }
}

/// sui's `GetCheckpointSummaryRequest`.
#[derive(serde::Serialize)]
enum GetCheckpointSummaryRequest {
    #[allow(dead_code)]
    Latest,
    #[allow(dead_code)]
    ByDigest([u8; 32]),
    BySequenceNumber(u64),
}

/// The signature in round `round`'s update, as `peer` has checkpoint `seq`,
/// if it has it certified. Unverified: the collector verifies it.
async fn fetch_round(
    network: &Network,
    peer: PeerId,
    epoch: u64,
    round: u64,
    seq: u64,
) -> Option<G1Element> {
    let summary: Option<CertifiedCheckpointSummary> = call(
        network,
        peer,
        "/sui.StateSync/GetCheckpointSummary",
        GetCheckpointSummaryRequest::BySequenceNumber(seq),
    )
    .await?;
    let contents: Option<VersionedFullCheckpointContents> = call(
        network,
        peer,
        "/sui.StateSync/GetCheckpointContentsV2",
        summary?.content_digest,
    )
    .await?;
    contents?.iter().find_map(|data| {
        let TransactionKind::RandomnessStateUpdate(update) =
            data.transaction.data().transaction_data().kind()
        else {
            return None;
        };
        (update.epoch == epoch && update.randomness_round.0 == round)
            .then(|| bcs::from_bytes(&update.random_bytes).ok())
            .flatten()
    })
}

/// A unary call of `route` on `peer`; none if it fails.
async fn call<T: serde::Serialize, U: serde::de::DeserializeOwned>(
    network: &Network,
    peer: PeerId,
    route: &str,
    request: T,
) -> Option<U> {
    let mut peer = network.peer(peer)?;
    let body = SnappyEncoder(PhantomData).encode(request).ok()?;
    let mut request = Request::new(body);
    *request.route_mut() = route.into();
    let response = tokio::time::timeout(ASK_INTERVAL * 10, peer.rpc(request))
        .await
        .ok()?
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    decode(response.into_body(), MAX_RESPONSE).ok()
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
        decode(buf, MAX_REQUEST)
    }
}

/// `buf`, snappy-framed BCS, decompressed to at most `max` bytes.
fn decode<U: serde::de::DeserializeOwned>(buf: Bytes, max: usize) -> std::io::Result<U> {
    let mut bytes = Vec::new();
    snap::read::FrameDecoder::new(&buf[..])
        .take(max as u64)
        .read_to_end(&mut bytes)?;
    bcs::from_bytes(&bytes).map_err(std::io::Error::other)
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
