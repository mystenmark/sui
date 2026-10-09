// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Effects construction against sui-types': the same object changes,
//! consensus inputs, accumulator writes, events and dependencies through
//! both, comparing effects and events bytes and digests.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

mod common;

use common::*;
use containers::Bump;
use executor::effects::{
    self as port, EffectsObjectChange as PortChange, SharedInput as PortSharedInput,
};
use messages::arena::BumpAlloc;
use move_core_types::account_address::AccountAddress;
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::{StructTag, TypeTag};
use sui_types::base_types::{ObjectID, SequenceNumber, TransactionDigest};
use sui_types::digests::ObjectDigest;
use sui_types::effects::{
    AccumulatorAddress, AccumulatorOperation, AccumulatorValue, AccumulatorWriteV1,
    EffectsObjectChange, TransactionEffects, TransactionEffectsV2, UnchangedConsensusKind,
};
use sui_types::event::Event;
use sui_types::execution::SharedInput;
use sui_types::execution_status::{ExecutionErrorKind, ExecutionFailure, ExecutionStatus};
use sui_types::gas::GasCostSummary;
use sui_types::message_envelope::Message as _;
use sui_types::object::{Object, Owner};
use sui_types::transaction::SharedObjectMutability;

/// One object change, as the temporary store hands it to both constructors.
struct ChangeInput {
    id: ObjectID,
    modified_at: Option<((SequenceNumber, ObjectDigest), Owner)>,
    written: Option<Object>,
    created: bool,
    deleted: bool,
}

#[derive(Default)]
struct Coverage {
    kinds: BTreeSet<&'static str>,
}

/// The accumulator writes of one accumulator object, before merging.
fn accumulator_writes(rng: &mut Rng, kind: u64) -> Vec<AccumulatorWriteV1> {
    let address = AccumulatorAddress::new(
        rng.address(),
        TypeTag::from_str(if kind == 1 {
            "0x2::accumulator_settlement::EventStreamHead"
        } else {
            "0x2::balance::Balance<0x2::sui::SUI>"
        })
        .unwrap(),
    );
    let n = match kind {
        2 => 1,
        _ => 1 + rng.below(4) as usize,
    };
    let mut event_index = 0u64;
    (0..n)
        .map(|_| {
            let (operation, value) = match kind {
                0 => (
                    if rng.below(2) == 0 {
                        AccumulatorOperation::Merge
                    } else {
                        AccumulatorOperation::Split
                    },
                    AccumulatorValue::Integer(rng.below(1 << 50)),
                ),
                1 => {
                    let entries: Vec<(u64, sui_types::digests::Digest)> = (0..=rng.below(3))
                        .map(|_| {
                            event_index += 1 + rng.below(3);
                            (event_index, sui_types::digests::Digest::new(rng.bytes()))
                        })
                        .collect();
                    // `NonEmpty` deserializes from a non-empty sequence.
                    let mut b = vec![2u8];
                    b.extend(bcs::to_bytes(&entries).unwrap());
                    (AccumulatorOperation::Merge, bcs::from_bytes(&b).unwrap())
                }
                _ => (
                    AccumulatorOperation::Merge,
                    AccumulatorValue::IntegerTuple(rng.next(), rng.next()),
                ),
            };
            AccumulatorWriteV1 {
                address: address.clone(),
                operation,
                value,
            }
        })
        .collect()
}

