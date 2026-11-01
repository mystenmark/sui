// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! What lives for good and what lives for an epoch
//! (`docs/reconfiguration_design.md`). Perpetual: the store, the handle to
//! the running epoch, and state sync. Per epoch: everything in `Epoch`,
//! which dropping stops and joins.

use std::path::PathBuf;
use std::sync::Arc;

use aggregator::Committee;
use checkpoint::ProtocolKey;
use consensus::TransactionClient;
use execution::system_state::{Member, SystemState};
use fastcrypto::hash::MultisetHash;
use messages::arena::BumpAlloc;
use messages::base::Digest;
use messages::checkpoint::{CertifiedCheckpointSummary, CheckpointCommitmentKind};
use messages::fast::Bump;
use messages::reader::Reader;
use state_hasher::{StateHash, StateHashes};
use sui_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use validator::current::CurrentEpoch;
use validator::epoch::EpochState;
use validator::epoch_store::EpochStore;
use validator::pipeline::items::CertifiedCheckpoint;
use validator::pipeline::tables::BuiltCheckpoint;
use validator::tls::NetworkKey;
use workqueue::Queue;

use crate::{ConsensusOutput, Pipeline};

/// What every epoch is built from, as the epoch's start agreed it. Stored
/// whole as the epoch's start record: the one durable point at which the
/// node is in this epoch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpochRecord {
    pub epoch: u64,
    pub protocol_version: u64,
    pub reference_gas_price: u64,
    pub epoch_start_timestamp_ms: u64,
    pub epoch_duration_ms: u64,
    /// Each member, by protocol key: the checkpoint committee's name and
    /// stake, and consensus's keys and address.
    pub committee: Vec<Member>,
    /// The checkpoint the epoch's first checkpoint follows: the previous
    /// epoch's last, or genesis.
    pub previous_checkpoint: u64,
    /// The live object set's ECMH after `previous_checkpoint`
    /// (`state_hasher::encode`), which the epoch's state hashes continue.
    pub root_state: Vec<u8>,
}

impl EpochRecord {
    /// The epoch the system state object describes, its first checkpoint
    /// following `previous_checkpoint`.
    pub fn new(
        state: execution::system_state::SystemState,
        previous_checkpoint: u64,
        root_state: Vec<u8>,
    ) -> EpochRecord {
        EpochRecord {
            epoch: state.epoch,
            protocol_version: state.protocol_version,
            reference_gas_price: state.reference_gas_price,
            epoch_start_timestamp_ms: state.epoch_start_timestamp_ms,
            epoch_duration_ms: state.epoch_duration_ms,
            committee: state.committee,
            previous_checkpoint,
            root_state,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let committee: Vec<_> = self
            .committee
            .iter()
            .map(|m| {
                (
                    m.name.to_vec(),
                    m.stake,
                    m.consensus_key,
                    m.network_key,
                    &m.address,
                    &m.hostname,
                    &m.p2p_address,
                )
            })
            .collect();
        bcs::to_bytes(&(
            self.epoch,
            self.protocol_version,
            self.reference_gas_price,
            self.epoch_start_timestamp_ms,
            self.epoch_duration_ms,
            self.previous_checkpoint,
            committee,
            &self.root_state,
        ))
        .expect("a record encodes")
    }

    /// # Panics
    /// If `bytes` is not what `encode` wrote.
    pub fn decode(bytes: &[u8]) -> EpochRecord {
        type Encoded = (
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            Vec<(Vec<u8>, u64, [u8; 32], [u8; 32], String, String, String)>,
            Vec<u8>,
        );
        let (
            epoch,
            protocol_version,
            reference_gas_price,
            epoch_start_timestamp_ms,
            epoch_duration_ms,
            previous_checkpoint,
            committee,
            root_state,
        ): Encoded = bcs::from_bytes(bytes).expect("an epoch start record decodes");
        EpochRecord {
            epoch,
            protocol_version,
            reference_gas_price,
            epoch_start_timestamp_ms,
            epoch_duration_ms,
            previous_checkpoint,
            committee: committee
                .into_iter()
                .map(
                    |(name, stake, consensus_key, network_key, address, hostname, p2p_address)| {
                        Member {
                            name: name.try_into().expect("a 96-byte protocol key"),
                            stake,
                            consensus_key,
                            network_key,
                            address,
                            hostname,
                            p2p_address,
                        }
                    },
                )
                .collect(),
            root_state,
        }
    }

    /// Each member's protocol key and stake: the checkpoint committee.
    pub fn stakes(&self) -> impl Iterator<Item = ([u8; 96], u64)> + '_ {
        self.committee.iter().map(|m| (m.name, m.stake))
    }

