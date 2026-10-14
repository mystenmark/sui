// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The order of a chunk's effects in its checkpoint, as the reference's
//! `CausalOrder::order_for_checkpoint` gives it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use messages::effects::{ObjectIn, TransactionEffects, UnchangedConsensusKind, VersionedEffects};
use messages::object::Owner;

type Digest = [u8; 32];
type ObjectKey = ([u8; 32], u64);

/// The positions of `effects`, given in schedule order, in checkpoint order.
/// With `keep_schedule_order` (`disable_effects_tx_dependencies`) that is
/// schedule order. Otherwise the prologue, if `prologue_first`, stays first
/// and the rest are sorted causally, independently of their given order.
pub fn checkpoint_order(
    effects: &[&TransactionEffects<'_>],
    prologue_first: bool,
    keep_schedule_order: bool,
) -> Vec<usize> {
    if keep_schedule_order || effects.is_empty() {
        return (0..effects.len()).collect();
    }
    let skip = usize::from(prologue_first);
    let mut order = Vec::with_capacity(effects.len());
    order.extend(0..skip);
    causal_sort(effects, skip, &mut order);
    order
}

fn transaction_digest(effects: &TransactionEffects<'_>) -> Digest {
    match &effects.version {
        VersionedEffects::V1(v1) => v1.transaction_digest.bytes,
        VersionedEffects::V2(v2) => v2.transaction_digest.bytes,
    }
}

fn dependencies<'a>(effects: &TransactionEffects<'a>) -> &'a [messages::base::Digest] {
    match &effects.version {
        VersionedEffects::V1(v1) => v1.dependencies,
        VersionedEffects::V2(v2) => v2.dependencies,
    }
}

/// Reads and writes of consensus objects (`accessed_consensus_objects`),
/// keyed by the version accessed. V1 effects contribute none.
fn consensus_accesses(
    effects: &TransactionEffects<'_>,
    mut read: impl FnMut(ObjectKey),
    mut write: impl FnMut(ObjectKey),
) {
    let VersionedEffects::V2(v2) = &effects.version else {
        return;
    };
    for change in v2.changed_objects {
        if let ObjectIn::Exist { version, owner, .. } = change.input_state
            && matches!(
                owner,
                Owner::Shared { .. } | Owner::ConsensusAddressOwner { .. }
            )
        {
            write((change.id.0, version));
        }
    }
    for (id, kind) in v2.unchanged_consensus_objects {
        match *kind {
            UnchangedConsensusKind::ReadOnlyRoot(version, _)
            | UnchangedConsensusKind::ReadConsensusStreamEnded(version) => read((id.0, version)),
            UnchangedConsensusKind::MutateConsensusStreamEnded(version) => write((id.0, version)),
            UnchangedConsensusKind::Cancelled(_) | UnchangedConsensusKind::PerEpochConfig => {}
        }
    }
}

/// `CausalOrder::causal_sort` of `effects[skip..]`, appended to `order`:
/// each transaction after its dependencies, and after the readers of every
/// version it overwrites. Ties go to the smallest digest, and a
/// transaction's dependencies are visited largest digest first.
fn causal_sort(effects: &[&TransactionEffects<'_>], skip: usize, order: &mut Vec<usize>) {
    let effects_rest = &effects[skip..];
    let mut readers: HashMap<ObjectKey, Vec<Digest>> = HashMap::new();
    let mut overwrites: HashMap<Digest, Vec<ObjectKey>> = HashMap::new();
    for e in effects_rest {
        let digest = transaction_digest(e);
        consensus_accesses(
            e,
            |key| readers.entry(key).or_default().push(digest),
            |key| overwrites.entry(digest).or_default().push(key),
        );
    }

    let mut not_seen: BTreeMap<Digest, (usize, BTreeSet<Digest>)> = BTreeMap::new();
    for (i, e) in effects_rest.iter().enumerate() {
        let digest = transaction_digest(e);
        let mut deps: BTreeSet<Digest> = dependencies(e).iter().map(|d| d.bytes).collect();
        for key in overwrites.get(&digest).into_iter().flatten() {
            deps.extend(readers.get(key).into_iter().flatten().copied());
        }
        not_seen.insert(digest, (skip + i, deps));
    }

    // Each frame: a transaction, and its dependencies not yet visited, in
    // ascending order so that `pop` takes the largest.
    let mut stack: Vec<(usize, Vec<Digest>)> = Vec::new();
    while let Some((_, (index, deps))) = not_seen.pop_first() {
        stack.push((index, deps.into_iter().collect()));
        while let Some((index, deps)) = stack.last_mut() {
            let mut next = None;
            while let Some(dep) = deps.pop() {
                if let Some((dep_index, dep_deps)) = not_seen.remove(&dep) {
                    next = Some((dep_index, dep_deps.into_iter().collect()));
                    break;
                }
            }
            if let Some(frame) = next {
                stack.push(frame);
            } else {
                order.push(*index);
                stack.pop();
            }
        }
    }
}
