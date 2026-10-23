// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! What became of each transaction consensus committed, by its position:
//! the reference's `ConsensusTxStatusCache`. Commit handling writes it; a
//! submitter waiting for its transaction reads it, racing the effects.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard, PoisonError};

use ::consensus::{BlockRef, TransactionIndex};
use tokio::sync::{oneshot, watch};

/// How many rounds a status is kept past its block's: the reference's
/// `CONSENSUS_STATUS_RETENTION_ROUNDS`. A position this far behind the
/// committed leader round has expired: if its block was never committed, it
/// never will be.
pub const RETENTION_ROUNDS: u32 = 400;

/// Where a transaction was included: the reference's `ConsensusPosition`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConsensusPosition {
    pub epoch: u64,
    pub block: BlockRef,
    pub index: TransactionIndex,
}

impl ConsensusPosition {
    /// As the reference's BCS: the epoch, the block's round, author and
    /// digest, then the index.
    pub fn to_bcs(&self) -> Vec<u8> {
        bcs::to_bytes(&(
            self.epoch,
            self.block.round,
            self.block.author,
            self.block.digest,
            self.index,
        ))
        .expect("a position encodes")
    }

    pub fn from_bcs(bytes: &[u8]) -> Option<ConsensusPosition> {
        let (epoch, round, author, digest, index) = bcs::from_bytes(bytes).ok()?;
        Some(ConsensusPosition {
            epoch,
            block: BlockRef {
                round,
                author,
                digest,
            },
            index,
        })
    }
}

/// What consensus decided about a committed transaction: the reference's
/// `ConsensusTxStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsensusTxStatus {
    /// A quorum voted to reject it.
    Rejected,
    /// Committed and to be executed (or executed before, if a duplicate).
    Finalized,
    /// Dropped post-consensus, the same way by every validator: an owned
    /// input conflicting with a lock, or invalid inputs.
    Dropped,
}

/// What a wait for a position's status ends with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusResult {
    Status(ConsensusTxStatus),
    /// Its round fell `RETENTION_ROUNDS` behind this committed leader round
    /// without a status.
    Expired(u32),
}

/// See the module documentation.
pub struct ConsensusTxStatusCache {
    inner: Mutex<Inner>,
    /// The committed leader round positions expire against.
    leader_round: watch::Sender<Option<u32>>,
}

#[derive(Default)]
struct Inner {
    /// Ordered by block, so by round: expiry pops from the front.
    statuses: BTreeMap<(BlockRef, TransactionIndex), ConsensusTxStatus>,
    waiters: HashMap<(BlockRef, TransactionIndex), Vec<oneshot::Sender<ConsensusTxStatus>>>,
    /// The last leader round passed to `update_leader_round`.
    last_leader_round: Option<u32>,
}

impl Default for ConsensusTxStatusCache {
    fn default() -> Self {
        ConsensusTxStatusCache {
            inner: Mutex::default(),
            leader_round: watch::Sender::new(None),
        }
    }
}

fn expired(round: u32, leader_round: u32) -> bool {
    round.saturating_add(RETENTION_ROUNDS) <= leader_round
}

impl ConsensusTxStatusCache {
    pub fn new() -> ConsensusTxStatusCache {
        ConsensusTxStatusCache::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // Each critical section leaves the maps consistent.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records each position's status, waking whoever waits on it. A
    /// position expired already is ignored.
    ///
    /// # Panics
    /// If a position already has a different status: every handler decides
    /// alike.
    pub fn set_statuses(
        &self,
        updates: impl IntoIterator<Item = (BlockRef, TransactionIndex, ConsensusTxStatus)>,
        bump: &containers::Bump,
    ) {
        let leader_round = *self.leader_round.borrow();
        let mut woken = containers::Vec::new_in(bump);
        {
            let mut inner = self.lock();
            for (block, index, status) in updates {
                if leader_round.is_some_and(|r| expired(block.round, r)) {
                    continue;
                }
                if let Some(old) = inner.statuses.insert((block, index), status) {
                    assert_eq!(old, status, "conflicting statuses for {block:?} {index}");
                }
                if let Some(waiters) = inner.waiters.remove(&(block, index)) {
                    woken.extend(waiters.into_iter().map(|w| (w, status)));
                }
            }
        }
        for (waiter, status) in woken {
            let _ = waiter.send(status);
        }
    }

    /// A commit with this leader round was handled. As the reference does,
    /// positions expire against the round before, so that the transactions
    /// of the commit just handled are not expired at once.
    pub fn update_leader_round(&self, leader_round: u32) {
        let mut inner = self.lock();
        let Some(previous) = inner.last_leader_round.replace(leader_round) else {
            return;
        };
        while let Some((&(block, _), _)) = inner.statuses.first_key_value() {
            if !expired(block.round, previous) {
                break;
            }
            inner.statuses.pop_first();
        }
        inner.waiters.retain(|(block, _), waiters| {
            waiters.retain(|w| !w.is_closed());
            !waiters.is_empty() && !expired(block.round, previous)
        });
        drop(inner);
        self.leader_round.send_replace(Some(previous));
    }

    /// The status of the transaction at `position`, once known, or its
    /// expiry.
    pub async fn notify_read(&self, position: ConsensusPosition) -> StatusResult {
        let key = (position.block, position.index);
        let registered = {
            let mut inner = self.lock();
            if let Some(status) = inner.statuses.get(&key) {
                return StatusResult::Status(*status);
            }
            let (tx, rx) = oneshot::channel();
            inner.waiters.entry(key).or_default().push(tx);
            rx
        };
        let mut leader_round = self.leader_round.subscribe();
        let expiry = async {
            loop {
                if let Some(round) = *leader_round.borrow_and_update()
                    && expired(position.block.round, round)
                {
                    return round;
                }
                if leader_round.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        };
        tokio::select! {
            status = registered => match status {
                Ok(status) => StatusResult::Status(status),
                // Dropped only by `update_leader_round`, on expiry.
                Err(_) => StatusResult::Expired(
                    self.leader_round.borrow().unwrap_or(position.block.round),
                ),
            },
            round = expiry => StatusResult::Expired(round),
        }
    }

    /// Whether `position` is too far ahead of the committed leader round to
    /// wait for: the reference's `check_position_too_ahead`.
    pub fn too_far_ahead(&self, position: &ConsensusPosition) -> bool {
        self.leader_round
            .borrow()
            .is_some_and(|r| position.block.round > r.saturating_add(RETENTION_ROUNDS))
    }
}