    /// The current epoch's record.
    ///
    /// # Panics
    /// If the store cannot be read or has no genesis.
    pub fn load(store: &store::Store) -> EpochRecord {
        let (epoch, bytes) = store
            .epoch_start()
            .expect("the store reads")
            .expect("genesis wrote the first epoch's start");
        let record = EpochRecord::decode(&bytes);
        assert_eq!(record.epoch, epoch, "an epoch's record is keyed by it");
        record
    }
}

/// Writes the genesis checkpoint, `certified` its certified summary, and the
/// first epoch's start record, read from the system state genesis created,
/// unless the store has them. The
/// genesis objects, with digests `objects`, must be in the store.
///
/// # Panics
/// If the store cannot be read or written, or holds no system state.
pub fn genesis(
    store: &store::Store,
    checkpoint: &BuiltCheckpoint,
    certified: Vec<u8>,
    objects: impl IntoIterator<Item = [u8; 32]>,
) {
    if store.epoch_start().expect("the store reads").is_some() {
        return;
    }
    assert_eq!(checkpoint.seq, 0, "the genesis checkpoint is checkpoint 0");
    let state = execution::system_state::read(store).expect("genesis made a system state");
    assert_eq!(state.epoch, 0, "genesis starts epoch 0");
    let root = state_hasher::encode(&state_hasher::of_objects(objects));
    store.commit_genesis_checkpoint(
        checkpoint.record(certified),
        EpochRecord::new(state, 0, root).encode(),
    );
}

/// What starting an epoch's consensus takes.
pub struct ConsensusStart {
    pub epoch: Arc<EpochState>,
    pub record: EpochRecord,
    /// Where it delivers, and from which commit.
    pub output: ConsensusOutput,
    /// Where it keeps its data.
    pub dir: PathBuf,
}

/// The node's consensus: started for each epoch, stopped before the node
/// reconfigures.
pub trait Consensus {
    /// Starts the epoch's consensus; its transaction client.
    fn start(&mut self, start: ConsensusStart) -> TransactionClient;

    /// Stops the running epoch's consensus.
    fn stop(&mut self);
}

/// A consensus that is started, and never needs stopping: the tests'.
impl<F: FnMut(&Arc<EpochState>, ConsensusOutput) -> TransactionClient> Consensus for F {
    fn start(&mut self, start: ConsensusStart) -> TransactionClient {
        self(&start.epoch, start.output)
    }

    fn stop(&mut self) {}
}

/// What the node keeps across epochs.
pub struct Perpetual {
    pub store: Arc<store::Store>,
    /// Where each epoch's store is kept, until the epoch after next.
    pub epochs: PathBuf,
    pub current: CurrentEpoch,
    /// To state sync, which owns the other end: certified checkpoints.
    pub state_sync: Queue<CertifiedCheckpoint>,
    pub chain: Chain,
    /// The genesis checkpoint's digest.
    pub chain_identifier: Digest,
    pub keys: ValidatorKeys,
}

/// The validator's keys, as sui's `NodeConfig` holds them. The protocol key
/// (BLS) names it in every committee and signs its checkpoints; consensus
/// signs its blocks with the worker key and secures its network with the
/// network key, both Ed25519.
pub struct ValidatorKeys {
    pub protocol: Arc<ProtocolKey>,
    /// The worker key's Ed25519 secret.
    pub worker: [u8; 32],
    pub network: NetworkKey,
}

/// One epoch: its state, its committee and its pipeline, built from the
/// epoch's start record. Running, it is the current epoch; dropped, it is
/// not, and every one of its workers has stopped.
pub struct Epoch {
    pub state: Arc<EpochState>,
    pub committee: Arc<Committee>,
    pub pipeline: Pipeline,
    current: CurrentEpoch,
}

impl Epoch {
    /// The epoch the store's start record names, its consensus started by
    /// `consensus`, made the current epoch.
    ///
    /// # Panics
    /// If the store cannot be read, or the record's committee is invalid.
    pub fn start(perpetual: &Perpetual, consensus: &mut impl Consensus) -> Epoch {
        let record = EpochRecord::load(&perpetual.store);
        // As the reference's `check_protocol_version`: a binary that cannot
        // run the epoch's version stops rather than diverge.
        let supported = execution::upgrade::supported_protocol_versions(perpetual.chain);
        if !execution::upgrade::is_supported(supported, record.protocol_version) {
            eprintln!(
                "epoch {} runs protocol version {}, outside this binary's {supported:?}",
                record.epoch, record.protocol_version,
            );
            std::process::exit(1);
        }
        let state = Arc::new(EpochState::starting_at(
            perpetual.chain,
            record.protocol_version,
            record.epoch,
            perpetual.chain_identifier,
            record.reference_gas_price,
            u32::try_from(record.committee.len()).expect("a committee of fewer than 2^32"),
            record.epoch_start_timestamp_ms,
            previous_digest(&perpetual.store, record.previous_checkpoint),
            [],
        ));
        let committee = Arc::new(
            Committee::new(record.epoch, record.stakes()).expect("the epoch's committee is valid"),
        );
        let (epoch_store, consensus_output, builder_output) =
            EpochStore::open(&perpetual.epochs, record.epoch);
        let pipeline = Pipeline::start(
            &perpetual.store,
            &state,
            &epoch_store,
            consensus_output,
            builder_output,
            &perpetual.keys.protocol,
            &committee,
            |output| {
                consensus.start(ConsensusStart {
                    epoch: state.clone(),
                    record: record.clone(),
                    output,
                    dir: store::epoch::consensus_dir(&perpetual.epochs, record.epoch),
                })
            },
            perpetual.state_sync.clone(),
            &record,
        );
        perpetual.current.set(pipeline.handle());
        Epoch {
            state,
            committee,
            pipeline,
            current: perpetual.current.clone(),
        }
    }
}

