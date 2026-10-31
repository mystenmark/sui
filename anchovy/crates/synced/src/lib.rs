// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Following the certified checkpoint chain from peers
//! (`docs/state_sync_design.md`, Catching up).

use std::sync::Arc;

use aggregator::{CertificateError, Committee};
use messages::arena::BumpAlloc;
use messages::base::Digest;
use messages::checkpoint::CertifiedCheckpointSummary;
use messages::reader::Reader;
use tracing::warn;
use validator::pipeline::chunk_lock::ChunkLock;
use validator::pipeline::items::{BuiltWaitItem, BuiltWaits, CertifiedCheckpoint};
use validator::pipeline::keys::CheckpointBuilt;
use validator::pipeline::tables::CheckpointStore;
use validator::state_sync::{SyncRequest, Wanted};
use waiter::{Command, WaitBatch};
use workqueue::{Bump, Processor};

/// How far past the last committed checkpoint summaries are taken: each
/// waits in memory until committed.
pub const WINDOW: u64 = 256;

/// Why a fetched summary is not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rejected {
    Malformed,
    /// Of another epoch than the running one.
    WrongEpoch,
    /// Past the next checkpoint.
    Gap,
    /// Its previous digest is not the last verified checkpoint's.
    Unchained,
    Certificate(CertificateError),
}

/// Verifies certified summaries fetched from peers, in sequence order, each
/// chained from the last verified: of the running epoch (checked before
/// anything else), the next sequence number, the last one's digest as its
/// previous, and a quorum of the epoch's committee
/// (`Committee::verify_certified`). A summary at or before the last verified
/// is skipped; one that does not verify ends its request, and the fetcher
/// tries another peer.
///
/// Each verified checkpoint takes the fast path: it waits for this
/// validator to build it, then is checked for a fork and committed. Not
/// yet: the slow path, executing a checkpoint consensus has not cut.
///
/// Answers each request with what to fetch next: up to `WINDOW` past the
/// last committed checkpoint, and nothing once the epoch's last checkpoint
/// is verified.
///
/// Inward: the fetcher (black box): `SyncRequest`.
///
/// Outward: built waiter, wait for: `BuiltWaitItem::Synced`, keyed by
/// `CheckpointBuilt(seq)`.
///
/// Reads: the checkpoint store's committed watermark.
pub struct SyncedScheduler {
    committee: Arc<Committee>,
    checkpoints: Arc<CheckpointStore>,
    /// Closed when the scheduler is dropped, so the commit stage never
    /// waits on a chunk it held.
    chunk_lock: Arc<ChunkLock>,
    built: BuiltWaits,
    /// The last checkpoint verified, or committed, and its digest.
    last: (u64, Digest),
    /// Whether the last is the epoch's last checkpoint.
    ended: bool,
}

impl SyncedScheduler {
    /// Chained from the last committed checkpoint, whose certificate `store`
    /// holds.
    pub fn new(
        committee: Arc<Committee>,
        checkpoints: Arc<CheckpointStore>,
        store: &store::Store,
        chunk_lock: Arc<ChunkLock>,
        built: BuiltWaits,
    ) -> SyncedScheduler {
        let seq = checkpoints.last_committed();
        let bytes = store
            .certified(seq)
            .expect("the store reads")
            .expect("a committed checkpoint's certificate is stored");
        let bump = Bump::default();
        let summary =
            CertifiedCheckpointSummary::parse(&mut Reader::new(&bytes), &mut BumpAlloc(&bump))
                .expect("a stored certificate parses")
                .data;
        SyncedScheduler {
            last: (seq, summary.digest.into()),
            ended: summary.epoch == committee.epoch() && summary.end_of_epoch_data.is_some(),
            committee,
            checkpoints,
            chunk_lock,
            built,
        }
    }

    /// `certified`'s digest and whether it ends the epoch, if it is the
    /// next checkpoint; none if it is at or before the last.
    fn verify(&self, certified: &[u8], bump: &Bump) -> Result<Option<(Digest, bool)>, Rejected> {
        let mut r = Reader::new(certified);
        let parsed = CertifiedCheckpointSummary::parse(&mut r, &mut BumpAlloc(bump))
            .and_then(|c| r.finish().map(|()| c))
            .map_err(|_| Rejected::Malformed)?;
        let summary = &parsed.data;
        if summary.epoch != self.committee.epoch() {
            return Err(Rejected::WrongEpoch);
        }
        let (last, last_digest) = self.last;
        if summary.sequence_number <= last {
            return Ok(None);
        }
        if summary.sequence_number != last + 1 {
            return Err(Rejected::Gap);
        }
        if summary.previous_digest != Some(&last_digest) {
            return Err(Rejected::Unchained);
        }
        self.committee
            .verify_certified(&parsed, bump)
            .map_err(Rejected::Certificate)?;
        Ok(Some((
            summary.digest.into(),
            summary.end_of_epoch_data.is_some(),
        )))
    }
}

impl Drop for SyncedScheduler {
    fn drop(&mut self) {
        self.chunk_lock.close();
    }
}

impl Processor for SyncedScheduler {
    type Input = SyncRequest;
    type Output = ();

    fn process(&mut self, request: SyncRequest, bump: &mut Bump) -> Option<()> {
        let mut waits = WaitBatch::new();
        for certified in request.summaries {
            if self.ended {
                break;
            }
            let verified = self.verify(&certified, bump);
            bump.reset();
            match verified {
                Ok(Some((digest, ends))) => {
                    let seq = self.last.0 + 1;
                    self.last = (seq, digest);
                    self.ended = ends;
                    let certified = CertifiedCheckpoint {
                        seq,
                        summary: certified.into(),
                    };
                    waits.push(
                        BuiltWaitItem::Synced {
                            certified,
                            local: None,
                        },
                        [CheckpointBuilt(seq)],
                    );
                }
                Ok(None) => {}
                Err(why) => {
                    warn!(
                        ?why,
                        after = self.last.0,
                        "a fetched checkpoint summary is rejected"
                    );
                    break;
                }
            }
        }
        // Inside the pipeline a full queue is a sizing bug; a closed one
        // means the pipeline is shutting down.
        if !waits.is_empty()
            && let Err(workqueue::PushError::Full(_)) = self.built.try_push(Command::Wait(waits))
        {
            panic!("the built waiter refused synced checkpoints");
        }
        let committed = self.checkpoints.last_committed();
        let _ = request.reply.send(Wanted {
            next: (!self.ended).then_some(self.last.0 + 1),
            room: (committed + WINDOW).saturating_sub(self.last.0),
        });
        None
    }
}
