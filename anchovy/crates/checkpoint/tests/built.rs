// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The built waiter: what waits for this validator to build a checkpoint,
//! routed by kind once it has, and the fork check of certified checkpoints.

use std::sync::Arc;

use checkpoint::{BuiltAvailability, BuiltRouter};
use validator::epoch_store::EpochStore;
use validator::pipeline::items::{
    BuiltWaitItem, CertifiedCheckpoint, CheckpointContentsBuilt, CheckpointSignature,
    EpochCloserInput, PendingCheckpoint,
};
use validator::pipeline::keys::CheckpointBuilt;
use validator::pipeline::tables::{BuiltCheckpoint, CheckpointStore};
use waiter::{Command, WaitBatch, WaiterProcessor};
use workqueue::{Inbox, Processor, Sink};

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
fn certified(seq: u64, summary: &[u8]) -> BuiltWaitItem {
    let mut bytes = summary.to_vec();
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&[0xee; 48]);
    bytes.push(0);
    BuiltWaitItem::Certified {
        certified: CertifiedCheckpoint {
            seq,
            summary: bytes.into(),
        },
        local: None,
    }
}

fn signature(seq: u64) -> BuiltWaitItem {
    BuiltWaitItem::Signature(CheckpointSignature {
        seq,
        message: vec![seq as u8].into(),
    })
}

fn next(seq: u64) -> BuiltWaitItem {
    BuiltWaitItem::Next(CheckpointContentsBuilt {
        pending: PendingCheckpoint {
            seq,
            heights: vec![seq],
            timestamp_ms: 0,
            close: None,
        },
        contents: Arc::from([]),
        contents_digest: [0; 32],
        gas: [0; 4],
        artifacts_digest: None,
        transaction_count: 0,
        randomness_rounds: vec![],
        last: None,
    })
}

type BuiltWaiter =
    WaiterProcessor<CheckpointBuilt, BuiltWaitItem, BuiltAvailability, Arc<BuiltCheckpoint>>;

/// The built waiter and its router, over an empty checkpoint store.
struct Setup {
    waiter: BuiltWaiter,
    router: BuiltRouter,
    closer: Inbox<EpochCloserInput>,
    aggregator: Inbox<Vec<CheckpointSignature>>,
    checkpoints: Arc<CheckpointStore>,
    _dir: tempfile::TempDir,
}

impl Setup {
    fn new() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let (epoch_store, _, builder) = EpochStore::open(dir.path(), 0);
        let objects = Arc::new(store::Store::open(&dir.path().join("perpetual")).unwrap());
        let checkpoints = Arc::new(CheckpointStore::new(objects, 0, epoch_store, builder));
        let (epoch_closer, closer) = workqueue::queue(16);
        let (aggregator_queue, aggregator) = workqueue::queue(16);
        Setup {
            waiter: WaiterProcessor::new(BuiltAvailability {
                checkpoints: checkpoints.clone(),
            }),
            router: BuiltRouter {
                epoch_closer,
                aggregator: aggregator_queue,
            },
            closer,
            aggregator,
            checkpoints,
            _dir: dir,
        }
    }

    /// Waits for `item` on checkpoint `seq`; routes whatever is ready.
    fn wait(&mut self, item: BuiltWaitItem, seq: u64) {
        let mut batch = WaitBatch::new();
        batch.push(item, [CheckpointBuilt(seq)]);
        self.process(Command::Wait(batch));
    }

    /// Builds checkpoint `seq`, as the summary builder does; routes whatever
    /// is ready.
    fn build(&mut self, checkpoint: BuiltCheckpoint) {
        let seq = checkpoint.seq;
        let checkpoint = self.checkpoints.insert_built(checkpoint);
        self.process(Command::Resolve(vec![(CheckpointBuilt(seq), checkpoint)]));
    }

    fn process(&mut self, command: Command<CheckpointBuilt, BuiltWaitItem, Arc<BuiltCheckpoint>>) {
        if let Some(ready) = self.waiter.process(command) {
            self.router.send(ready);
        }
    }

    /// The sequence numbers of the contents and signatures routed so far.
    fn routed(&self) -> (Vec<u64>, Vec<u64>) {
        let mut next = Vec::new();
        while let Some(input) = self.closer.try_pop() {
            let EpochCloserInput::Built(built) = input else {
                panic!("only built checkpoints are routed to the epoch closer");
            };
            next.extend(built.iter().map(|b| b.pending.seq));
        }
        let signatures = std::iter::from_fn(|| self.aggregator.try_pop())
            .flatten()
            .map(|s| s.seq)
            .collect();
        (next, signatures)
    }
}

#[test]
fn items_wait_for_their_checkpoint_then_go_where_their_kind_goes() {
    let mut s = Setup::new();
    s.wait(next(4), 3);
    s.wait(signature(3), 3);
    s.wait(signature(4), 4);
    s.wait(certified(3, &summary(3, 0)), 3);
    assert_eq!(s.routed(), (vec![], vec![]));

    s.build(built(3, &summary(3, 0)));
    assert_eq!(s.routed(), (vec![4], vec![3]));

    // Once built, an item is ready at once.
    s.wait(signature(3), 3);
    assert_eq!(s.routed(), (vec![], vec![3]));
}

#[test]
#[should_panic(expected = "fork: certified checkpoint 3")]
fn a_certified_checkpoint_other_than_the_local_one_is_a_fork() {
    let mut s = Setup::new();
    s.wait(certified(3, &summary(3, 1)), 3);
    s.build(built(3, &summary(3, 0)));
}
