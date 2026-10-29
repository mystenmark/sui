// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Committing certified checkpoints that come from two sources, the
//! aggregator and peers: each once, in sequence order.

use std::sync::Arc;

use checkpoint::CheckpointCommitter;
use sui_types::base_types::ExecutionDigests;
use sui_types::messages_checkpoint::CheckpointContents;
use validator::epoch_store::EpochStore;
use validator::pipeline::backpressure::{Backpressure, Limits};
use validator::pipeline::items::CertifiedCheckpoint;
use validator::pipeline::tables::{BuiltCheckpoint, CheckpointStore, ChunkTable, GenerationTable};
use workqueue::Processor;

struct Setup {
    store: Arc<store::Store>,
    committer: CheckpointCommitter,
    _dir: tempfile::TempDir,
}

impl Setup {
    /// A committer with checkpoints 1 to `count` built, none committed.
    fn new(count: u64) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let (epoch_store, consensus, builder) = EpochStore::open(dir.path(), 0);
        let store = Arc::new(store::Store::open(&dir.path().join("perpetual")).unwrap());
        let checkpoints = Arc::new(CheckpointStore::new(
            store.clone(),
            0,
            epoch_store.clone(),
            builder,
        ));
        let contents = bcs::to_bytes(&CheckpointContents::new_with_digests_only_for_tests(Vec::<
            ExecutionDigests,
        >::new(
        )))
        .unwrap();
        let built = |seq: u64| BuiltCheckpoint {
            seq,
            summary: vec![seq as u8],
            digest: [seq as u8; 32],
            contents: contents.clone(),
            network_total_transactions: 0,
            epoch_rolling_gas: [0; 4],
            last_height: seq,
            timestamp_ms: 0,
            root_state: None,
        };
        store.commit_genesis_checkpoint(built(0).record(vec![0]), vec![]);
        for seq in 1..=count {
            checkpoints.insert_built(built(seq));
        }
        let limits = Limits {
            chunks: 100,
            transactions: 100,
        };
        let committer = CheckpointCommitter::new(
            store.clone(),
            checkpoints,
            Arc::new(ChunkTable::new()),
            Arc::new(GenerationTable::new(epoch_store)),
            consensus.committed_checkpoints(),
            Arc::new(Backpressure::new(limits, 0)),
            tokio::sync::watch::channel(None).0,
        );
        Setup {
            store,
            committer,
            _dir: dir,
        }
    }

    fn certify(&mut self, seqs: &[u64]) {
        let certified = seqs
            .iter()
            .map(|&seq| CertifiedCheckpoint {
                seq,
                summary: vec![seq as u8, 0xee].into(),
            })
            .collect();
        self.committer
            .process(certified, &mut workqueue::Bump::default());
    }

    fn committed(&self) -> Option<u64> {
        self.store.committed_checkpoint().unwrap()
    }
}

#[test]
fn checkpoints_commit_once_in_order_from_either_source() {
    let mut s = Setup::new(3);
    // Peers' certificate for 2 arrives before anyone's for 1.
    s.certify(&[2]);
    assert_eq!(s.committed(), Some(0));

    s.certify(&[1, 2]);
    assert_eq!(s.committed(), Some(2));
    assert_eq!(s.store.certified(2).unwrap().unwrap(), vec![2, 0xee]);

    // The other source's certificates for checkpoints committed already.
    s.certify(&[1]);
    s.certify(&[2, 3]);
    assert_eq!(s.committed(), Some(3));
}
