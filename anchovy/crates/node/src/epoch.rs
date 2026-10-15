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
}

impl EpochRecord {
    const FIXED: usize = 6 * 8;
    const MEMBER: usize = 96 + 8;

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(Self::FIXED + 8 + self.committee.len() * Self::MEMBER);
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
        bytes
    }

    /// # Panics
    /// If `bytes` is not what `encode` wrote.
    pub fn decode(bytes: &[u8]) -> EpochRecord {
        let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("a word"));
        let members = usize::try_from(word(Self::FIXED)).expect("a committee fits in memory");
        assert_eq!(
            bytes.len(),
            Self::FIXED + 8 + members * Self::MEMBER,
            "an epoch start record"
        );
        let committee = (0..members)
            .map(|i| {
                let at = Self::FIXED + 8 + i * Self::MEMBER;
                (
                    bytes[at..at + 96].try_into().expect("a protocol key"),
                    word(at + 96),
                )
            })
            .collect();
        EpochRecord {
            epoch: word(0),
            protocol_version: word(8),
            reference_gas_price: word(16),
            epoch_start_timestamp_ms: word(24),
            epoch_duration_ms: word(32),
            previous_checkpoint: word(40),
            committee,
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

/// Writes the genesis checkpoint and the first epoch's start record, unless
/// the store has them.
///
/// # Panics
/// If the store cannot be read or written.
pub fn genesis(store: &store::Store, checkpoint: &BuiltCheckpoint, first: &EpochRecord) {
    if store.epoch_start().expect("the store reads").is_some() {
        return;
    }
    assert_eq!(checkpoint.seq, 0, "the genesis checkpoint is checkpoint 0");
    assert_eq!(
        first.previous_checkpoint, 0,
        "the first epoch follows genesis"
    );
    store.commit_genesis_checkpoint(checkpoint.encode(), first.encode());
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
        let state = Arc::new(EpochState::new(
            perpetual.chain,
            record.protocol_version,
            record.epoch,
            perpetual.chain_identifier,
            record.reference_gas_price,
            u32::try_from(record.committee.len()).expect("a committee of fewer than 2^32"),
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
            record.previous_checkpoint,
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
