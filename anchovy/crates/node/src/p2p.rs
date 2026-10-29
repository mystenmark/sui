// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The validator's p2p endpoint, as sui validators reach it: an anemo
//! server on its p2p address, under its network key, serving sui's
//! `Randomness` service, through which they push the random beacon's
//! signatures (`docs/randomness_reference.md` R5.3), and its `StateSync`
//! service, through which the running epoch's checkpoint server answers
//! for committed checkpoints (`validator::state_sync`). Other routes answer
//! not found.
//!
//! It follows the certified chain from the running epoch's committee: what
//! they have certified past what this validator has goes to the epoch's
//! synced scheduler (`synced`), which verifies it.
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
use sui_types::digests::{CheckpointContentsDigest, CheckpointDigest};
use sui_types::messages_checkpoint::{CertifiedCheckpointSummary, VersionedFullCheckpointContents};
use sui_types::transaction::{TransactionDataAPI as _, TransactionKind};
use tokio::sync::oneshot;
use validator::current::CurrentEpoch;
use validator::pipeline::items::GateInput;
use validator::randomness::{BeaconInput, SendSignaturesRequest};
use validator::state_sync::{self, Query, SyncRequest, Wanted};
use workqueue::Queue;

/// sui's limits on a p2p request frame, and a response frame.
const MAX_REQUEST: usize = 1 << 20;
const MAX_RESPONSE: usize = 128 << 20;

/// How often this member pushes its signatures to its peers: the
/// reference's `send_partial_signatures_interval`.
const PUSH_INTERVAL: Duration = Duration::from_secs(5);

/// How often peers are asked how far they have certified, as the
/// reference's state sync asks.
const SYNC_INTERVAL: Duration = Duration::from_secs(5);

/// How many summaries are fetched before they are handed over.
const SYNC_BATCH: u64 = 16;

/// How often the gate is asked which round it waits for. A round waited
/// for at two asks in a row is fetched: pushes come within moments.
const ASK_INTERVAL: Duration = Duration::from_secs(1);

/// The p2p endpoint: serves until dropped.
pub struct P2p {
    pub network: Network,
    fetcher: tokio::task::JoinHandle<()>,
    pusher: tokio::task::JoinHandle<()>,
    follower: tokio::task::JoinHandle<()>,
}

impl P2p {
    /// Stops serving, its port free once this returns.
    pub async fn shutdown(self) {
        self.abort();
        let _ = self.network.shutdown().await;
    }

    fn abort(&self) {
        self.fetcher.abort();
        self.pusher.abort();
        self.follower.abort();
    }
}

impl Drop for P2p {
    fn drop(&mut self) {
        self.abort();
    }
}

/// Starts the endpoint at `address`, under the Ed25519 network key
/// `network_key`, on the chain `chain_identifier` names, in the current
/// runtime. Pushes, and fetched rounds, go to the running epoch's
/// collector, and state sync queries to its checkpoint server.
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
    let router = Router::new()
        .route("/sui.Randomness/SendSignatures", route)
        .route(
            "/sui.StateSync/PushCheckpointSummary",
            tower::service_fn(push_checkpoint_summary),
        )
        .route(
            "/sui.StateSync/GetCheckpointSummary",
            serve(current.clone(), |request| match request {
                GetCheckpointSummaryRequest::Latest => Query::Latest,
                GetCheckpointSummaryRequest::ByDigest(digest) => {
                    Query::ByDigest(Digest::new(digest.into_inner()))
                }
                GetCheckpointSummaryRequest::BySequenceNumber(seq) => Query::BySequenceNumber(seq),
            }),
        )
        .route(
            "/sui.StateSync/GetCheckpointContents",
            serve(current.clone(), |digest: CheckpointContentsDigest| {
                Query::Contents {
                    digest: Digest::new(digest.into_inner()),
                    versioned: false,
                }
            }),
        )
        .route(
            "/sui.StateSync/GetCheckpointContentsV2",
            serve(current.clone(), |digest: CheckpointContentsDigest| {
                Query::Contents {
                    digest: Digest::new(digest.into_inner()),
                    versioned: true,
                }
            }),
        )
        .route(
            "/sui.StateSync/GetCheckpointAvailability",
            serve(current.clone(), |()| Query::Availability),
        );
    let mut config = anemo::Config::default();
    config.max_request_frame_size = Some(MAX_REQUEST);
    config.max_response_frame_size = Some(MAX_RESPONSE);
    // sui's `ChainIdentifier` displays as its first four bytes.
    let server_name = format!(
        "sui-{:08x}",
        u32::from_be_bytes(chain_identifier.bytes[..4].try_into().expect("4 bytes"))
    );
    let network = Network::bind(address)
        .server_name(server_name)
        .private_key(network_key)
        .config(config)
        .start(router)?;
    let fetcher = tokio::spawn(fetch_missing(network.clone(), current.clone()));
    let pusher = tokio::spawn(push_signatures(network.clone(), current.clone()));
    let follower = tokio::spawn(follow_chain(network.clone(), current));
    Ok(P2p {
        network,
        fetcher,
        pusher,
        follower,
    })
}

