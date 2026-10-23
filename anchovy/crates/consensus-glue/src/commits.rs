// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use consensus::{Block, BlockRef, CommittedSubDag};
use consensus_core::{BlockAPI, CommitConsumerMonitor};
use mysten_metrics::monitored_mpsc::UnboundedReceiver;
use tokio::sync::oneshot;
use validator::consensus::decode::CommitRequest;
use workqueue::{PushError, Queue};

/// Forwards consensus-core's commits to the pipeline, one in flight: each is
/// pushed with a oneshot, which the commit stage answers once it has handled
/// it; then the monitor learns of it, and the next is taken. Ends when
/// consensus-core or the pipeline does.
pub(crate) async fn forward(
    mut committed: UnboundedReceiver<consensus_core::CommittedSubDag>,
    commits: Queue<CommitRequest>,
    monitor: Arc<CommitConsumerMonitor>,
) {
    while let Some(sub_dag) = committed.recv().await {
        let index = sub_dag.commit_ref.index;
        let (handled, answer) = oneshot::channel();
        let request = CommitRequest {
            commit: commit(sub_dag),
            handled: Some(handled),
        };
        match commits.try_push(request) {
            Ok(()) => {}
            Err(PushError::Full(_)) => panic!("one commit in flight fits the commit queue"),
            Err(PushError::Closed(_)) => return,
        }
        if answer.await.is_err() {
            return;
        }
        monitor.set_highest_handled_commit(index);
    }
}

fn commit(sub_dag: consensus_core::CommittedSubDag) -> CommittedSubDag {
    CommittedSubDag {
        commit_index: sub_dag.commit_ref.index,
        digest: sub_dag.commit_ref.digest.into_inner(),
        leader: block_ref(&sub_dag.leader),
        timestamp_ms: sub_dag.timestamp_ms,
        blocks: sub_dag
            .blocks
            .iter()
            .map(|block| Block {
                reference: block_ref(&block.reference()),
                transactions: block
                    .transactions()
                    .iter()
                    .map(|t| t.data().to_vec())
                    .collect(),
            })
            .collect(),
        rejected: sub_dag
            .rejected_transactions_by_block
            .into_iter()
            .map(|(block, rejected)| (block_ref(&block), rejected))
            .collect(),
    }
}

pub(crate) fn block_ref(block: &consensus_types::block::BlockRef) -> BlockRef {
    BlockRef {
        round: block.round,
        author: block.author.value() as u32,
        digest: block.digest.0,
    }
}
