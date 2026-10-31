// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Following the certified checkpoint chain from peers
//! (`docs/state_sync_design.md`, Catching up).

pub mod contents;
pub mod item;

use std::collections::{BTreeMap, VecDeque};
use std::ops::RangeInclusive;
use std::sync::Arc;

use aggregator::{CertificateError, Committee};
use messages::arena::BumpAlloc;
use messages::base::{Digest, ObjectId, TransactionDigest};
use messages::checkpoint::{CertifiedCheckpointSummary, ExecutionData};
use messages::effects::{ObjectIn, VersionedEffects};
use messages::reader::Reader;
use messages::transaction::TransactionKind;
use tracing::warn;
use validator::pipeline::chunk_lock::ChunkLock;
use validator::pipeline::items::{
    BuiltWaitItem, BuiltWaits, CertifiedCheckpoint, CheckpointEffects, EffectsWaitItem,
    ExecutedEffects, InputCommand,
};
use validator::pipeline::keys::{CheckpointBuilt, EffectsKey, InputKey};
use validator::pipeline::tables::{BuiltCheckpoint, CheckpointStore};
use validator::randomness::BeaconInput;
use validator::state_sync::{SyncRequest, SyncedInput, Wanted};
use waiter::{Command, WaitBatch};
use workqueue::{Bump, Processor, PushError, Queue};

/// How far past the last committed checkpoint summaries are taken: each
/// waits in memory until committed.
pub const WINDOW: u64 = 256;

/// How many checkpoints' contents are asked for at once.
pub const CONTENTS_BATCH: usize = 4;

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

/// A verified summary's digest and contents digest, and whether it ends the
/// epoch.
struct Verified {
    digest: Digest,
    contents: Digest,
    ends: bool,
    /// Its summary lists randomness rounds.
    rounds: bool,
}

/// A checkpoint past the commit stage's last cut, until split.
struct Slow {
    certified: CertifiedCheckpoint,
    contents_digest: Digest,
    /// The last chunk height before it, if the cut was just before it; else
    /// the last of the checkpoint split before it.
    after: Option<u64>,
    /// Its `VersionedFullCheckpointContents`, once fetched and verified.
    contents: Option<Vec<u8>>,
}

/// A checkpoint split, until its transactions have executed.
struct Running {
    certified: CertifiedCheckpoint,
    contents_digest: Digest,
    contents: Vec<u8>,
    /// The chunk heights it took from the commit stage.
    synced: RangeInclusive<u64>,
    last_height: u64,
}

/// Where the scheduler's work goes.
pub struct Outputs {
    pub built: BuiltWaits,
    pub inputs: Queue<InputCommand>,
    pub effects: Queue<Command<EffectsKey, EffectsWaitItem>>,
    pub hasher: Queue<CheckpointEffects>,
    pub committer: Queue<Vec<CertifiedCheckpoint>>,
    pub beacon: Queue<BeaconInput>,
}

/// Verifies certified summaries fetched from peers, in sequence order, each
/// chained from the last verified: of the running epoch (checked before
/// anything else), the next sequence number, the last one's digest as its
/// previous, and a quorum of the epoch's committee
/// (`Committee::verify_certified`). A summary at or before the last verified
/// is skipped; one that does not verify ends its request, and the fetcher
/// tries another peer.
///
/// Then, under the chunk lock:
/// - **Fast path:** a checkpoint the commit stage has cut waits for this
///   validator to build it, then is checked for a fork and committed. If it
///   lists randomness rounds and is not built yet, its contents are fetched
///   for the rounds' bytes alone.
/// - **Slow path:** for one past the cut, it asks for the contents, checks
///   them against the summary, and splits them into chunks at their
///   barriers, numbered on from the cut. It runs the chunks past the commit
///   stage's, each transaction at the versions its certified effects give.
///   Once every transaction of the checkpoint has executed, by either
///   path, it checks each one's effects against the certified ones,
///   releases its chunks, and commits the checkpoint as certified.
///
/// Not yet: the epoch's last checkpoint takes the fast path.
///
/// Answers each request with what to fetch next: summaries up to `WINDOW`
/// past the last committed checkpoint, nothing once the epoch's last is
/// verified, and the contents of checkpoints past the cut, or wanted for
/// their rounds.
///
/// Inward: the fetcher (black box), and the effects waiter: `SyncedInput`.
///
/// Outward:
/// - built waiter, wait for: `BuiltWaitItem::Synced`, keyed by
///   `CheckpointBuilt(seq)`;
/// - input waiter, wait for: each certified transaction it runs, keyed by
///   its inputs (`item::executable`);
/// - effects waiter, wait for: `SyncedExecuted(seq)`, keyed by each
///   transaction's `EffectsKey`;
/// - `SignatureCollector`: the random bytes of each randomness state update
///   in the checkpoints it splits (`BeaconInput::Certified`);
/// - `StateHasher`: `CheckpointEffects` of each checkpoint it commits;
/// - `CheckpointCommitter`: each checkpoint it commits.
///
/// Reads: the store's executed effects. Writes: the checkpoint store, each
/// checkpoint it commits as built.
pub struct SyncedScheduler {
    committee: Arc<Committee>,
    store: Arc<store::Store>,
    checkpoints: Arc<CheckpointStore>,
    /// Closed when the scheduler is dropped, so the commit stage never
    /// waits on a chunk it held.
    chunk_lock: Arc<ChunkLock>,
    outputs: Outputs,
    /// The last checkpoint verified, or committed, and its digest.
    last: (u64, Digest),
    /// Whether the last is the epoch's last checkpoint.
    ended: bool,
    /// In sequence order.
    slow: VecDeque<Slow>,
    /// The last chunk height of the last checkpoint split.
    split_height: u64,
    running: BTreeMap<u64, Running>,
    /// Cut checkpoints with randomness rounds, not built here yet, by
    /// sequence number: their contents are fetched only for their rounds'
    /// bytes, which the commit stage's chunks may wait for.
    rounds_wanted: BTreeMap<u64, Digest>,
    /// Scratch, per transaction.
    keys: Vec<InputKey>,
    packages: Vec<ObjectId>,
}

