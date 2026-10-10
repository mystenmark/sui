// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! What consensus hands the validator: verified blocks to vote on, and
//! committed sub-dags to execute. A minimal mirror of consensus-core's
//! `VerifiedBlock` and `CommittedSubDag`, without depending on it; the
//! structural checks on a block (its signature, ancestors and limits) are
//! consensus-core's, so a `Block` here has passed them.

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
