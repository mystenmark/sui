// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Checkpoint order, on the reference's own `CausalOrder` test cases.

use messages::Message;
use messages::build::base::{
    AccountAddress, Digest, ObjectDigest, ObjectId, SequenceNumber, TransactionDigest,
};
use messages::build::effects::{
    GasCostSummary, TransactionEffects, TransactionEffectsV2, UnchangedConsensusKind,
};
use messages::build::execution_status::ExecutionStatus;
use messages::effects::TransactionEffects as View;
use settlement::causal::checkpoint_order;

fn d(i: u8) -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes[0] = i;
    bytes
}

/// Effects of `d(digest)` depending on `d(dependencies)`, reading
/// (`ReadOnlyRoot`) or writing (a stream ended under it) object 1 at
/// version 1.
fn effects(digest: u8, dependencies: &[u8], access: Option<bool>) -> Message<View<'static>> {
    let unchanged = access
        .map(|write| {
            let id = ObjectId(AccountAddress(d(1)));
            let kind = if write {
                UnchangedConsensusKind::MutateConsensusStreamEnded(SequenceNumber(1))
            } else {
                UnchangedConsensusKind::ReadOnlyRoot((
                    SequenceNumber(1),
                    ObjectDigest(Digest([0; 32])),
                ))
            };
            (id, kind)
        })
        .into_iter()
        .collect();
    let bytes = bcs::to_bytes(&TransactionEffects::V2(Box::new(TransactionEffectsV2 {
        status: ExecutionStatus::Success,
        executed_epoch: 0,
        gas_used: GasCostSummary {
            computation_cost: 0,
            storage_cost: 0,
            storage_rebate: 0,
            non_refundable_storage_fee: 0,
        },
        transaction_digest: TransactionDigest(Digest(d(digest))),
        gas_object_index: None,
        events_digest: None,
        dependencies: dependencies
            .iter()
            .map(|&i| TransactionDigest(Digest(d(i))))
            .collect(),
        lamport_version: SequenceNumber(2),
        changed_objects: vec![],
        unchanged_consensus_objects: unchanged,
        aux_data_digest: None,
    })))
    .unwrap();
    Message::parse(bytes).map_err(|(e, _)| e).unwrap()
}

/// The first digest byte of each effects, in checkpoint order.
fn order(effects: &[Message<View<'static>>], prologue_first: bool) -> Vec<u8> {
    let views: Vec<&View<'_>> = effects.iter().map(Message::get).collect();
    let digest = |i: usize| match &views[i].version {
        messages::effects::VersionedEffects::V2(v2) => v2.transaction_digest.bytes[0],
        messages::effects::VersionedEffects::V1(_) => unreachable!(),
    };
    checkpoint_order(&views, prologue_first, false)
        .into_iter()
        .map(digest)
        .collect()
}

#[test]
fn dependencies_first_then_by_digest() {
    let (e1, e2, e3, e4) = (
        effects(1, &[2, 3], None),
        effects(2, &[3, 4], None),
        effects(3, &[], None),
        effects(4, &[], None),
    );
    assert_eq!(order(&[e1, e2, e3, e4], false), [3, 4, 2, 1]);
    let (e1, e4) = (effects(1, &[2, 3], None), effects(4, &[], None));
    assert_eq!(order(&[e1, e4], false), [1, 4]);
    let (e1, e4) = (effects(1, &[2, 3], None), effects(4, &[], None));
    assert_eq!(order(&[e4, e1], false), [1, 4]);
}

#[test]
fn a_writer_follows_the_readers_of_the_version_it_overwrites() {
    let all = [
        effects(5, &[], Some(false)),
        effects(2, &[], Some(false)),
        effects(3, &[], Some(true)),
    ];
    let order = order(&all, false);
    assert_eq!(order.len(), 3);
    assert_eq!(order[2], 3);
}

#[test]
fn the_prologue_stays_first() {
    let all = [
        effects(9, &[], None),
        effects(1, &[2], None),
        effects(2, &[], None),
    ];
    assert_eq!(order(&all, true), [9, 2, 1]);
    assert_eq!(order(&all, false), [2, 1, 9]);
}

#[test]
fn schedule_order_when_effects_carry_no_dependencies() {
    let all = [effects(3, &[], None), effects(1, &[3], None)];
    let views: Vec<&View<'_>> = all.iter().map(Message::get).collect();
    assert_eq!(checkpoint_order(&views, true, true), [0, 1]);
}