#[allow(clippy::too_many_lines)]
fn run(seed: u64, coverage: &mut Coverage) {
    let mut rng = Rng(seed);
    let bump = Bump::with_capacity(1 << 16);
    let bump = &bump;

    let lamport = 1_000 + rng.below(1_000);
    let mut inputs: Vec<ChangeInput> = Vec::new();

    // The gas coin: absent, mutated, or deleted.
    let gas_mode = rng.below(3);
    let mut gas_object = None;
    if gas_mode != 0 {
        let id = rng.id();
        let old_owner = Owner::AddressOwner(rng.address());
        let modified_at = Some((
            (
                SequenceNumber::from(rng.below(lamport)),
                rng.object_digest(),
            ),
            old_owner,
        ));
        let new_owner = Owner::AddressOwner(rng.address());
        let written = (gas_mode == 1).then(|| coin(&mut rng, id, lamport, new_owner));
        coverage.kinds.insert(if gas_mode == 1 {
            "gas mutated"
        } else {
            "gas deleted"
        });
        inputs.push(ChangeInput {
            id,
            modified_at,
            written,
            created: false,
            deleted: gas_mode == 2,
        });
        gas_object = Some(id);
    }

    for _ in 0..rng.below(12) {
        let id = rng.id();
        let old_version = SequenceNumber::from(rng.below(lamport));
        let old_digest = rng.object_digest();
        let input = match rng.below(9) {
            0 => {
                coverage.kinds.insert("created");
                let owner = if rng.below(3) == 0 {
                    rng.shared_owner()
                } else {
                    rng.unshared_owner(true)
                };
                ChangeInput {
                    id,
                    modified_at: None,
                    written: Some(coin(&mut rng, id, lamport, owner)),
                    created: true,
                    deleted: false,
                }
            }
            1 => {
                let shared = rng.below(3) == 0;
                let (old, new) = if shared {
                    coverage.kinds.insert("mutated shared");
                    let o = rng.shared_owner();
                    (o.clone(), o)
                } else {
                    coverage.kinds.insert("mutated");
                    (rng.unshared_owner(false), rng.unshared_owner(true))
                };
                ChangeInput {
                    id,
                    modified_at: Some(((old_version, old_digest), old)),
                    written: Some(coin(&mut rng, id, lamport, new)),
                    created: false,
                    deleted: false,
                }
            }
            2 => {
                coverage.kinds.insert("deleted");
                let old = if rng.below(3) == 0 {
                    rng.shared_owner()
                } else {
                    rng.unshared_owner(false)
                };
                ChangeInput {
                    id,
                    modified_at: Some(((old_version, old_digest), old)),
                    written: None,
                    created: false,
                    deleted: true,
                }
            }
            3 => {
                coverage.kinds.insert("wrapped");
                ChangeInput {
                    id,
                    modified_at: Some(((old_version, old_digest), rng.unshared_owner(false))),
                    written: None,
                    created: false,
                    deleted: false,
                }
            }
            4 => {
                coverage.kinds.insert("unwrapped");
                let owner = rng.unshared_owner(true);
                ChangeInput {
                    id,
                    modified_at: None,
                    written: Some(coin(&mut rng, id, lamport, owner)),
                    created: false,
                    deleted: false,
                }
            }
            5 => {
                coverage.kinds.insert("unwrapped then deleted");
                ChangeInput {
                    id,
                    modified_at: None,
                    written: None,
                    created: false,
                    deleted: true,
                }
            }
            6 => {
                coverage.kinds.insert("created then wrapped");
                ChangeInput {
                    id,
                    modified_at: None,
                    written: None,
                    created: true,
                    deleted: false,
                }
            }
            7 => {
                coverage.kinds.insert("package published");
                ChangeInput {
                    id,
                    modified_at: None,
                    written: {
                        let version = 1 + rng.below(5);
                        Some(package(&mut rng, id, version))
                    },
                    created: true,
                    deleted: false,
                }
            }
            _ => {
                if inputs.iter().any(|i| i.id == ObjectID::from_single_byte(2)) {
                    continue;
                }
                coverage.kinds.insert("system package upgrade");
                let id = ObjectID::from_single_byte(2);
                ChangeInput {
                    id,
                    modified_at: Some(((old_version, old_digest), Owner::Immutable)),
                    written: Some(package(&mut rng, id, old_version.value() + 1)),
                    created: false,
                    deleted: false,
                }
            }
        };
        inputs.push(input);
    }

    // Both sides' changed objects.
    let mut sui_changes: BTreeMap<ObjectID, EffectsObjectChange> = BTreeMap::new();
    let mut port_changes = containers::BTreeMap::new_in(bump);
    for input in &inputs {
        sui_changes.insert(
            input.id,
            EffectsObjectChange::new(
                input.modified_at.clone(),
                input.written.as_ref(),
                input.created,
                input.deleted,
            ),
        );
        let written = input.written.as_ref().map(|o| {
            if rng.below(2) == 0 {
                stored_object(bump, o)
            } else {
                sealed_object(bump, o)
            }
        });
        port_changes.insert(
            oid(input.id),
            PortChange::new(
                bump,
                input
                    .modified_at
                    .as_ref()
                    .map(|((v, d), o)| ((v.value(), digest(*d)), owner(bump, o))),
                written.as_ref(),
                input.created,
                input.deleted,
            ),
        );
    }

    // Accumulator writes, merged per accumulator object.
    for _ in 0..rng.below(4) {
        let kind = rng.below(3);
        coverage.kinds.insert(match kind {
            0 => "accumulator integer",
            1 => "accumulator event digest",
            _ => "accumulator integer tuple",
        });
        let writes = accumulator_writes(&mut rng, kind);
        if writes.len() > 1 {
            coverage.kinds.insert("accumulator merge");
        }
        let port_writes: Vec<_> = writes.iter().map(|w| accumulator_write(bump, w)).collect();
        let merged = AccumulatorWriteV1::merge(writes);
        let port_merged = port::merge_accumulator_writes(bump, &port_writes);
        assert_eq!(port_merged, accumulator_write(bump, &merged), "seed {seed}");

        let id = rng.id();
        sui_changes.insert(id, EffectsObjectChange::new_from_accumulator_write(merged));
        port_changes.insert(
            oid(id),
            PortChange::new_from_accumulator_write(bump, port_merged),
        );
    }

    // Consensus inputs: read-only, ended streams and cancelled ones, and a mutated one
    // that the changed objects already record.
    let mut sui_shared = Vec::new();
    let mut port_shared = Vec::new();
    let mut sui_system = BTreeMap::new();
    let mut port_system = containers::BTreeMap::new_in(bump);
    for input in &inputs {
        if let Some(((v, d), Owner::Shared { .. })) = &input.modified_at {
            sui_shared.push(SharedInput::Existing((input.id, *v, *d)));
            port_shared.push(PortSharedInput::Existing((
                oid(input.id),
                v.value(),
                digest(*d),
            )));
            if rng.below(2) == 0 {
                // A system object read at the version its changed entry records.
                sui_system.insert(input.id, (*v, *d));
                port_system.insert(oid(input.id), (v.value(), digest(*d)));
            }
        }
    }
    for _ in 0..rng.below(6) {
        let id = rng.id();
        let version = SequenceNumber::from(rng.below(lamport));
        match rng.below(5) {
            0 => {
                coverage.kinds.insert("read-only root");
                let d = rng.object_digest();
                sui_shared.push(SharedInput::Existing((id, version, d)));
                port_shared.push(PortSharedInput::Existing((
                    oid(id),
                    version.value(),
                    digest(d),
                )));
                if rng.below(2) == 0 {
                    coverage.kinds.insert("system object already read-only");
                    sui_system.insert(id, (version, d));
                    port_system.insert(oid(id), (version.value(), digest(d)));
                }
            }
            1..=3 => {
                let (sui_m, port_m, name) = match rng.below(3) {
                    0 => (
                        SharedObjectMutability::Mutable,
                        messages::transaction::SharedObjectMutability::Mutable,
                        "mutate stream ended",
                    ),
                    1 => (
                        SharedObjectMutability::Immutable,
                        messages::transaction::SharedObjectMutability::Immutable,
                        "read stream ended",
                    ),
                    _ => (
                        SharedObjectMutability::NonExclusiveWrite,
                        messages::transaction::SharedObjectMutability::NonExclusiveWrite,
                        "non-exclusive stream ended",
                    ),
                };
                coverage.kinds.insert(name);
                let prev = rng.tx_digest();
                sui_shared.push(SharedInput::ConsensusStreamEnded((
                    id, version, sui_m, prev,
                )));
                port_shared.push(PortSharedInput::ConsensusStreamEnded((
                    oid(id),
                    version.value(),
                    port_m,
                    digest(prev),
                )));
            }
            _ => {
                coverage.kinds.insert("cancelled");
                sui_shared.push(SharedInput::Cancelled((id, version)));
                port_shared.push(PortSharedInput::Cancelled((oid(id), version.value())));
            }
        }
    }
    let mut sui_epoch_config = BTreeSet::new();
    let mut port_epoch_config = containers::BTreeSet::new_in(bump);
    for _ in 0..rng.below(3) {
        coverage.kinds.insert("per-epoch config");
        let id = rng.id();
        sui_epoch_config.insert(id);
        port_epoch_config.insert(oid(id));
    }
    for _ in 0..rng.below(3) {
        coverage.kinds.insert("system object read");
        let (id, v, d) = (
            rng.id(),
            SequenceNumber::from(rng.below(lamport)),
            rng.object_digest(),
        );
        sui_system.insert(id, (v, d));
        port_system.insert(oid(id), (v.value(), digest(d)));
    }

    let sui_unchanged = TransactionEffectsV2::compute_unchanged_consensus_objects(
        sui_shared,
        sui_epoch_config,
        &sui_changes,
        sui_system,
    );
    // The port takes the changes as a map built once, from the same entries in the same order.
    let port_changes = {
        let mut entries = containers::Vec::new_in(bump);
        entries.extend(port_changes);
        containers::VecMap::from_entries(entries)
    };
    let port_unchanged = port::compute_unchanged_consensus_objects(
        bump,
        &port_shared,
        &port_epoch_config,
        &port_changes,
        &port_system,
    );
    assert_eq!(port_unchanged.len(), sui_unchanged.len(), "seed {seed}");
    for (i, (pid, pk)) in port_unchanged.iter().enumerate() {
        let (sid, sk) = &sui_unchanged[i];
        assert_eq!(*pid, oid(*sid), "seed {seed}");
        let expected = match sk {
            UnchangedConsensusKind::ReadOnlyRoot((v, d)) => {
                messages::effects::UnchangedConsensusKind::ReadOnlyRoot(
                    v.value(),
                    containers::alloc(bump, digest(*d)),
                )
            }
            UnchangedConsensusKind::MutateConsensusStreamEnded(v) => {
                messages::effects::UnchangedConsensusKind::MutateConsensusStreamEnded(v.value())
            }
            UnchangedConsensusKind::ReadConsensusStreamEnded(v) => {
                messages::effects::UnchangedConsensusKind::ReadConsensusStreamEnded(v.value())
            }
            UnchangedConsensusKind::Cancelled(v) => {
                messages::effects::UnchangedConsensusKind::Cancelled(v.value())
            }
            UnchangedConsensusKind::PerEpochConfig => {
                messages::effects::UnchangedConsensusKind::PerEpochConfig
            }
        };
        assert_eq!(*pk, expected, "seed {seed}");
    }

    // Events.
    let events: Vec<Event> = (0..rng.below(4))
        .map(|i| {
            let package = AccountAddress::new(rng.bytes());
            let ty = StructTag::from_str(&format!(
                "{}::m{i}::E<u64, 0x2::sui::SUI>",
                package.to_hex_literal()
            ))
            .unwrap();
            Event::new(
                &package,
                Identifier::new(format!("module{i}"))
                    .unwrap()
                    .as_ident_str(),
                rng.address(),
                ty,
                (0..rng.below(40)).map(|_| rng.next() as u8).collect(),
            )
        })
        .collect();
    coverage.kinds.insert(if events.is_empty() {
        "no events"
    } else {
        "events"
    });
    let sui_events = sui_types::effects::TransactionEvents { data: events };
    let port_events = messages::effects::TransactionEvents::parse(
        &mut reader(bump, &sui_events),
        &mut BumpAlloc(bump),
    )
    .unwrap();
    let built_events = port::build_events(bump, port_events.data);
    assert_eq!(built_events.bytes, &bcs::to_bytes(&sui_events).unwrap()[..]);
    assert_eq!(built_events.digest, digest(sui_events.digest()));
    let (sui_events_digest, port_events_digest) = if sui_events.data.is_empty() {
        (None, None)
    } else {
        (Some(sui_events.digest()), Some(built_events.digest))
    };

    // Dependencies, with duplicates, as the store collects them in a set.
    let mut port_deps = Vec::new();
    for _ in 0..rng.below(6) {
        let d = rng.tx_digest();
        port_deps.push(digest(d));
        if rng.below(2) == 0 {
            coverage.kinds.insert("duplicate dependency");
            port_deps.push(digest(d));
        }
    }
    let sui_deps: BTreeSet<TransactionDigest> = port_deps
        .iter()
        .map(|d| TransactionDigest::new(d.bytes))
        .collect();

    let sui_status = if rng.below(2) == 0 {
        ExecutionStatus::Success
    } else {
        ExecutionStatus::new_failure(ExecutionFailure {
            error: ExecutionErrorKind::InsufficientGas,
            command: (rng.below(2) == 0).then(|| rng.below(10) as usize),
        })
    };
    let gas_used = GasCostSummary {
        computation_cost: rng.next(),
        storage_cost: rng.next(),
        storage_rebate: rng.next(),
        non_refundable_storage_fee: rng.next(),
    };
    let epoch = rng.below(1_000);
    let tx_digest = rng.tx_digest();

    let sui_effects = TransactionEffects::new_from_execution_v2(
        sui_status.clone(),
        epoch,
        gas_used.clone(),
        sui_unchanged,
        tx_digest,
        SequenceNumber::from(lamport),
        sui_changes,
        gas_object,
        sui_events_digest,
        sui_deps.into_iter().collect(),
    );
    let built = port::new_from_execution_v2(
        bump,
        status(bump, &sui_status),
        epoch,
        messages::effects::GasCostSummary {
            computation_cost: gas_used.computation_cost,
            storage_cost: gas_used.storage_cost,
            storage_rebate: gas_used.storage_rebate,
            non_refundable_storage_fee: gas_used.non_refundable_storage_fee,
        },
        port_unchanged,
        digest(tx_digest),
        lamport,
        port_changes,
        gas_object.map(oid),
        port_events_digest,
        port_deps,
    );
    assert_eq!(
        built.bytes,
        &bcs::to_bytes(&sui_effects).unwrap()[..],
        "seed {seed}"
    );
    assert_eq!(built.digest, digest(sui_effects.digest()), "seed {seed}");
}