impl Epoch {
    /// No longer current, so requests wait for the next epoch; then every
    /// item ready anywhere in the pipeline is processed. So every
    /// checkpoint certified has been checked against the local one, and
    /// nothing the epoch could still do now is left undone. Items waiting
    /// on keys stay: a validator may move on through certified checkpoints
    /// before it has built them all. Its consensus must have stopped.
    pub fn drain(&self) {
        self.current.clear();
        self.pipeline.drain();
    }
}

impl Drop for Epoch {
    /// No longer current, so requests wait for the next epoch; then the
    /// pipeline drops, stopping every worker.
    fn drop(&mut self) {
        self.current.clear();
    }
}

/// Committed checkpoint `seq`'s summary digest.
fn previous_digest(store: &store::Store, seq: u64) -> [u8; 32] {
    BuiltCheckpoint::decode(
        &store
            .checkpoint(seq)
            .expect("the store reads")
            .expect("the epoch's previous checkpoint is committed"),
    )
    .digest
}

/// Whether the current epoch's last checkpoint has committed: the epoch is
/// over, though the next epoch's start record may not be written yet.
///
/// # Panics
/// If the store cannot be read or has no genesis.
pub fn epoch_ended(store: &store::Store) -> bool {
    let record = EpochRecord::load(store);
    let Some((seq, bytes)) = store.last_checkpoint().expect("the store reads") else {
        return false;
    };
    // The previous epoch's last checkpoint ends an epoch too.
    seq > record.previous_checkpoint && BuiltCheckpoint::decode(&bytes).ends_epoch
}

/// The root after the current epoch's last committed checkpoint, from the
/// epoch's `hashes`: as stored, or folded again from committed effects.
fn committed_root(store: &store::Store, hashes: &StateHashes) -> StateHash {
    let current = EpochRecord::load(store);
    let (last, _) = store
        .last_checkpoint()
        .expect("the store reads")
        .expect("the genesis checkpoint is committed");
    hashes.committed_root(
        store,
        last,
        current.previous_checkpoint,
        &state_hasher::decode(&current.root_state),
    )
}

/// Writes the next epoch's start record, once the current epoch's last
/// checkpoint has committed: from the system state it wrote, following it,
/// and continuing `root`, the root after it. Each is checked against the
/// certified end-of-epoch data first. Idempotent until written; the write
/// is the durable point at which the node is in the next epoch.
///
/// # Panics
/// If the current epoch has not ended, the store cannot be read or
/// written, or the system state is not the next epoch's. On a fork: the
/// next committee, protocol version or root is not what the quorum
/// certified.
pub fn reconfigure(store: &store::Store, root: &StateHash, chain: Chain) {
    assert!(epoch_ended(store), "only an ended epoch is followed");
    let current = EpochRecord::load(store);
    let (last, _) = store
        .last_checkpoint()
        .expect("the store reads")
        .expect("the epoch's last checkpoint is committed");
    let state = execution::system_state::read(store).expect("the store holds the system state");
    assert_eq!(
        state.epoch,
        current.epoch + 1,
        "the system state is the next epoch's"
    );
    let config =
        ProtocolConfig::get_for_version(ProtocolVersion::new(current.protocol_version), chain);
    check_end_of_epoch(store, last, &state, root, &config);
    // Every executed transaction was in a checkpoint, and the last one has
    // committed.
    assert_eq!(
        store.uncommitted_transactions(),
        0,
        "nothing uncommitted outlives its epoch"
    );
    store.commit_epoch_start(
        state.epoch,
        EpochRecord::new(state, last, state_hasher::encode(root)).encode(),
    );
}

