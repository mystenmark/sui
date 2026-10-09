// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `ExecutionInputs`' derivations against sui-types' `InputObjects` over random input sets.

mod common;

use common::*;
use containers::Bump;
use executor::effects::SharedInput as PortSharedInput;
use executor::inputs::{
    CONGESTED, ExecutionInputs, InputObjectKind as PortKind, InputState, LoadedInput,
    RANDOMNESS_UNAVAILABLE,
};
use messages::transaction::SharedObjectMutability as PortMutability;
use sui_types::base_types::{ObjectRef, SequenceNumber};
use sui_types::execution::SharedInput;
use sui_types::object::{Object, Owner};
use sui_types::transaction::{
    InputObjectKind, InputObjects, ObjectReadResult, ObjectReadResultKind, SharedObjectMutability,
};

fn mutability(rng: &mut Rng) -> (SharedObjectMutability, PortMutability) {
    match rng.below(3) {
        0 => (SharedObjectMutability::Immutable, PortMutability::Immutable),
        1 => (SharedObjectMutability::Mutable, PortMutability::Mutable),
        _ => (
            SharedObjectMutability::NonExclusiveWrite,
            PortMutability::NonExclusiveWrite,
        ),
    }
}

fn port_ref(r: &ObjectRef) -> exec_types::base::ObjectRef {
    (oid(r.0), r.1.value(), digest(r.2))
}

/// One random input, in both worlds.
fn input<'a>(
    rng: &mut Rng,
    bump: &'a Bump,
    cancel_reason: SequenceNumber,
) -> (ObjectReadResult, LoadedInput<'a>) {
    let id = rng.id();
    let version = 1 + rng.below(1000);
    match rng.below(3) {
        0 => {
            let object = package(rng, id, version);
            let port = LoadedInput::new(
                PortKind::MovePackage(oid(id)),
                InputState::Object(stored_object(bump, &object)),
            );
            let reference = ObjectReadResult::new(
                InputObjectKind::MovePackage(id),
                ObjectReadResultKind::Object(object),
            );
            (reference, port)
        }
        1 => {
            let owner = if rng.below(4) == 0 {
                Owner::Immutable
            } else {
                Owner::AddressOwner(rng.address())
            };
            let object = coin(rng, id, version, owner);
            let oref = object.compute_object_reference();
            let port = LoadedInput::new(
                PortKind::ImmOrOwnedMoveObject(port_ref(&oref)),
                InputState::Object(stored_object(bump, &object)),
            );
            let reference = ObjectReadResult::new(
                InputObjectKind::ImmOrOwnedMoveObject(oref),
                ObjectReadResultKind::Object(object),
            );
            (reference, port)
        }
        _ => {
            let initial_shared_version = SequenceNumber::from(1 + rng.below(version));
            let (m, port_m) = mutability(rng);
            let kind = InputObjectKind::SharedMoveObject {
                id,
                initial_shared_version,
                mutability: m,
            };
            let port_kind = PortKind::SharedMoveObject {
                id: oid(id),
                initial_shared_version: initial_shared_version.value(),
                mutability: port_m,
            };
            let (state, port_state) = match rng.below(4) {
                0 => {
                    let tx = rng.tx_digest();
                    (
                        ObjectReadResultKind::ObjectConsensusStreamEnded(
                            SequenceNumber::from(version),
                            tx,
                        ),
                        InputState::ConsensusStreamEnded(version, digest(tx)),
                    )
                }
                1 => {
                    // A cancelled transaction's other shared objects are read-cancelled.
                    let v = if rng.below(2) == 0 {
                        cancel_reason
                    } else {
                        SequenceNumber::CANCELLED_READ
                    };
                    (
                        ObjectReadResultKind::CancelledTransactionSharedObject(v),
                        InputState::Cancelled(v.value()),
                    )
                }
                _ => {
                    let owner = Owner::Shared {
                        initial_shared_version,
                    };
                    let object = coin(rng, id, version, owner);
                    (
                        ObjectReadResultKind::Object(object.clone()),
                        InputState::Object(stored_object(bump, &object)),
                    )
                }
            };
            (
                ObjectReadResult::new(kind, state),
                LoadedInput::new(port_kind, port_state),
            )
        }
    }
}

fn shared_input(s: &SharedInput) -> PortSharedInput {
    match s {
        SharedInput::Existing(r) => PortSharedInput::Existing(port_ref(r)),
        SharedInput::ConsensusStreamEnded((id, v, m, tx)) => {
            let m = match m {
                SharedObjectMutability::Immutable => PortMutability::Immutable,
                SharedObjectMutability::Mutable => PortMutability::Mutable,
                SharedObjectMutability::NonExclusiveWrite => PortMutability::NonExclusiveWrite,
            };
            PortSharedInput::ConsensusStreamEnded((oid(*id), v.value(), m, digest(tx)))
        }
        SharedInput::Cancelled((id, v)) => PortSharedInput::Cancelled((oid(*id), v.value())),
    }
}

