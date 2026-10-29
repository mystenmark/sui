// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Certified checkpoints served to peers as sui's state sync server answers
//! them (`/sui.StateSync/*`), from the store's committed checkpoints
//! (`docs/state_sync_reference.md` §1). The epoch's checkpoint server reads
//! the store and encodes each answer; the p2p handlers only queue the query
//! and await the bytes.

use std::io::Write as _;
use std::sync::Arc;

use crate::pipeline::tables::BuiltCheckpoint;
use bytes::Bytes;
use messages::Message;
use messages::arena::BumpAlloc;
use messages::base::{Digest, Hashed};
use messages::checkpoint::{CheckpointContents, VersionedCheckpointContents};
use messages::effects::TransactionEffects;
use messages::fast::Writer;
use messages::reader::Reader;
use messages::transaction::SenderSignedData;
use tokio::sync::oneshot;
use workqueue::{Bump, Processor};

/// The lowest checkpoint whose contents are served: genesis's transaction is
/// not kept, and every peer has genesis.
const LOWEST_AVAILABLE: u64 = 1;

/// What a peer asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Query {
    /// `GetCheckpointSummary`: the highest committed checkpoint.
    Latest,
    /// `GetCheckpointSummary`, by summary digest.
    ByDigest(Digest),
    /// `GetCheckpointSummary`, by sequence number.
    BySequenceNumber(u64),
    /// `GetCheckpointContents` (`FullCheckpointContents`), or with
    /// `versioned` `GetCheckpointContentsV2`
    /// (`VersionedFullCheckpointContents`), by contents digest.
    Contents { digest: Digest, versioned: bool },
    /// `GetCheckpointAvailability`.
    Availability,
}

pub struct Request {
    pub query: Query,
    /// The answer as sui's p2p codec sends it, snappy-framed BCS; dropped
    /// if the store cannot answer.
    pub reply: oneshot::Sender<Bytes>,
}

impl workqueue::Refuse for Request {
    fn refuse(self, _: workqueue::Refusal) {}
}

/// Certified summaries fetched from peers (`CertifiedCheckpointSummary`
/// BCS), in sequence order, for the running epoch's `SyncedScheduler`.
pub struct SyncRequest {
    pub summaries: Vec<Vec<u8>>,
    /// What to fetch next.
    pub reply: oneshot::Sender<Wanted>,
}

impl workqueue::Refuse for SyncRequest {
    fn refuse(self, _: workqueue::Refusal) {}
}

/// What the synced scheduler would take next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wanted {
    /// The next checkpoint to fetch; none once the epoch's last is verified.
    pub next: Option<u64>,
    /// How many, from `next`, may be fetched now.
    pub room: u64,
}

pub struct CheckpointServer {
    store: Arc<store::Store>,
}

impl CheckpointServer {
    pub fn new(store: Arc<store::Store>) -> CheckpointServer {
        CheckpointServer { store }
    }

