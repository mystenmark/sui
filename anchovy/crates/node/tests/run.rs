// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The node's loop, `Node::run`, left to itself: it starts each epoch's
//! consensus, runs the epoch until its last checkpoint commits, and moves
//! on, while the test plays consensus for whichever epoch it started last.

mod genesis;

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use consensus::{Block, BlockRef, CommittedSubDag, TransactionClient, TransactionsGuard};
use messages::base::Digest;
use node::ConsensusOutput;
use node::epoch::{EpochRecord, Node, Perpetual};
use protocol_config::Chain;
use sui_types::crypto::{AccountKeyPair, get_key_pair};
use tokio::sync::mpsc;
use validator::consensus::decode::CommitRequest;
use validator::current::CurrentEpoch;
use validator::epoch::EpochParams;

const TIMEOUT: Duration = Duration::from_mins(1);
const COMMIT_INTERVAL_MS: u64 = 10_000;

/// One epoch's consensus, as `run` started it.
struct Started {
    params: Arc<EpochParams>,
    output: ConsensusOutput,
    lanes: (
        mpsc::Receiver<TransactionsGuard>,
        mpsc::Receiver<TransactionsGuard>,
    ),
}

impl Started {
    /// Commits `index` at `timestamp_ms`: a block of whatever the node
    /// submitted. Commits after the epoch's end are dropped, as by
    /// consensus that has stopped.
    fn commit(&mut self, index: u32, timestamp_ms: u64) {
        let reference = BlockRef {
            round: index,
            author: 0,
            digest: [index as u8; 32],
        };
        let mut transactions = Vec::new();
        for lane in [&mut self.lanes.1, &mut self.lanes.0] {
            while let Ok(guard) = lane.try_recv() {
                transactions.extend(guard.transactions);
            }
        }
        let _ = self.output.commits.try_push(CommitRequest {
            commit: CommittedSubDag {
                commit_index: index,
                digest: [index as u8; 32],
                leader: reference,
                timestamp_ms,
                blocks: vec![Block {
                    reference,
                    transactions,
                }],
                rejected: vec![],
            },
        });
    }
}

#[test]
fn the_node_runs_epoch_after_epoch() {
    let (sender, _): (_, AccountKeyPair) = get_key_pair();
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(store::Store::open(dir.path()).unwrap());
    let genesis = genesis::build(sender, 1, 1_000_000_000, 25_000);
    execution::genesis::commit(&store, &genesis.objects).unwrap();
    node::epoch::genesis(
        &store,
        &genesis.checkpoint,
        genesis.objects.iter().map(|o| o.digest().into_inner()),
    );
    let (state_sync, _certified) = workqueue::queue(1024);
    let perpetual = Perpetual {
        store: store.clone(),
        current: CurrentEpoch::new(),
        state_sync,
        chain: Chain::Unknown,
        chain_identifier: Digest::new(genesis.chain_identifier),
        protocol_key: Arc::new(checkpoint::ProtocolKey::new(genesis::protocol_key_pair())),
    };

    let (started, epochs) = std::sync::mpsc::channel();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.spawn(Node::new(perpetual).run(move |epoch, output| {
        let (client, normal, priority) = TransactionClient::new(64);
        started
            .send(Started {
                params: epoch.params.clone(),
                output,
                lanes: (normal, priority),
            })
            .unwrap();
        client
    }));

    let deadline = Instant::now() + TIMEOUT;
    let next = |epochs: &Receiver<Started>| match epochs.recv_timeout(Duration::from_millis(5)) {
        Ok(started) => Some(started),
        Err(RecvTimeoutError::Timeout) => None,
        Err(RecvTimeoutError::Disconnected) => panic!("the node stopped"),
    };
    let mut current = epochs.recv_timeout(TIMEOUT).unwrap();
    let mut clock_ms = 0;
    for epoch in 0..3 {
        assert_eq!(current.params.epoch, epoch);
        assert_eq!(EpochRecord::load(&store).epoch, epoch);
        let mut index = 0;
        current = loop {
            if let Some(started) = next(&epochs) {
                break started;
            }
            assert!(Instant::now() < deadline, "timed out in epoch {epoch}");
            index += 1;
            clock_ms += COMMIT_INTERVAL_MS;
            current.commit(index, clock_ms);
        };
    }
    assert_eq!(current.params.epoch, 3);
    let state = execution::system_state::read(&store).unwrap();
    assert_eq!(state.epoch, 3);
}
