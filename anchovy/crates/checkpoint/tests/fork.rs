// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The fork check: certified checkpoints waiting for the ones this validator
//! built, then compared with them.

use std::sync::Arc;

use checkpoint::{LocalCheckpoints, check_forks};
use validator::pipeline::items::{CertifiedCheckpoint, ForkCheck};
use validator::pipeline::keys::CheckpointBuilt;
use validator::pipeline::tables::{BuiltCheckpoint, CheckpointStore};
use waiter::{Command, WaitBatch, WaiterProcessor};
use workqueue::Processor;

fn built(seq: u64, summary: &[u8]) -> BuiltCheckpoint {
    BuiltCheckpoint {
        seq,
        summary: summary.to_vec(),
        digest: [seq as u8; 32],
        contents: vec![],
        network_total_transactions: seq,
        epoch_rolling_gas: [0; 4],
        last_height: seq,
        timestamp_ms: 0,
        root_state: None,
    }
}

/// A `CheckpointSummary`'s BCS, of checkpoint `seq` at `timestamp_ms`.
fn summary(seq: u64, timestamp_ms: u64) -> Vec<u8> {
    bcs::to_bytes(&sui_types::messages_checkpoint::CheckpointSummary {
        epoch: 0,
        sequence_number: seq,
        network_total_transactions: seq,
        content_digest: sui_types::digests::CheckpointContentsDigest::new([seq as u8; 32]),
        previous_digest: None,
        epoch_rolling_gas_cost_summary: sui_types::gas::GasCostSummary::default(),
        timestamp_ms,
        checkpoint_commitments: vec![],
        end_of_epoch_data: None,
        version_specific_data: vec![],
    })
    .unwrap()
}

/// `summary` as certified: its bytes, then a quorum signature's (epoch,
/// signature, signer bitmap), whose contents the check never reads.
fn certified(seq: u64, summary: &[u8]) -> ForkCheck {
    let mut bytes = summary.to_vec();
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&[0xee; 48]);
    bytes.push(0);
    ForkCheck {
        certified: CertifiedCheckpoint {
            seq,
            summary: bytes.into(),
        },
        local: None,
    }
}

type ForkWaiter =
    WaiterProcessor<CheckpointBuilt, ForkCheck, LocalCheckpoints, Arc<BuiltCheckpoint>>;

fn waiter() -> (
    ForkWaiter,
    Arc<CheckpointStore>,
    Arc<store::Store>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(store::Store::open(dir.path()).unwrap());
    let checkpoints = Arc::new(CheckpointStore::new());
    let waiter = WaiterProcessor::new(LocalCheckpoints {
        checkpoints: checkpoints.clone(),
        store: store.clone(),
    });
    (waiter, checkpoints, store, dir)
}

fn wait(check: ForkCheck) -> Command<CheckpointBuilt, ForkCheck, Arc<BuiltCheckpoint>> {
    let mut batch = WaitBatch::new();
    let seq = check.certified.seq;
    batch.push(check, [CheckpointBuilt(seq)]);
    Command::Wait(batch)
}

#[test]
fn a_certified_checkpoint_waits_for_the_local_one_then_matches() {
    let (mut waiter, checkpoints, _store, _dir) = waiter();
    assert!(waiter.process(wait(certified(3, &summary(3, 0)))).is_none());
    checkpoints.insert_built(built(3, &summary(3, 0)));
    let ready = waiter
        .process(Command::Resolve(vec![(
            CheckpointBuilt(3),
            checkpoints.built(3).unwrap(),
        )]))
        .unwrap();
    assert_eq!(ready.len(), 1);
    check_forks(ready);
}

#[test]
fn a_local_checkpoint_pruned_is_read_as_committed() {
    let (mut waiter, checkpoints, store, _dir) = waiter();
    store.commit_genesis_checkpoint(built(0, &summary(0, 0)).encode(), vec![]);
    checkpoints.prune_below(1);
    let ready = waiter.process(wait(certified(0, &summary(0, 0)))).unwrap();
    check_forks(ready);
}

#[test]
#[should_panic(expected = "fork: certified checkpoint 3")]
fn a_certified_checkpoint_other_than_the_local_one_is_a_fork() {
    let (mut waiter, checkpoints, _store, _dir) = waiter();
    checkpoints.insert_built(built(3, &summary(3, 0)));
    let ready = waiter.process(wait(certified(3, &summary(3, 1)))).unwrap();
    check_forks(ready);
}