    fn answer<'b>(&self, query: Query, bump: &'b Bump) -> store::Result<Option<&'b [u8]>> {
        match query {
            Query::Latest => self.summary(self.store.committed_checkpoint()?, bump),
            Query::ByDigest(digest) => {
                self.summary(self.store.checkpoint_by_digest(&digest)?, bump)
            }
            Query::BySequenceNumber(seq) => self.summary(Some(seq), bump),
            Query::Contents { digest, versioned } => self.contents(&digest, versioned, bump),
            Query::Availability => self.availability(bump),
        }
    }

    /// `Option<CertifiedCheckpointSummary>`.
    fn summary<'b>(&self, seq: Option<u64>, bump: &'b Bump) -> store::Result<Option<&'b [u8]>> {
        let certified = match seq {
            Some(seq) => self.store.certified(seq)?,
            None => None,
        };
        let mut w = Writer::new_in(bump, 1 + certified.as_ref().map_or(0, Vec::len));
        match certified {
            Some(certified) => {
                w.u8(1);
                w.raw(&certified);
            }
            None => w.u8(0),
        }
        Ok(Some(w.finish_bytes()))
    }

    /// `GetCheckpointAvailabilityResponse`; none before genesis is in.
    fn availability<'b>(&self, bump: &'b Bump) -> store::Result<Option<&'b [u8]>> {
        let Some(seq) = self.store.committed_checkpoint()? else {
            return Ok(None);
        };
        let Some(certified) = self.store.certified(seq)? else {
            return Ok(None);
        };
        let mut w = Writer::new_in(bump, certified.len() + 8);
        w.raw(&certified);
        w.u64(LOWEST_AVAILABLE);
        Ok(Some(w.finish_bytes()))
    }

    /// `Option<FullCheckpointContents>`, or with `versioned`
    /// `Option<VersionedFullCheckpointContents>`: the contents' transactions
    /// and effects from the store, then their signatures as the contents
    /// hold them. None if any is missing, as the reference answers.
    fn contents<'b>(
        &self,
        digest: &Digest,
        versioned: bool,
        bump: &'b Bump,
    ) -> store::Result<Option<&'b [u8]>> {
        const NONE: &[u8] = &[0];
        let Some(seq) = self.store.checkpoint_by_contents(digest)? else {
            return Ok(Some(NONE));
        };
        let checkpoint = self
            .store
            .checkpoint(seq)?
            .ok_or(store::Error::Inconsistent(
                "an indexed checkpoint is missing",
            ))?;
        let built = BuiltCheckpoint::decode(&checkpoint);
        let mut r = Reader::new(&built.contents);
        let contents = CheckpointContents::parse(&mut r, &mut BumpAlloc(bump))
            .expect("committed contents parse");

        let mut executed: containers::Vec<
            '_,
            (
                Message<SenderSignedData<'static>>,
                Message<TransactionEffects<'static>>,
            ),
        > = containers::Vec::new_in(bump);
        let digests = |i: usize| match contents.version {
            VersionedCheckpointContents::V1 { transactions, .. } => transactions[i],
            VersionedCheckpointContents::V2(transactions) => *transactions[i].digest,
        };
        let n = match contents.version {
            VersionedCheckpointContents::V1 { transactions, .. } => transactions.len(),
            VersionedCheckpointContents::V2(transactions) => transactions.len(),
        };
        for i in 0..n {
            let d = digests(i);
            let Some(transaction) = self.store.transaction(&d.transaction)? else {
                return Ok(Some(NONE));
            };
            // From contents this validator built: a computed digest.
            let Some(effects) = self.store.effects(&Hashed::computed(d.effects))? else {
                return Ok(Some(NONE));
            };
            executed.push((transaction, effects));
        }

        let size: usize = executed
            .iter()
            .map(|(t, e)| t.wire_bytes().len() + e.wire_bytes().len())
            .sum();
        let mut w = Writer::new_in(bump, 16 + size + contents.bytes.len());
        w.u8(1);
        if versioned {
            w.len_prefix(match contents.version {
                VersionedCheckpointContents::V1 { .. } => 0,
                VersionedCheckpointContents::V2(_) => 1,
            });
        }
        // `ExecutionData`: a `Transaction`'s envelope adds nothing to its
        // `SenderSignedData`.
        w.len_prefix(n);
        for (transaction, effects) in &executed {
            w.raw(transaction.wire_bytes());
            w.raw(effects.wire_bytes());
        }
        w.len_prefix(n);
        match contents.version {
            VersionedCheckpointContents::V1 {
                user_signatures, ..
            } => {
                for signatures in user_signatures {
                    w.len_prefix(signatures.len());
                    for signature in *signatures {
                        w.bytes(signature.0);
                    }
                }
            }
            VersionedCheckpointContents::V2(transactions) => {
                for transaction in transactions {
                    w.len_prefix(transaction.user_signatures.len());
                    for (signature, alias_version) in transaction.user_signatures {
                        w.bytes(signature.0);
                        // `FullCheckpointContents` has no alias versions.
                        if versioned {
                            match alias_version {
                                Some(version) => {
                                    w.u8(1);
                                    w.u64(*version);
                                }
                                None => w.u8(0),
                            }
                        }
                    }
                }
            }
        }
        Ok(Some(w.finish_bytes()))
    }
}

impl Processor for CheckpointServer {
    type Input = Request;
    type Output = ();

    fn process(&mut self, request: Request, bump: &mut Bump) -> Option<()> {
        if let Ok(Some(bcs)) = self.answer(request.query, bump) {
            let _ = request.reply.send(frame(bcs));
        }
        None
    }
}

fn frame(bcs: &[u8]) -> Bytes {
    let mut encoder = snap::write::FrameEncoder::new(Vec::new());
    encoder.write_all(bcs).expect("memory takes any write");
    encoder.into_inner().expect("memory takes any write").into()
}