/// A `StateSync` route: its request decoded as `T` and made a query, which
/// the running epoch's checkpoint server answers. Waits while no epoch runs:
/// a peer's first query checks this validator's chain, and one that fails
/// leaves it out of the peer's state sync until it reconnects.
fn serve<T>(
    current: CurrentEpoch,
    query: fn(T) -> Query,
) -> impl tower::Service<
    Request<Bytes>,
    Response = Response<Bytes>,
    Error = Infallible,
    Future: Send + 'static,
> + Clone
+ Send
+ 'static
where
    T: serde::de::DeserializeOwned + Send + Sync + 'static,
{
    tower::service_fn(move |request: Request<Bytes>| {
        let current = current.clone();
        async move {
            let mut rpc = anemo::rpc::server::Rpc::new(BcsSnappy::<(), T>::default(), Encoded);
            let service = tower::service_fn(move |request: Request<T>| {
                let current = current.clone();
                async move {
                    let (reply, answer) = oneshot::channel();
                    let request = state_sync::Request {
                        query: query(request.into_body()),
                        reply,
                    };
                    current
                        .get()
                        .await
                        .checkpoint_queries
                        .try_push(request)
                        .map_err(|_| Status::new(StatusCode::InternalServerError))?;
                    let bytes = answer
                        .await
                        .map_err(|_| Status::new(StatusCode::InternalServerError))?;
                    Ok::<_, Status>(Response::new(bytes))
                }
            });
            Ok::<_, Infallible>(rpc.unary(service, request).await)
        }
    })
}

/// Pushed summaries are taken and let go: nothing syncs from them yet.
async fn push_checkpoint_summary(request: Request<Bytes>) -> Result<Response<Bytes>, Infallible> {
    let mut rpc = anemo::rpc::server::Rpc::new(
        BcsSnappy::<(), CertifiedCheckpointSummary>::default(),
        BcsSnappy::<(), ()>::default(),
    );
    let service = tower::service_fn(|_: Request<CertifiedCheckpointSummary>| async {
        Ok::<_, Status>(Response::new(()))
    });
    Ok(rpc.unary(service, request).await)
}

/// Pushes what this member signed, or the full signature once known, to
/// every other member, every `PUSH_INTERVAL`, as the reference's
/// `send_signatures_task`: the collector says what.
async fn push_signatures(network: Network, current: CurrentEpoch) {
    let own = PeerId(network.peer_id().0);
    let mut interval = tokio::time::interval(PUSH_INTERVAL);
    loop {
        interval.tick().await;
        let Some(epoch) = current.now() else {
            continue;
        };
        let (answer, outgoing) = oneshot::channel();
        if epoch
            .beacon
            .try_push(BeaconInput::Outgoing(answer))
            .is_err()
        {
            continue;
        }
        let Ok(outgoing) = outgoing.await else {
            continue;
        };
        if outgoing.is_empty() {
            continue;
        }
        for (key, address) in epoch.network_keys.iter().zip(&epoch.p2p_addresses) {
            let peer = PeerId(*key);
            if peer == own {
                continue;
            }
            let (network, address, outgoing) = (network.clone(), address.clone(), outgoing.clone());
            tokio::spawn(async move {
                if !connect(&network, peer, &address).await {
                    return;
                }
                for request in outgoing {
                    let _: Option<()> =
                        call(&network, peer, "/sui.Randomness/SendSignatures", request).await;
                }
            });
        }
    }
}

/// Whether `peer` is connected, connecting to it at `address` (a p2p
/// multiaddr) if not.
async fn connect(network: &Network, peer: PeerId, address: &str) -> bool {
    if network.peer(peer).is_some() {
        return true;
    }
    let Some(address) = anemo_address(address) else {
        return false;
    };
    let connect = network.connect_with_peer_id(address, peer);
    matches!(
        tokio::time::timeout(PUSH_INTERVAL, connect).await,
        Ok(Ok(_))
    )
}

/// Follows the certified chain: every `SYNC_INTERVAL`, and at once again
/// while a round takes summaries, fetches what the running epoch's synced
/// scheduler wants from the first committee member that has it.
async fn follow_chain(network: Network, current: CurrentEpoch) {
    let mut interval = tokio::time::interval(SYNC_INTERVAL);
    loop {
        interval.tick().await;
        // Not the handle itself: it holds the store, which must be let go
        // as the node stops, whatever a peer keeps this waiting for.
        let Some((sync, members)) = current.now().map(|epoch| {
            let members: Vec<(PeerId, String)> = epoch
                .network_keys
                .iter()
                .map(|key| PeerId(*key))
                .zip(epoch.p2p_addresses.iter().cloned())
                .collect();
            (epoch.checkpoint_sync.clone(), members)
        }) else {
            continue;
        };
        while follow_round(&network, &sync, &members).await {}
    }
}