#[test]
fn effects_match_reference() {
    let mut coverage = Coverage::default();
    for seed in 1..=500u64 {
        run(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1, &mut coverage);
    }
    for kind in [
        "gas mutated",
        "gas deleted",
        "created",
        "mutated",
        "mutated shared",
        "deleted",
        "wrapped",
        "unwrapped",
        "unwrapped then deleted",
        "created then wrapped",
        "package published",
        "system package upgrade",
        "accumulator integer",
        "accumulator event digest",
        "accumulator integer tuple",
        "accumulator merge",
        "read-only root",
        "system object already read-only",
        "mutate stream ended",
        "read stream ended",
        "non-exclusive stream ended",
        "cancelled",
        "per-epoch config",
        "system object read",
        "events",
        "no events",
        "duplicate dependency",
    ] {
        assert!(coverage.kinds.contains(kind), "never covered: {kind}");
    }
}

#[test]
fn size_estimate_matches_reference() {
    for (w, m, d) in [(0, 0, 0), (1, 2, 3), (100, 50, 7), (2048, 1024, 1000)] {
        assert_eq!(
            port::estimate_effects_size_upperbound_v2(w, m, d),
            TransactionEffects::estimate_effects_size_upperbound_v2(w, m, d)
        );
    }
}

#[test]
#[should_panic(expected = "All writes must have the same accumulator address")]
fn merge_rejects_mismatched_addresses() {
    let mut rng = Rng(11);
    let bump = Bump::with_capacity(1 << 16);
    let mut writes = accumulator_writes(&mut rng, 0);
    while writes.len() < 2 {
        writes = accumulator_writes(&mut rng, 0);
    }
    writes[1].address.address = rng.address();
    let port_writes: Vec<_> = writes.iter().map(|w| accumulator_write(&bump, w)).collect();
    port::merge_accumulator_writes(&bump, &port_writes);
}

#[test]
#[should_panic(expected = "an object's digest is taken once it is sealed")]
fn written_object_must_be_sealed() {
    let mut rng = Rng(13);
    let bump = Bump::with_capacity(1 << 16);
    let id = rng.id();
    let owner = Owner::AddressOwner(rng.address());
    let o = coin(&mut rng, id, 3, owner);
    let changed = stored_object(&bump, &o).with_storage_rebate(1);
    PortChange::new(&bump, None, Some(&changed), true, false);
}