/// Checks checkpoint `last`'s certified end-of-epoch data against `next`,
/// the system state its change-epoch transaction wrote here, and `root`,
/// the root after it, as `config` commits it.
fn check_end_of_epoch(
    store: &store::Store,
    last: u64,
    next: &SystemState,
    root: &StateHash,
    config: &ProtocolConfig,
) {
    let bytes = store
        .certified(last)
        .expect("the store reads")
        .expect("a committed checkpoint's certificate is stored");
    let bump = Bump::default();
    let summary =
        CertifiedCheckpointSummary::parse(&mut Reader::new(&bytes), &mut BumpAlloc(&bump))
            .expect("a stored certificate parses")
            .data;
    let end = summary
        .end_of_epoch_data
        .expect("the epoch's last checkpoint has end-of-epoch data");
    let committee: Vec<([u8; 96], u64)> = end
        .next_epoch_committee
        .iter()
        .map(|m| (m.authority.bytes, m.stake.get()))
        .collect();
    assert!(
        committee == next.stakes(),
        "fork: checkpoint {last} certifies another next committee"
    );
    assert_eq!(
        end.next_epoch_protocol_version, next.protocol_version,
        "fork: checkpoint {last} certifies another next protocol version"
    );
    let commitments: Vec<_> = end
        .epoch_commitments
        .iter()
        .map(|c| (c.kind(), c.digest.bytes))
        .collect();
    let expected: Vec<_> = config
        .commit_root_state_digest()
        .then(|| {
            (
                CheckpointCommitmentKind::EcmhLiveObjectSetDigest,
                root.digest().digest,
            )
        })
        .into_iter()
        .collect();
    assert!(
        commitments == expected,
        "fork: checkpoint {last} certifies another root"
    );
}

/// The node: what lives for good, and the epoch running now.
pub struct Node {
    // Dropped first: its workers use the perpetual parts.
    running: Option<Epoch>,
    pub perpetual: Perpetual,
}

impl Node {
    pub fn new(perpetual: Perpetual) -> Node {
        Node {
            perpetual,
            running: None,
        }
    }

    pub fn running(&self) -> Option<&Epoch> {
        self.running.as_ref()
    }

    /// Stops the running epoch, if any, without moving to the next: for
    /// shutting down.
    pub fn stop(&mut self) {
        drop(self.running.take());
    }

    /// The perpetual parts, the running epoch stopped.
    pub fn into_perpetual(mut self) -> Perpetual {
        self.stop();
        self.perpetual
    }

    /// Starts the store's current epoch, its consensus started by
    /// `consensus`, first reconfiguring if that epoch has already ended: a
    /// crash after its last checkpoint committed, before the next epoch's
    /// record. The root is then read from, or folded again in, the ended
    /// epoch's store.
    ///
    /// # Panics
    /// If an epoch is running.
    pub fn start_epoch(&mut self, consensus: &mut impl Consensus) -> &Epoch {
        assert!(self.running.is_none(), "one epoch at a time");
        let perpetual = &self.perpetual;
        if epoch_ended(&perpetual.store) {
            let current = EpochRecord::load(&perpetual.store).epoch;
            let (epoch_store, _, builder) = EpochStore::open(&perpetual.epochs, current);
            let root = committed_root(&perpetual.store, &StateHashes::new(epoch_store, builder));
            reconfigure(&perpetual.store, &root, perpetual.chain);
        }
        self.running
            .insert(Epoch::start(&self.perpetual, consensus))
    }

    /// Stops the running epoch, whose last checkpoint has committed: drains
    /// it, then joins every one of its workers, then moves the node into the
    /// next epoch. Its consensus must have stopped.
    ///
    /// # Panics
    /// If no epoch is running, or it has not ended.
    pub fn finish_epoch(&mut self) {
        let epoch = self.running.take().expect("an epoch is running");
        assert!(
            epoch.pipeline.ended.borrow().is_some(),
            "the epoch has ended"
        );
        epoch.drain();
        let root = committed_root(&self.perpetual.store, &epoch.pipeline.state_hashes);
        drop(epoch);
        reconfigure(&self.perpetual.store, &root, self.perpetual.chain);
        let next = EpochRecord::load(&self.perpetual.store).epoch;
        store::epoch::prune_before(&self.perpetual.epochs, next)
            .expect("old epochs' stores are deleted");
    }

    /// Runs epoch after epoch, for good, starting each one's consensus with
    /// `consensus`, and stopping it before reconfiguring.
    pub async fn run(mut self, mut consensus: impl Consensus) {
        loop {
            let mut ended = self.start_epoch(&mut consensus).pipeline.ended.clone();
            ended
                .wait_for(Option::is_some)
                .await
                .expect("the committer lives as long as the epoch");
            consensus.stop();
            // Joining the epoch's workers blocks.
            self = tokio::task::spawn_blocking(move || {
                self.finish_epoch();
                self
            })
            .await
            .expect("finishing an epoch does not panic");
        }
    }
}