/// Fetches up to `SYNC_BATCH` summaries the scheduler wants from the first
/// member that has them, trying the next if they are rejected. Returns
/// whether the scheduler took any.
async fn follow_round(
    network: &Network,
    sync: &Queue<SyncRequest>,
    members: &[(PeerId, String)],
) -> bool {
    let Some(mut wanted) = hand_over(sync, Vec::new()).await else {
        return false;
    };
    let own = PeerId(network.peer_id().0);
    for &(peer, ref address) in members {
        let Some(next) = wanted.next.filter(|_| wanted.room > 0) else {
            return false;
        };
        if peer == own || !connect(network, peer, address).await {
            continue;
        }
        let Some(highest) = highest_certified(network, peer).await else {
            continue;
        };
        let last = highest.min(next + wanted.room.min(SYNC_BATCH) - 1);
        let mut summaries = Vec::new();
        for seq in next..=last {
            let Some(summary) = certified_summary(network, peer, seq).await else {
                break;
            };
            summaries.push(summary);
        }
        if summaries.is_empty() {
            continue;
        }
        let Some(after) = hand_over(sync, summaries).await else {
            return false;
        };
        if after.next != wanted.next {
            return true;
        }
        wanted = after;
    }
    false
}

/// Hands `summaries` to the epoch's synced scheduler; what it wants next,
/// unless the epoch has stopped.
async fn hand_over(sync: &Queue<SyncRequest>, summaries: Vec<Vec<u8>>) -> Option<Wanted> {
    let (reply, wanted) = oneshot::channel();
    sync.try_push(SyncRequest { summaries, reply }).ok()?;
    wanted.await.ok()
}

/// The highest checkpoint `peer` has certified.
async fn highest_certified(network: &Network, peer: PeerId) -> Option<u64> {
    let (highest, _lowest): (CertifiedCheckpointSummary, u64) = call(
        network,
        peer,
        "/sui.StateSync/GetCheckpointAvailability",
        (),
    )
    .await?;
    Some(highest.sequence_number)
}

/// `peer`'s certified summary of checkpoint `seq`, as sent:
/// `CertifiedCheckpointSummary` BCS.
async fn certified_summary(network: &Network, peer: PeerId, seq: u64) -> Option<Vec<u8>> {
    let bytes = call_raw(
        network,
        peer,
        "/sui.StateSync/GetCheckpointSummary",
        GetCheckpointSummaryRequest::BySequenceNumber(seq),
    )
    .await?;
    // `Option<CertifiedCheckpointSummary>`.
    match bytes.split_first() {
        Some((1, summary)) => Some(summary.to_vec()),
        _ => None,
    }
}

/// A p2p multiaddr (`/ip4/<host>/udp/<port>`, or `/ip6` or `/dns`) as a
/// host and port.
fn anemo_address(multiaddr: &str) -> Option<(String, u16)> {
    let parts: Vec<&str> = multiaddr.split('/').collect();
    let [
        "",
        "ip4" | "ip6" | "dns" | "dns4" | "dns6",
        host,
        "udp",
        port,
        ..,
    ] = parts[..]
    else {
        return None;
    };
    Some((host.to_owned(), port.parse().ok()?))
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
#[derive(serde::Serialize, serde::Deserialize)]
enum GetCheckpointSummaryRequest {
    Latest,
    ByDigest(CheckpointDigest),
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
    bcs::from_bytes(&call_raw(network, peer, route, request).await?).ok()
}

/// A unary call of `route` on `peer`: the response's BCS, decompressed.
async fn call_raw<T: serde::Serialize>(
    network: &Network,
    peer: PeerId,
    route: &str,
    request: T,
) -> Option<Vec<u8>> {
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
    decompress(response.body(), MAX_RESPONSE).ok()
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
        decode(&buf, MAX_REQUEST)
    }
}

/// `buf`, snappy-framed BCS, decoded from at most `max` bytes.
fn decode<U: serde::de::DeserializeOwned>(buf: &Bytes, max: usize) -> std::io::Result<U> {
    bcs::from_bytes(&decompress(buf, max)?).map_err(std::io::Error::other)
}

/// `buf`, snappy-framed, decompressed to at most `max` bytes.
fn decompress(buf: &Bytes, max: usize) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    snap::read::FrameDecoder::new(&buf[..])
        .take(max as u64)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// A response the checkpoint server encoded already, as `BcsSnappy` would.
struct Encoded;

struct PassThrough;

impl Encoder for PassThrough {
    type Item = Bytes;
    type Error = Infallible;

    fn encode(&mut self, item: Bytes) -> Result<Bytes, Infallible> {
        Ok(item)
    }
}

impl Decoder for PassThrough {
    type Item = Bytes;
    type Error = Infallible;

    fn decode(&mut self, buf: Bytes) -> Result<Bytes, Infallible> {
        Ok(buf)
    }
}

impl Codec for Encoded {
    type Encode = Bytes;
    type Decode = Bytes;
    type Encoder = PassThrough;
    type Decoder = PassThrough;

    fn encoder(&mut self) -> PassThrough {
        PassThrough
    }

    fn decoder(&mut self) -> PassThrough {
        PassThrough
    }

    fn format_name(&self) -> &'static str {
        "bcs"
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