#[allow(clippy::too_many_lines)]
fn run(seed: u64) {
    let mut rng = Rng(seed);
    let bump = Bump::with_capacity(1 << 16);
    let cancel_reason = if rng.below(2) == 0 {
        SequenceNumber::CONGESTED
    } else {
        SequenceNumber::RANDOMNESS_UNAVAILABLE
    };
    let n = rng.below(8);
    let mut reference = Vec::new();
    let mut port = containers::Vec::new_in(&bump);
    for _ in 0..n {
        let (r, p) = input(&mut rng, &bump, cancel_reason);
        reference.push(r);
        port.push(p);
    }
    // A cancellation needs a reason among the inputs, as consensus assigns them.
    let cancelled = |r: &ObjectReadResult| {
        matches!(
            r.object,
            ObjectReadResultKind::CancelledTransactionSharedObject(_)
        )
    };
    let has_reason = reference.iter().any(|r| {
        matches!(r.object, ObjectReadResultKind::CancelledTransactionSharedObject(v) if v == cancel_reason)
    });
    if reference.iter().any(cancelled) && !has_reason {
        return;
    }
    let receiving: Vec<ObjectRef> = (0..rng.below(3))
        .map(|_| {
            (
                rng.id(),
                SequenceNumber::from(1 + rng.below(2000)),
                rng.object_digest(),
            )
        })
        .collect();
    let port_receiving: Vec<_> = receiving.iter().map(port_ref).collect();
    let port_receiving = containers::alloc_slice_copy(&bump, &port_receiving);

    let reference = InputObjects::new(reference);
    let port = ExecutionInputs::new(&bump, port, port_receiving, None);

    assert_eq!(
        port.lamport_timestamp(),
        reference.lamport_timestamp(&receiving).value(),
        "seed {seed}"
    );

    let expected: Vec<_> = reference
        .exclusive_mutable_inputs()
        .into_iter()
        .map(|(id, ((v, d), o))| (oid(id), ((v.value(), digest(d)), owner(&bump, &o))))
        .collect();
    let actual: Vec<_> = port
        .exclusive_mutable_inputs()
        .iter()
        .map(|(id, entry)| (*id, *entry))
        .collect();
    assert_eq!(actual, expected, "seed {seed}");

    let expected: Vec<_> = reference
        .non_exclusive_input_objects()
        .into_iter()
        .map(|(id, o)| (oid(id), digest(o.digest())))
        .collect();
    let actual: Vec<_> = port
        .non_exclusive_input_objects()
        .map(|(id, o)| (*id, o.digest()))
        .collect();
    assert_eq!(actual, expected, "seed {seed}");

    let expected: Vec<_> = reference
        .consensus_stream_ended_objects()
        .into_iter()
        .map(|(id, v)| (oid(id), v.value()))
        .collect();
    let actual: Vec<_> = port
        .consensus_stream_ended_objects()
        .iter()
        .map(|(id, v)| (*id, *v))
        .collect();
    assert_eq!(actual, expected, "seed {seed}");

    let expected: Vec<_> = reference
        .filter_shared_objects()
        .iter()
        .map(shared_input)
        .collect();
    let actual: Vec<_> = port.filter_shared_objects(&bump).into_iter().collect();
    assert_eq!(actual, expected, "seed {seed}");

    let expected: Vec<_> = reference
        .transaction_dependencies()
        .into_iter()
        .map(digest)
        .collect();
    let actual: Vec<_> = port.transaction_dependencies(&bump).into_iter().collect();
    assert_eq!(actual, expected, "seed {seed}");

    let expected = reference
        .get_cancelled_objects()
        .map(|(ids, v)| (ids.into_iter().map(oid).collect::<Vec<_>>(), v.value()));
    let actual = port
        .get_cancelled_objects(&bump)
        .map(|(ids, v)| (ids.into_iter().collect::<Vec<_>>(), v));
    assert_eq!(actual, expected, "seed {seed}");

    let expected: Vec<_> = reference
        .into_object_map()
        .into_iter()
        .map(|(id, o): (_, Object)| (oid(id), digest(o.digest())))
        .collect();
    let actual: Vec<_> = port
        .objects()
        .iter()
        .map(|(id, o)| (*id, o.digest()))
        .collect();
    assert_eq!(actual, expected, "seed {seed}");
}

#[test]
fn inputs_match_reference() {
    assert_eq!(CONGESTED, SequenceNumber::CONGESTED.value());
    assert_eq!(
        RANDOMNESS_UNAVAILABLE,
        SequenceNumber::RANDOMNESS_UNAVAILABLE.value()
    );
    for seed in 1..=2000 {
        run(seed);
    }
}
