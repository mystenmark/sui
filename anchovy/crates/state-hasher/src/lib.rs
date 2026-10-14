// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The state hash (ECMH) of each checkpoint and the running root
//! (`docs/consensus_pipeline.md`, Checkpointing).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use fastcrypto::hash::MultisetHash;
use messages::arena::BumpAlloc;
use messages::effects::{ChangeKind, ObjectIn, ObjectOut, TransactionEffects, VersionedEffects};
use messages::fast::Bump;
use messages::reader::Reader;
use workqueue::Processor;

use validator::pipeline::items::{CheckpointEffects, ExecutedEffects};

/// The reference's `GlobalStateHash`: an ECMH over object digests.
pub use fastcrypto::hash::EllipticCurveMultisetHash as StateHash;

/// Arena for parsing one transaction's effects; larger effects spill into
/// extra chunks, freed at the next reset.
const PARSE_ARENA: usize = 64 * 1024;

/// What one checkpoint's effects add to the live object set and remove from
/// it, as the reference's `accumulate_effects_v3` (effects V2): every
/// created, mutated or unwrapped object's new digest is inserted, and every
/// object that existed before the transaction has its old digest removed.
///
/// `bump` is scratch for parsing, reset per transaction.
///
/// # Panics
/// On effects that do not parse or are V1. Both come only from this
/// validator's own executor at a protocol version with effects V2.
pub fn accumulate(effects: &[ExecutedEffects], bump: &mut Bump) -> StateHash {
    let mut acc = StateHash::default();
    for executed in effects {
        bump.reset();
        let mut r = Reader::new(&executed.effects);
        let parsed = TransactionEffects::parse(&mut r, &mut BumpAlloc(bump))
            .and_then(|e| r.finish().map(|()| e))
            .expect("executed effects parse");
        let VersionedEffects::V2(v2) = parsed.version else {
            panic!("the state hash covers effects V2 only");
        };
        for change in v2.changed_objects {
            // The reference's `created()`, `mutated()` and `unwrapped()`.
            let written = match (change.kind, &change.output_state) {
                (
                    ChangeKind::Created | ChangeKind::Mutated | ChangeKind::Unwrapped,
                    ObjectOut::ObjectWrite(digest, _) | ObjectOut::PackageWrite(_, digest),
                ) => Some(*digest),
                _ => None,
            };
            if let Some(digest) = written {
                acc.insert(digest.bytes);
            }
            // The reference's `old_object_metadata()`.
            if let ObjectIn::Exist { digest, .. } = &change.input_state {
                acc.remove(digest.bytes);
            }
        }
    }
    acc
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Every critical section is a single map operation, which leaves the
    // maps consistent even if it panicked.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Default)]
struct Hashes {
    checkpoints: BTreeMap<u64, StateHash>,
    roots: BTreeMap<u64, StateHash>,
}

/// The epoch's state hashes, shared between `StateHasher` (the only writer)
/// and their readers: the end of the epoch reads the running root through
/// the epoch's last checkpoint (its ECMH digest is the epoch's live object
/// set commitment), and the checkpoint executor reads per-checkpoint
/// hashes.
///
/// A checkpoint's hash is recorded as soon as it is computed; its running
/// root only after every earlier root of the epoch, so `root(seq)` present
/// means every root before it is too.
#[derive(Default)]
pub struct StateHashes {
    hashes: Mutex<Hashes>,
}

impl StateHashes {
    pub fn new() -> StateHashes {
        StateHashes::default()
    }

    /// What checkpoint `seq`'s effects added and removed, if computed.
    pub fn checkpoint(&self, seq: u64) -> Option<StateHash> {
        lock(&self.hashes).checkpoints.get(&seq).cloned()
    }

    /// The live object set's hash after checkpoint `seq`, if folded.
    pub fn root(&self, seq: u64) -> Option<StateHash> {
        lock(&self.hashes).roots.get(&seq).cloned()
    }

    /// The last checkpoint folded into the root, and the root after it.
    pub fn latest_root(&self) -> Option<(u64, StateHash)> {
        lock(&self.hashes)
            .roots
            .last_key_value()
            .map(|(seq, root)| (*seq, root.clone()))
    }

    fn insert_checkpoint(&self, seq: u64, hash: StateHash) {
        lock(&self.hashes).checkpoints.insert(seq, hash);
    }

    fn insert_root(&self, seq: u64, root: StateHash) {
        lock(&self.hashes).roots.insert(seq, root);
    }
}

/// Computes each checkpoint's ECMH as its effects arrive (insert each
/// changed object's digest, remove each old one's), then folds it into the
/// running root once the previous root is folded.
///
/// Checkpoints may arrive in any order. The previous root is this
/// processor's own state, so the wait for it is the `pending` map: a
/// checkpoint's hash is kept there until every earlier one is folded.
///
/// Inward:
/// - `ContentsBuilder`: `CheckpointEffects`.
///
/// Outward: none. Writes `StateHashes`, which the end of the epoch and the
/// checkpoint executor read.
///
/// Starts from the root before the epoch's first checkpoint, given at
/// construction. Loading it from the store on restart and handing the
/// final root to the next epoch belong to reconfiguration, not covered yet.
pub struct StateHasher {
    hashes: Arc<StateHashes>,
    /// The next checkpoint to fold, and the root before it.
    next: u64,
    root: StateHash,
    /// Computed, but some earlier checkpoint is not.
    pending: BTreeMap<u64, StateHash>,
    bump: Bump,
}

impl StateHasher {
    /// Folds from checkpoint `first`, onto `prior`: the root after `first - 1`
    /// (empty at genesis).
    pub fn new(hashes: Arc<StateHashes>, first: u64, prior: StateHash) -> StateHasher {
        StateHasher {
            hashes,
            next: first,
            root: prior,
            pending: BTreeMap::new(),
            bump: Bump::with_capacity(PARSE_ARENA),
        }
    }
}

impl Processor for StateHasher {
    type Input = CheckpointEffects;
    type Output = ();

    fn process(&mut self, effects: CheckpointEffects) -> Option<()> {
        let seq = effects.seq;
        assert!(
            seq >= self.next && !self.pending.contains_key(&seq),
            "checkpoint {seq} hashed twice"
        );
        let hash = accumulate(&effects.effects, &mut self.bump);
        self.hashes.insert_checkpoint(seq, hash.clone());
        self.pending.insert(seq, hash);
        while let Some(hash) = self.pending.remove(&self.next) {
            self.root.union(&hash);
            self.hashes.insert_root(self.next, self.root.clone());
            self.next += 1;
        }
        None
    }
}