impl SyncedScheduler {
    /// Chained from the last committed checkpoint, whose certificate `store`
    /// holds.
    pub fn new(
        committee: Arc<Committee>,
        store: Arc<store::Store>,
        checkpoints: Arc<CheckpointStore>,
        chunk_lock: Arc<ChunkLock>,
        outputs: Outputs,
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
            store,
            checkpoints,
            chunk_lock,
            outputs,
            slow: VecDeque::new(),
            split_height: 0,
            running: BTreeMap::new(),
            rounds_wanted: BTreeMap::new(),
            keys: Vec::new(),
            packages: Vec::new(),
        }
    }

    /// `certified`'s digests and whether it ends the epoch, if it is the
    /// next checkpoint; none if it is at or before the last.
    fn verify(&self, certified: &[u8], bump: &Bump) -> Result<Option<Verified>, Rejected> {
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
        Ok(Some(Verified {
            digest: summary.digest.into(),
            contents: *summary.content_digest,
            ends: summary.end_of_epoch_data.is_some(),
            rounds: lists_rounds(summary.version_specific_data),
        }))
    }

    fn request(&mut self, request: SyncRequest, bump: &mut Bump) {
        let mut waits = WaitBatch::new();
        for certified in request.summaries {
            if self.ended {
                break;
            }
            let verified = self.verify(&certified, bump);
            bump.reset();
            match verified {
                Ok(Some(verified)) => {
                    let seq = self.last.0 + 1;
                    self.last = (seq, verified.digest);
                    self.ended = verified.ends;
                    let certified = CertifiedCheckpoint {
                        seq,
                        summary: certified.into(),
                    };
                    let cut = self.chunk_lock.cut();
                    if seq <= cut.seq || verified.ends {
                        if verified.rounds && self.checkpoints.built(seq).is_none() {
                            self.rounds_wanted.insert(seq, verified.contents);
                        }
                        waits.push(
                            BuiltWaitItem::Synced {
                                certified,
                                local: None,
                            },
                            [CheckpointBuilt(seq)],
                        );
                    } else {
                        // Past the cut, the one before is the cut or was
                        // past it then: the cut only moves on.
                        self.slow.push_back(Slow {
                            certified,
                            contents_digest: verified.contents,
                            after: (cut.seq + 1 == seq).then_some(cut.height),
                            contents: None,
                        });
                    }
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
        if !waits.is_empty() {
            push(&self.outputs.built, Command::Wait(waits), "built waiter");
        }

        for (seq, bytes) in request.contents {
            if let Some(digest) = self.rounds_wanted.get(&seq) {
                match contents::verify(&bytes, digest, bump) {
                    Ok(contents) => {
                        self.feed_rounds(contents.transactions());
                        self.rounds_wanted.remove(&seq);
                    }
                    Err(why) => warn!(?why, seq, "fetched checkpoint contents are rejected"),
                }
                bump.reset();
                continue;
            }
            let Some(slow) = self
                .slow
                .iter_mut()
                .find(|s| s.certified.seq == seq && s.contents.is_none())
            else {
                continue;
            };
            match contents::verify(&bytes, &slow.contents_digest, bump) {
                Ok(_) => slow.contents = Some(bytes),
                Err(why) => warn!(?why, seq, "fetched checkpoint contents are rejected"),
            }
            bump.reset();
        }
        while self.slow.front().is_some_and(|s| s.contents.is_some()) {
            let slow = self.slow.pop_front().expect("a front");
            self.split(slow, bump);
            bump.reset();
        }

        let committed = self.checkpoints.last_committed();
        let checkpoints = &self.checkpoints;
        self.rounds_wanted
            .retain(|&seq, _| seq > committed && checkpoints.built(seq).is_none());
        let mut contents: Vec<_> = self
            .slow
            .iter()
            .filter(|s| s.contents.is_none())
            .map(|s| (s.certified.seq, s.contents_digest))
            .chain(
                self.rounds_wanted
                    .iter()
                    .map(|(&seq, &digest)| (seq, digest)),
            )
            .collect();
        contents.sort_unstable_by_key(|(seq, _)| *seq);
        contents.truncate(CONTENTS_BATCH);
        let _ = request.reply.send(Wanted {
            next: (!self.ended).then_some(self.last.0 + 1),
            room: (committed + WINDOW).saturating_sub(self.last.0),
            contents,
        });
    }

    /// Splits `slow`'s contents into chunks, takes those past the commit
    /// stage's, and runs their transactions; then waits for every
    /// transaction of the checkpoint to execute.
    fn split(&mut self, slow: Slow, bump: &Bump) {
        let bytes = slow.contents.expect("fetched");
        let contents =
            contents::verify(&bytes, &slow.contents_digest, bump).expect("verified as fetched");
        let transactions = contents.transactions();
        let ends = contents::chunk_ends(transactions);
        assert_eq!(
            ends.last().copied(),
            Some(transactions.len()),
            "a checkpoint before the epoch's last ends with a barrier"
        );
        self.feed_rounds(transactions);
        let first = slow.after.unwrap_or(self.split_height) + 1;
        let last = first + ends.len() as u64 - 1;
        let handled = self.chunk_lock.take(first, last);
        self.split_height = last;

        let mut runs = WaitBatch::new();
        let mut start = 0;
        for (i, &end) in ends.iter().enumerate() {
            let height = first + i as u64;
            if height > handled {
                let root_version = barrier_root_version(&transactions[end - 1]);
                for t in &transactions[start..end] {
                    let deleter =
                        |id: &ObjectId, version: u64, dependencies: &[TransactionDigest]| {
                            deleter(
                                &self.store,
                                &self.running,
                                transactions,
                                id,
                                version,
                                dependencies,
                            )
                        };
                    let executable = item::executable(
                        t,
                        height,
                        root_version,
                        deleter,
                        &mut self.keys,
                        &mut self.packages,
                    );
                    runs.push(executable, self.keys.drain(..));
                }
            }
            start = end;
        }
        if !runs.is_empty() {
            push(&self.outputs.inputs, Command::Wait(runs), "input waiter");
        }
        let seq = slow.certified.seq;
        let mut executed = WaitBatch::with_capacity(1, transactions.len());
        executed.push(
            EffectsWaitItem::SyncedExecuted(seq),
            transactions
                .iter()
                .map(|t| EffectsKey(*t.transaction.digest().digest())),
        );
        push(
            &self.outputs.effects,
            Command::Wait(executed),
            "effects waiter",
        );
        self.running.insert(
            seq,
            Running {
                certified: slow.certified,
                contents_digest: slow.contents_digest,
                contents: bytes,
                synced: (handled + 1).max(first)..=last,
                last_height: last,
            },
        );
    }

    /// Gives the collector the random bytes of each randomness state update
    /// in `transactions`: the commit stage's chunks headed by an update wait
    /// for its round's bytes, which the quorum may have stopped sending. In
    /// checkpoint order, as the collector takes rounds in a window.
    fn feed_rounds(&self, transactions: &[ExecutionData<'_>]) {
        for t in transactions {
            if let TransactionKind::RandomnessStateUpdate(update) = t.transaction.data().kind() {
                let update = update.get();
                push(
                    &self.outputs.beacon,
                    BeaconInput::Certified {
                        round: update.randomness_round,
                        bytes: update.random_bytes.to_vec(),
                    },
                    "signature collector",
                );
            }
        }
    }

    /// Every transaction of checkpoint `seq` has executed: checks each one's
    /// effects, releases its chunks, and commits it.
    ///
    /// # Panics
    /// If a transaction's effects are not the certified ones: this
    /// validator, or the quorum, forked.
    fn executed(&mut self, seq: u64, bump: &Bump) {
        let running = self.running.remove(&seq).expect("a checkpoint it ran");
        let contents = contents::verify(&running.contents, &running.contents_digest, bump)
            .expect("verified as fetched");
        let mut effects = Vec::with_capacity(contents.transactions().len());
        for t in contents.transactions() {
            let digest = *t.transaction.digest();
            let executed = self
                .store
                .executed_effects(&digest)
                .expect("the store reads")
                .expect("executed");
            assert!(
                executed == t.effects.digest,
                "fork: transaction {digest:?} of certified checkpoint {seq} has other effects than certified"
            );
            effects.push(ExecutedEffects {
                digest,
                effects_digest: executed,
                effects: t.effects.bytes.into(),
            });
        }
        for height in running.synced {
            self.chunk_lock.release(height);
        }

        let summary = CertifiedCheckpointSummary::parse(
            &mut Reader::new(&running.certified.summary),
            &mut BumpAlloc(bump),
        )
        .expect("a verified summary parses")
        .data;
        let gas = summary.epoch_rolling_gas_cost_summary;
        self.checkpoints.insert_built(BuiltCheckpoint {
            seq,
            summary: summary.bytes.to_vec(),
            digest: summary.digest.bytes,
            contents: contents.contents.bytes.to_vec(),
            network_total_transactions: summary.network_total_transactions,
            epoch_rolling_gas: [
                gas.computation_cost,
                gas.storage_cost,
                gas.storage_rebate,
                gas.non_refundable_storage_fee,
            ],
            last_height: running.last_height,
            timestamp_ms: summary.timestamp_ms,
            root_state: None,
        });
        push(
            &self.outputs.hasher,
            CheckpointEffects {
                seq,
                effects,
                last_of_epoch: false,
            },
            "state hasher",
        );
        push(
            &self.outputs.committer,
            vec![running.certified],
            "committer",
        );
    }
}

/// Whether `version_specific_data`, `CheckpointVersionSpecificData` BCS,
/// lists randomness rounds: V1's tag, then a nonzero count.
fn lists_rounds(version_specific_data: &[u8]) -> bool {
    matches!(version_specific_data, [0, n, ..] if *n != 0)
}

/// The accumulator root's version a chunk's transactions read: the input
/// version of its barrier, `barrier` (R4.4).
fn barrier_root_version(barrier: &ExecutionData<'_>) -> u64 {
    let VersionedEffects::V2(effects) = &barrier.effects.version else {
        unreachable!("verified effects are V2");
    };
    effects
        .changed_objects
        .iter()
        .find(|c| *c.id == contents::ACCUMULATOR_ROOT)
        .and_then(|c| match c.input_state {
            ObjectIn::Exist { version, .. } => Some(version),
            ObjectIn::NotExist => None,
        })
        .expect("a barrier changes the existing root")
}

/// The transaction among `dependencies` that ended `id`'s stream at
/// `version`: in this checkpoint's `transactions`, a checkpoint `running`,
/// or the store.
///
/// # Panics
/// If none did: the quorum certified effects no execution produces.
fn deleter(
    store: &store::Store,
    running: &BTreeMap<u64, Running>,
    transactions: &[ExecutionData<'_>],
    id: &ObjectId,
    version: u64,
    dependencies: &[TransactionDigest],
) -> [u8; 32] {
    let ended = |t: &ExecutionData<'_>| {
        let VersionedEffects::V2(effects) = &t.effects.version else {
            return false;
        };
        dependencies.contains(t.transaction.digest().digest())
            && effects.lamport_version == version
            && effects.changed_objects.iter().any(|c| c.id == id)
    };
    if let Some(t) = transactions.iter().find(|t| ended(t)) {
        return t.transaction.digest().bytes;
    }
    let bump = Bump::default();
    for r in running.values() {
        let contents =
            contents::verify(&r.contents, &r.contents_digest, &bump).expect("verified as fetched");
        if let Some(t) = contents.transactions().iter().find(|t| ended(t)) {
            return t.transaction.digest().bytes;
        }
    }
    for dependency in dependencies {
        let Some(effects_digest) = store
            .executed_effects_named(dependency)
            .expect("the store reads")
        else {
            continue;
        };
        let effects = store
            .effects(&effects_digest)
            .expect("the store reads")
            .expect("executed effects are stored");
        let VersionedEffects::V2(v2) = &effects.get().version else {
            continue;
        };
        if v2.lamport_version == version && v2.changed_objects.iter().any(|c| c.id == id) {
            return dependency.bytes;
        }
    }
    panic!("no dependency ended {id:?} at version {version}");
}

/// Inside the pipeline a full queue is a sizing bug; a closed one means the
/// pipeline is shutting down.
fn push<T>(queue: &Queue<T>, item: T, name: &str) {
    if let Err(PushError::Full(_)) = queue.try_push(item) {
        panic!("the {name} queue refused synced work");
    }
}

impl Drop for SyncedScheduler {
    fn drop(&mut self) {
        self.chunk_lock.close();
    }
}

impl Processor for SyncedScheduler {
    type Input = SyncedInput;
    type Output = ();

    fn process(&mut self, input: SyncedInput, bump: &mut Bump) -> Option<()> {
        match input {
            SyncedInput::Request(request) => self.request(request, bump),
            SyncedInput::Executed(seq) => self.executed(seq, bump),
        }
        None
    }
}
