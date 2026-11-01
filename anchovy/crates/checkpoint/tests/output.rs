// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Consensus output waits until its checkpoints are built here and
//! committed.

use std::sync::Arc;

use checkpoint::OutputAvailability;
use sui_types::base_types::ExecutionDigests;
use sui_types::messages_checkpoint::CheckpointContents;
use validator::epoch_store::EpochStore;
use validator::pipeline::items::OutputThrough;
use validator::pipeline::keys::CheckpointProgress;
use validator::pipeline::tables::{BuiltCheckpoint, CheckpointStore};
use waiter::{Command, WaitBatch, WaiterProcessor};
use workqueue::Processor;

fn built(seq: u64) -> BuiltCheckpoint {
    BuiltCheckpoint {
        seq,
        summary: vec![seq as u8],
        digest: [seq as u8; 32],
        contents: bcs::to_bytes(&CheckpointContents::new_with_digests_only_for_tests(Vec::<
            ExecutionDigests,
        >::new(
        )))
        .unwrap(),
        network_total_transactions: 0,
        epoch_rolling_gas: [0; 4],
        last_height: seq,
        timestamp_ms: 0,
        ends_epoch: false,
    }
}

#[test]
fn output_waits_for_a_certified_checkpoint_to_be_built_here() {
    let dir = tempfile::tempdir().unwrap();
    let (epoch_store, _, builder) = EpochStore::open(dir.path(), 0);
    let store = Arc::new(store::Store::open(&dir.path().join("perpetual")).unwrap());
    store.commit_genesis_checkpoint(built(0).record(vec![0]), vec![]);
    let checkpoints = Arc::new(CheckpointStore::new(store, 0, epoch_store, builder));
    let mut waiter = WaiterProcessor::new(OutputAvailability {
        checkpoints: checkpoints.clone(),
    });
    let mut run = |command| waiter.process(command, &mut workqueue::Bump::default());

    let mut wait = WaitBatch::new();
    wait.push(
        OutputThrough(7),
        [
            CheckpointProgress::BuiltHere(1),
            CheckpointProgress::Committed(1),
        ],
    );
    assert_eq!(run(Command::Wait(wait)), None);

    // Committed from its certificate, ahead of consensus.
    checkpoints.insert_certified(built(1));
    checkpoints.committed(1);
    assert_eq!(
        run(Command::notify([CheckpointProgress::Committed(1)])),
        None
    );

    checkpoints.insert_built(built(1));
    assert_eq!(
        run(Command::notify([CheckpointProgress::BuiltHere(1)])),
        Some(vec![OutputThrough(7)])
    );
}
