// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;
use std::time::Duration;

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

/// How long a garbage-collected system message waits to be submitted again:
/// the reference's `RETRY_DELAY_STEP`.
const RESUBMIT_DELAY: Duration = Duration::from_secs(1);

async fn submit(
    client: Arc<consensus_core::TransactionClient>,
    guard: TransactionsGuard,
    priority: Priority,
) {
    let priority = match priority {
        Priority::Normal => consensus_core::Priority::Normal,
        Priority::High => consensus_core::Priority::High,
    };
    let TransactionsGuard {
        transactions,
        included_in_block_ack,
        until_sequenced,
    } = guard;
    let mut ack = Some(included_in_block_ack);
    loop {
        let Ok((block, indices, status)) = client.submit(transactions.clone(), priority).await
        else {
            return;
        };
        // The first inclusion is acknowledged; later ones are retries.
        let mut answer = None;
        if let Some(ack) = ack.take() {
            let (sender, receiver) = oneshot::channel();
            if ack.send((block_ref(&block), indices, receiver)).is_err() && !until_sequenced {
                return;
            }
            answer = Some(sender);
        }
        let Ok(status) = status.await else {
            return;
        };
        let status = match status {
            consensus_core::BlockStatus::Sequenced(block) => {
                BlockStatus::Sequenced(block_ref(&block))
            }
            consensus_core::BlockStatus::GarbageCollected(block) => {
                BlockStatus::GarbageCollected(block_ref(&block))
            }
        };
        if let Some(answer) = answer {
            let _ = answer.send(status);
        }
        if !until_sequenced || matches!(status, BlockStatus::Sequenced(_)) {
            return;
        }
        tracing::debug!(
            ?status,
            "a system message's block was garbage collected; resubmitting"
        );
        tokio::time::sleep(RESUBMIT_DELAY).await;
    }
}
