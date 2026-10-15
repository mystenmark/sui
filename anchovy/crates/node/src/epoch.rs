// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! What lives for good and what lives for an epoch
//! (`docs/reconfiguration_design.md`). Perpetual: the store, the handle to
//! the running epoch, and state sync. Per epoch: everything in `Epoch`,
//! which dropping stops and joins.

use std::sync::Arc;

use aggregator::Committee;
use checkpoint::ProtocolKey;
use consensus::TransactionClient;
use messages::base::Digest;
use protocol_config::Chain;
use validator::current::CurrentEpoch;
use validator::epoch::EpochState;
use validator::pipeline::items::CertifiedCheckpoint;
use validator::pipeline::tables::BuiltCheckpoint;
use workqueue::Queue;

use crate::Pipeline;

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
    /// Each member's protocol key and stake.
    pub committee: Vec<([u8; 96], u64)>,
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

    const FIXED: usize = 6 * 8;
    const MEMBER: usize = 96 + 8;

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(
            Self::FIXED + 16 + self.committee.len() * Self::MEMBER + self.root_state.len(),
        );
        for word in [
            self.epoch,
            self.protocol_version,
            self.reference_gas_price,
            self.epoch_start_timestamp_ms,
            self.epoch_duration_ms,
            self.previous_checkpoint,
            self.committee.len() as u64,
        ] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        for (key, stake) in &self.committee {
            bytes.extend_from_slice(key);
            bytes.extend_from_slice(&stake.to_le_bytes());
        }
        bytes.extend_from_slice(&(self.root_state.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&self.root_state);
        bytes
    }

    /// # Panics
    /// If `bytes` is not what `encode` wrote.
    pub fn decode(bytes: &[u8]) -> EpochRecord {
        let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("a word"));
        let members = usize::try_from(word(Self::FIXED)).expect("a committee fits in memory");
        let committee_end = Self::FIXED + 8 + members * Self::MEMBER;
        let committee = (0..members)
            .map(|i| {
                let at = Self::FIXED + 8 + i * Self::MEMBER;
                (
                    bytes[at..at + 96].try_into().expect("a protocol key"),
                    word(at + 96),
                )
            })
            .collect();
        let root_len = usize::try_from(word(committee_end)).expect("a root fits in memory");
        assert_eq!(
            bytes.len(),
            committee_end + 8 + root_len,
            "an epoch start record"
        );
        EpochRecord {
            epoch: word(0),
            protocol_version: word(8),
            reference_gas_price: word(16),
            epoch_start_timestamp_ms: word(24),
            epoch_duration_ms: word(32),
            previous_checkpoint: word(40),
            committee,
            root_state: bytes[committee_end + 8..].to_vec(),
        }
    }

    /// The current epoch's record.
    ///
    /// # Panics
    /// If the store cannot be read or has no genesis.
    pub fn load(store: &store::Store) -> EpochRecord {
        let bytes = store
            .epoch_start()
            .expect("the store reads")
            .expect("genesis wrote the first epoch's start");
        EpochRecord::decode(&bytes)
    }
}

/// Writes the genesis checkpoint and the first epoch's start record, read
/// from the system state genesis created, unless the store has them. The
/// genesis objects, with digests `objects`, must be in the store.
///
/// # Panics
/// If the store cannot be read or written, or holds no system state.
pub fn genesis(
    store: &store::Store,
    checkpoint: &BuiltCheckpoint,
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
        checkpoint.encode(),
        EpochRecord::new(state, 0, root).encode(),
    );
}

/// What the node keeps across epochs.
pub struct Perpetual {
    pub store: Arc<store::Store>,
    pub current: CurrentEpoch,
    /// To state sync, which owns the other end: certified checkpoints.
    pub state_sync: Queue<CertifiedCheckpoint>,
    pub chain: Chain,
    /// The genesis checkpoint's digest.
    pub chain_identifier: Digest,
    pub protocol_key: Arc<ProtocolKey>,
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
    /// The epoch the store's start record names, over consensus's
    /// `client`, made the current epoch.
    ///
    /// # Panics
    /// If the store cannot be read, or the record's committee is invalid.
    pub fn start(perpetual: &Perpetual, client: &TransactionClient) -> Epoch {
        let record = EpochRecord::load(&perpetual.store);
        let state = Arc::new(EpochState::starting_at(
            perpetual.chain,
            record.protocol_version,
            record.epoch,
            perpetual.chain_identifier,
            record.reference_gas_price,
            u32::try_from(record.committee.len()).expect("a committee of fewer than 2^32"),
            record.epoch_start_timestamp_ms,
            [],
        ));
        let committee = Arc::new(
            Committee::new(record.epoch, record.committee.iter().copied())
                .expect("the epoch's committee is valid"),
        );
        let pipeline = Pipeline::start(
            &perpetual.store,
            &state,
            &perpetual.protocol_key,
            &committee,
            client,
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

impl Drop for Epoch {
    /// No longer current, so requests wait for the next epoch; then the
    /// pipeline drops, stopping every worker.
    fn drop(&mut self) {
        self.current.clear();
    }
}
