// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! What consensus hands the validator: verified blocks to vote on, and
//! committed sub-dags to execute; and how the validator submits
//! transactions. A minimal mirror of consensus-core's `VerifiedBlock`,
//! `CommittedSubDag` and `TransactionClient`, without depending on it; the
//! structural checks on a block (its signature, ancestors and limits) are
//! consensus-core's, so a `Block` here has passed them.

use tokio::sync::{mpsc, oneshot};

/// A transaction's position in its block.
pub type TransactionIndex = u16;

/// A block's round, author and digest: consensus-core's `BlockRef`.
/// Ordered by round, then author, then digest, as consensus-core orders it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockRef {
    pub round: u32,
    /// The author's index in the committee.
    pub author: u32,
    pub digest: [u8; 32],
}

/// A block that passed consensus-core's verification: its reference and its
/// transactions, each the BCS of a sui `ConsensusTransaction`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub reference: BlockRef,
    pub transactions: Vec<Vec<u8>>,
}

/// A committed sub-dag: its blocks in commit order, and the transactions a
/// quorum voted to reject.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedSubDag {
    /// Consecutive from 1.
    pub commit_index: u32,
    /// The commit's digest, which the commit prologue records.
    pub digest: [u8; 32],
    pub leader: BlockRef,
    pub timestamp_ms: u64,
    pub blocks: Vec<Block>,
    /// The rejected transactions of each block that has any, by block.
    pub rejected: Vec<(BlockRef, Vec<TransactionIndex>)>,
}

impl CommittedSubDag {
    /// The rejected transactions of `block`, in no particular order.
    pub fn rejected_in(&self, block: &BlockRef) -> &[TransactionIndex] {
        self.rejected
            .iter()
            .find(|(b, _)| b == block)
            .map_or(&[], |(_, rejected)| rejected)
    }
}

/// Which lane a submission takes: consensus-core's `Priority`. The local
/// validator's own high-priority submissions (checkpoint signatures) are
/// pulled ahead of normal ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    Normal,
    High,
}

/// What became of the block a submission was included in: consensus-core's
/// `BlockStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockStatus {
    /// Sequenced in a committed sub-dag, with every transaction in it.
    Sequenced(BlockRef),
    /// Garbage-collected: it will never be committed, and its transactions
    /// may need to be submitted again.
    GarbageCollected(BlockRef),
}

/// Where a submission was included, and the receiver for what became of the
/// block.
pub type Included = (
    BlockRef,
    Vec<TransactionIndex>,
    oneshot::Receiver<BlockStatus>,
);

/// consensus-core's `ClientError`, as far as anchovy meets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    ConsensusShuttingDown,
}

/// Transactions submitted, and where to acknowledge their inclusion in a
/// proposed block: consensus-core's `TransactionsGuard`. What consensus
/// receives.
#[derive(Debug)]
pub struct TransactionsGuard {
    pub transactions: Vec<Vec<u8>>,
    pub included_in_block_ack: oneshot::Sender<Included>,
}

/// Submits transactions to consensus: consensus-core's `TransactionClient`.
#[derive(Clone, Debug)]
pub struct TransactionClient {
    sender: mpsc::Sender<TransactionsGuard>,
    priority_sender: mpsc::Sender<TransactionsGuard>,
}

impl TransactionClient {
    /// The client, and the receivers consensus drains for normal and
    /// priority submissions, in that order.
    pub fn new(
        capacity: usize,
    ) -> (
        TransactionClient,
        mpsc::Receiver<TransactionsGuard>,
        mpsc::Receiver<TransactionsGuard>,
    ) {
        let (sender, receiver) = mpsc::channel(capacity);
        let (priority_sender, priority_receiver) = mpsc::channel(capacity);
        (
            TransactionClient {
                sender,
                priority_sender,
            },
            receiver,
            priority_receiver,
        )
    }

    /// Submits `transactions` to be sequenced, returning once they are
    /// included in a block consensus will propose: where, and the receiver
    /// for what becomes of that block.
    pub async fn submit(
        &self,
        transactions: Vec<Vec<u8>>,
        priority: Priority,
    ) -> Result<Included, ClientError> {
        let (ack, included) = oneshot::channel();
        self.lane(priority)
            .send(TransactionsGuard {
                transactions,
                included_in_block_ack: ack,
            })
            .await
            .map_err(|_| ClientError::ConsensusShuttingDown)?;
        included
            .await
            .map_err(|_| ClientError::ConsensusShuttingDown)
    }

    /// `submit` for a caller on a plain thread that need not learn where the
    /// transactions were included: blocks while the lane is full, not until
    /// inclusion. Must not be called from within an async runtime.
    pub fn submit_blocking(
        &self,
        transactions: Vec<Vec<u8>>,
        priority: Priority,
    ) -> Result<oneshot::Receiver<Included>, ClientError> {
        let (ack, included) = oneshot::channel();
        self.lane(priority)
            .blocking_send(TransactionsGuard {
                transactions,
                included_in_block_ack: ack,
            })
            .map_err(|_| ClientError::ConsensusShuttingDown)?;
        Ok(included)
    }

    fn lane(&self, priority: Priority) -> &mpsc::Sender<TransactionsGuard> {
        match priority {
            Priority::Normal => &self.sender,
            Priority::High => &self.priority_sender,
        }
    }
}
