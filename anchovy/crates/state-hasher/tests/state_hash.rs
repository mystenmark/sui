// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The state hashes against the reference's `accumulate_effects_v3` and
//! running root, over the same effects decoded by sui-types.

mod support;

use std::sync::Arc;

use fastcrypto::hash::MultisetHash;
use messages::fast::Bump;
use state_hasher::{StateHash, StateHasher, StateHashes, accumulate};
use sui_types::effects::{TransactionEffects, TransactionEffectsAPI};
use sui_types::global_state_hash::GlobalStateHash;
use support::effects::{KINDS, Rng, checkpoint, transaction};
use validator::pipeline::items::{CheckpointEffects, ExecutedEffects};
use workqueue::Processor;

/// Empty state hashes, over an epoch store in a directory kept alive with
/// them.
fn state_hashes() -> (Arc<StateHashes>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let (store, _, builder) = validator::epoch_store::EpochStore::open(dir.path(), 0);
    (Arc::new(StateHashes::new(store, builder)), dir)
}

/// The reference's `accumulate_effects_v3`.
fn reference(effects: &[ExecutedEffects]) -> GlobalStateHash {
    let effects: Vec<TransactionEffects> = effects
        .iter()
        .map(|e| bcs::from_bytes(&e.effects).unwrap())
        .collect();
    let mut acc = GlobalStateHash::default();
    acc.insert_all(
        effects
            .iter()
            .flat_map(|fx| fx.all_changed_objects().into_iter().map(|(r, _, _)| r.2))
            .collect::<Vec<_>>(),
    );
    acc.remove_all(
        effects
            .iter()
            .flat_map(|fx| fx.old_object_metadata().into_iter().map(|(r, _)| r.2))
            .collect::<Vec<_>>(),
    );
    acc
}

fn checkpoints(rng: &mut Rng, count: usize) -> Vec<Vec<ExecutedEffects>> {
    (0..count)
        .map(|_| {
            let transactions = rng.below(20) as usize;
            checkpoint(rng, transactions)
        })
        .collect()
}

/// The reference's running roots, from `prior`.
fn reference_roots(
    prior: &GlobalStateHash,
    checkpoints: &[Vec<ExecutedEffects>],
) -> Vec<StateHash> {
    let mut root = prior.clone();
    checkpoints
        .iter()
        .map(|effects| {
            root.union(&reference(effects));
            root.clone()
        })
        .collect()
}

#[test]
fn each_kind_of_change_matches_the_reference() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut bump = Bump::with_capacity(1024);
    for kind in 0..KINDS {
        let effects: Vec<_> = (0..3).map(|_| transaction(&mut rng, 4, |_| kind)).collect();
        assert_eq!(
            accumulate(&effects, &mut bump),
            reference(&effects),
            "kind {kind}"
        );
    }
}

#[test]
fn checkpoints_match_the_reference() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut bump = Bump::with_capacity(1024);
    for effects in checkpoints(&mut rng, 30) {
        assert_eq!(accumulate(&effects, &mut bump), reference(&effects));
    }
    assert_eq!(accumulate(&[], &mut bump), StateHash::default());
}

#[test]
fn effects_that_overflow_the_parse_arena_match() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let effects = vec![transaction(&mut rng, 2000, |rng| rng.below(KINDS))];
    assert_eq!(
        accumulate(&effects, &mut Bump::with_capacity(64)),
        reference(&effects)
    );
}

#[test]
fn roots_fold_in_order() {
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    let checkpoints = checkpoints(&mut rng, 10);
    let expected = reference_roots(&GlobalStateHash::default(), &checkpoints);
    let (hashes, _dir) = state_hashes();
    let mut processor = StateHasher::new(hashes.clone(), 0, StateHash::default());
    for (seq, effects) in checkpoints.iter().enumerate() {
        let seq = seq as u64;
        assert!(
            processor
                .process(
                    CheckpointEffects {
                        last_of_epoch: false,
                        seq,
                        effects: effects.clone()
                    },
                    &mut workqueue::Bump::default()
                )
                .is_none()
        );
        assert_eq!(hashes.checkpoint(seq), Some(reference(effects)));
        assert_eq!(hashes.root(seq).as_ref(), Some(&expected[seq as usize]));
        assert_eq!(
            hashes.latest_root(),
            Some((seq, expected[seq as usize].clone()))
        );
    }
}

#[test]
fn roots_fold_in_order_whatever_the_arrival_order() {
    let mut rng = Rng(0x0bad_c0de_1357_9bdf);
    let first = 40;
    let mut prior = GlobalStateHash::default();
    prior.insert([1; 32]);
    let checkpoints = checkpoints(&mut rng, 12);
    let expected = reference_roots(&prior, &checkpoints);

    let mut order: Vec<usize> = (0..checkpoints.len()).collect();
    for i in (1..order.len()).rev() {
        order.swap(i, rng.below(i as u64 + 1) as usize);
    }
    if order[0] == 0 {
        order.swap(0, 1);
    }

    let (hashes, _dir) = state_hashes();
    let mut processor = StateHasher::new(hashes.clone(), first, prior);
    let mut arrived = vec![false; checkpoints.len()];
    for i in order {
        let seq = first + i as u64;
        processor.process(
            CheckpointEffects {
                last_of_epoch: false,
                seq,
                effects: checkpoints[i].clone(),
            },
            &mut workqueue::Bump::default(),
        );
        arrived[i] = true;
        assert_eq!(hashes.checkpoint(seq), Some(reference(&checkpoints[i])));
        let folded = arrived.iter().take_while(|a| **a).count();
        for (j, root) in expected.iter().enumerate() {
            let got = hashes.root(first + j as u64);
            if j < folded {
                assert_eq!(got.as_ref(), Some(root));
            } else {
                assert_eq!(got, None);
            }
        }
        assert_eq!(
            hashes.latest_root().map(|(seq, _)| seq),
            folded.checked_sub(1).map(|j| first + j as u64)
        );
    }
    assert_eq!(hashes.root(first - 1), None);
}

#[test]
#[should_panic(expected = "hashed twice")]
fn a_checkpoint_hashed_twice_panics() {
    let (hashes, _dir) = state_hashes();
    let mut processor = StateHasher::new(hashes, 0, StateHash::default());
    for _ in 0..2 {
        processor.process(
            CheckpointEffects {
                last_of_epoch: false,
                seq: 3,
                effects: vec![],
            },
            &mut workqueue::Bump::default(),
        );
    }
}
