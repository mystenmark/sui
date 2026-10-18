// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use consensus::{BlockStatus, Priority, TransactionsGuard};
use tokio::sync::{mpsc, oneshot};

use crate::commits::block_ref;

/// Submits what the pipeline's client lanes carry to consensus-core's
/// client, the high lane first, and answers each submission in the
/// pipeline's types. Each submission waits for its block on its own task;
/// one that fails drops its answer, and the submitter sees consensus gone.
/// Ends when both lanes close.
pub(crate) async fn forward(
    mut normal: mpsc::Receiver<TransactionsGuard>,
    mut high: mpsc::Receiver<TransactionsGuard>,
    client: Arc<consensus_core::TransactionClient>,
) {
    loop {
        let (guard, priority) = tokio::select! {
            biased;
            Some(guard) = high.recv() => (guard, Priority::High),
            Some(guard) = normal.recv() => (guard, Priority::Normal),
            else => return,
        };
        tokio::spawn(submit(client.clone(), guard, priority));
    }
}

async fn submit(
    client: Arc<consensus_core::TransactionClient>,
    guard: TransactionsGuard,
    priority: Priority,
) {
    let priority = match priority {
        Priority::Normal => consensus_core::Priority::Normal,
        Priority::High => consensus_core::Priority::High,
    };
    let Ok((block, indices, status)) = client.submit(guard.transactions, priority).await else {
        return;
    };
    let (sender, receiver) = oneshot::channel();
    if guard
        .included_in_block_ack
        .send((block_ref(&block), indices, receiver))
        .is_err()
    {
        return;
    }
    if let Ok(status) = status.await {
        let _ = sender.send(match status {
            consensus_core::BlockStatus::Sequenced(block) => {
                BlockStatus::Sequenced(block_ref(&block))
            }
            consensus_core::BlockStatus::GarbageCollected(block) => {
                BlockStatus::GarbageCollected(block_ref(&block))
            }
        });
    }
}
