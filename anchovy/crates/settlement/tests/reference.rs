// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Settlement transactions and barriers against the reference's: the same
//! effects, settled by `settlement` and by a transcription of sui-core's
//! `AccumulatorSettlementTxBuilder` and `build_accumulator_barrier_tx` over
//! sui-types (sui-core itself is not linked), give the same `Transaction`
//! bytes and digests.

mod support;

use std::collections::{BTreeMap, HashMap};

use messages::Message;
use messages::effects::TransactionEffects as EffectsView;
use move_core_types::u256::U256;
use settlement::settle::{self, SettlementBuilder};
use sui_types::accumulator_event::AccumulatorEvent;
use sui_types::accumulator_root::{AccumulatorObjId, EventCommitment, build_event_merkle_root};
use sui_types::balance::{BALANCE_MODULE_NAME, BALANCE_STRUCT_NAME};
use sui_types::base_types::SequenceNumber;
use sui_types::effects::{
    AccumulatorAddress, AccumulatorOperation, AccumulatorValue, AccumulatorWriteV1, IDOperation,
    TransactionEffects, TransactionEffectsAPI,
};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{
    Argument, CallArg, ObjectArg, SharedObjectMutability, TransactionKind, VerifiedTransaction,
};
use sui_types::{
    Identifier, SUI_ACCUMULATOR_ROOT_OBJECT_ID, SUI_FRAMEWORK_ADDRESS, SUI_FRAMEWORK_PACKAGE_ID,
    TypeTag,
};

// The transcription: sui-core `accumulators/mod.rs`, unchanged in substance.

enum Merged {
    Sum(u128),
    Events(Vec<EventCommitment>),
}

struct Update {
    merge: Merged,
    split: Merged,
    input_sui: u64,
    output_sui: u64,
}

fn is_balance(ty: &TypeTag) -> bool {
    matches!(ty, TypeTag::Struct(s)
        if s.address == SUI_FRAMEWORK_ADDRESS
            && s.module.as_ident_str() == BALANCE_MODULE_NAME
            && s.name.as_ident_str() == BALANCE_STRUCT_NAME)
}

fn call(
    builder: &mut ProgrammableTransactionBuilder,
    module: &str,
    function: &str,
    ty: Vec<TypeTag>,
    args: Vec<Argument>,
) {
    builder.programmable_move_call(
        SUI_FRAMEWORK_PACKAGE_ID,
        Identifier::new(module).unwrap(),
        Identifier::new(function).unwrap(),
        ty,
        args,
    );
}

fn add_prologue(
    builder: &mut ProgrammableTransactionBuilder,
    root: Argument,
    epoch: u64,
    height: u64,
    idx: u64,
    input: u64,
    output: u64,
) {
    let args = vec![
        root,
        builder.pure(epoch).unwrap(),
        builder.pure(height).unwrap(),
        builder.pure(idx).unwrap(),
        builder.pure(input).unwrap(),
        builder.pure(output).unwrap(),
    ];
    call(
        builder,
        "accumulator_settlement",
        "settlement_prologue",
        vec![],
        args,
    );
}

#[allow(clippy::too_many_lines)]
fn reference_settlements(
    effects: &[TransactionEffects],
    max_updates: Option<u32>,
    epoch: u64,
    isv: u64,
    height: u64,
    seq: u64,
    offset: u64,
) -> Vec<TransactionKind> {
    let mut updates = BTreeMap::<AccumulatorObjId, Update>::new();
    let mut addresses = HashMap::<AccumulatorObjId, AccumulatorAddress>::new();
    for (tx_index, effect) in effects.iter().enumerate() {
        for event in effect.accumulator_events() {
            let (event_input_sui, event_output_sui) = event.total_sui_in_event();
            let AccumulatorEvent {
                accumulator_obj,
                write:
                    AccumulatorWriteV1 {
                        operation,
                        value,
                        address,
                    },
            } = event;
            addresses.insert(accumulator_obj, address);
            let entry = updates.entry(accumulator_obj).or_insert_with(|| {
                let zero = || match &value {
                    AccumulatorValue::Integer(_) => Merged::Sum(0),
                    AccumulatorValue::EventDigest(_) => Merged::Events(vec![]),
                    AccumulatorValue::IntegerTuple(..) => unreachable!(),
                };
                Update {
                    merge: zero(),
                    split: zero(),
                    input_sui: 0,
                    output_sui: 0,
                }
            });
            entry.input_sui += event_output_sui;
            entry.output_sui += event_input_sui;
            let target = match operation {
                AccumulatorOperation::Merge => &mut entry.merge,
                AccumulatorOperation::Split => &mut entry.split,
            };
            match (target, value) {
                (Merged::Sum(v), AccumulatorValue::Integer(x)) => *v += u128::from(x),
                (Merged::Events(c), AccumulatorValue::EventDigest(digests)) => {
                    for (event_idx, digest) in digests {
                        c.push(EventCommitment::new(
                            seq,
                            tx_index as u64 + offset,
                            event_idx,
                            digest,
                        ));
                    }
                }
                _ => unreachable!(),
            }
        }
    }

    let chunk_size = max_updates.unwrap_or(u32::MAX) as usize;
    let updates: Vec<_> = updates.into_iter().collect();
    updates
        .chunks(chunk_size)
        .enumerate()
        .map(|(idx, chunk)| {
            let (input, output) = chunk
                .iter()
                .fold((0, 0), |(i, o), (_, u)| (i + u.input_sui, o + u.output_sui));
            let mut builder = ProgrammableTransactionBuilder::new();
            let root = builder
                .input(CallArg::Object(ObjectArg::SharedObject {
                    id: SUI_ACCUMULATOR_ROOT_OBJECT_ID,
                    initial_shared_version: SequenceNumber::from_u64(isv),
                    mutability: SharedObjectMutability::NonExclusiveWrite,
                }))
                .unwrap();
            add_prologue(&mut builder, root, epoch, height, idx as u64, input, output);
            for (obj, update) in chunk {
                let address = addresses.get(obj).unwrap();
                let address_arg = builder.pure(address.address).unwrap();
                match (&update.merge, &update.split) {
                    (Merged::Sum(merge), Merged::Sum(split)) if is_balance(&address.ty) => {
                        let (merge, split) = if merge >= split {
                            (merge - split, 0)
                        } else {
                            (0, split - merge)
                        };
                        if merge != 0 || split != 0 {
                            let args = vec![
                                root,
                                address_arg,
                                builder.pure(merge).unwrap(),
                                builder.pure(split).unwrap(),
                            ];
                            call(
                                &mut builder,
                                "accumulator_settlement",
                                "settle_u128",
                                vec![address.ty.clone()],
                                args,
                            );
                        }
                    }
                    (Merged::Events(events), Merged::Events(_)) => {
                        let digest = build_event_merkle_root(events);
                        let args = vec![
                            root,
                            builder.pure(address.address).unwrap(),
                            builder
                                .pure(U256::from_le_bytes(&digest.into_inner()))
                                .unwrap(),
                            builder.pure(events.len() as u64).unwrap(),
                            builder.pure(seq).unwrap(),
                        ];
                        call(
                            &mut builder,
                            "accumulator_settlement",
                            "settle_events",
                            vec![],
                            args,
                        );
                    }
                    _ => unreachable!(),
                }
            }
            TransactionKind::ProgrammableSystemTransaction(builder.finish())
        })
        .collect()
}

fn reference_barrier(
    epoch: u64,
    isv: u64,
    height: u64,
    settlement_effects: &[TransactionEffects],
) -> TransactionKind {
    let (created, destroyed) = settlement_effects
        .iter()
        .flat_map(TransactionEffectsAPI::object_changes)
        .fold((0u64, 0u64), |(c, d), change| match change.id_operation {
            IDOperation::Created => (c + 1, d),
            IDOperation::Deleted => (c, d + 1),
            IDOperation::None => (c, d),
        });
    let mut builder = ProgrammableTransactionBuilder::new();
    let root = builder
        .input(CallArg::Object(ObjectArg::SharedObject {
            id: SUI_ACCUMULATOR_ROOT_OBJECT_ID,
            initial_shared_version: SequenceNumber::from_u64(isv),
            mutability: SharedObjectMutability::Mutable,
        }))
        .unwrap();
    add_prologue(
        &mut builder,
        root,
        epoch,
        height,
        settlement_effects.len() as u64,
        0,
        0,
    );
    let args = vec![
        root,
        builder.pure(created).unwrap(),
        builder.pure(destroyed).unwrap(),
    ];
    call(
        &mut builder,
        "accumulator_metadata",
        "record_accumulator_object_changes",
        vec![],
        args,
    );
    TransactionKind::ProgrammableSystemTransaction(builder.finish())
}

// The comparison.

fn reference_transaction(kind: TransactionKind) -> settle::Built {
    let tx = VerifiedTransaction::new_system_transaction(kind);
    settle::Built {
        bytes: bcs::to_bytes(tx.inner()).unwrap(),
        digest: tx.digest().into_inner(),
    }
}

fn ours(
    effects: &[Vec<u8>],
    max_updates: Option<u32>,
    (epoch, isv, height, seq, offset): (u64, u64, u64, u64, u64),
) -> Vec<settle::Built> {
    let parsed: Vec<Message<EffectsView<'static>>> = effects
        .iter()
        .map(|b| Message::parse(b.clone()).map_err(|(e, _)| e).unwrap())
        .collect();
    let mut builder = SettlementBuilder::new(seq, offset);
    for (i, e) in parsed.iter().enumerate() {
        builder.add(i as u64, e.get());
    }
    builder
        .build(max_updates, epoch, isv, height)
        .into_iter()
        .map(settle::system_transaction)
        .collect()
}

fn theirs(
    effects: &[Vec<u8>],
    max_updates: Option<u32>,
    (epoch, isv, height, seq, offset): (u64, u64, u64, u64, u64),
) -> Vec<settle::Built> {
    let effects: Vec<TransactionEffects> = effects
        .iter()
        .map(|b| bcs::from_bytes(b).unwrap())
        .collect();
    reference_settlements(&effects, max_updates, epoch, isv, height, seq, offset)
        .into_iter()
        .map(reference_transaction)
        .collect()
}

const PARAMS: (u64, u64, u64, u64, u64) = (7, 1234, 11, 42, 3);

#[test]
fn fixed_chunk_matches_the_reference() {
    let chunk = support::fixed_chunk();
    for max_updates in [None, Some(100), Some(2), Some(1)] {
        let ours = ours(&chunk, max_updates, PARAMS);
        let theirs = theirs(&chunk, max_updates, PARAMS);
        assert!(!ours.is_empty());
        assert_eq!(ours, theirs, "max_updates {max_updates:?}");
    }
}

#[test]
fn event_indices_follow_the_offset_and_checkpoint() {
    let chunk = support::fixed_chunk();
    let (epoch, isv, height, _, _) = PARAMS;
    let a = ours(&chunk, None, (epoch, isv, height, 42, 0));
    let b = ours(&chunk, None, (epoch, isv, height, 42, 5));
    let c = ours(&chunk, None, (epoch, isv, height, 43, 0));
    assert_ne!(a, b);
    assert_ne!(a, c);
    assert_eq!(b, theirs(&chunk, None, (epoch, isv, height, 42, 5)));
    assert_eq!(c, theirs(&chunk, None, (epoch, isv, height, 43, 0)));
}

#[test]
fn a_chunk_without_events_has_no_settlements() {
    let chunk = vec![support::effects(support::digest(1), 1, &[], vec![])];
    assert!(ours(&chunk, None, PARAMS).is_empty());
    assert!(theirs(&chunk, None, PARAMS).is_empty());
}

#[test]
fn many_accounts_match_the_reference() {
    let chunk = support::transfers(300, 250);
    for max_updates in [None, Some(100)] {
        assert_eq!(
            ours(&chunk, max_updates, PARAMS),
            theirs(&chunk, max_updates, PARAMS)
        );
    }
}

#[test]
fn barrier_matches_the_reference() {
    use support::{created_or_deleted, effects, id, merge, sui_balance};
    let settlements = [
        effects(
            support::digest(90),
            9,
            &[],
            vec![
                created_or_deleted(id(0x2001), true),
                created_or_deleted(id(0x2002), true),
                merge(id(0xa1), sui_balance(), 1),
            ],
        ),
        effects(
            support::digest(91),
            9,
            &[],
            vec![created_or_deleted(id(0x2004), false)],
        ),
    ];
    let views: Vec<Message<EffectsView<'static>>> = settlements
        .iter()
        .map(|b| Message::parse(b.clone()).map_err(|(e, _)| e).unwrap())
        .collect();
    let (epoch, isv, height, _, _) = PARAMS;
    for n in [0, 1, 2] {
        let counts = settle::count_object_changes(views[..n].iter().map(Message::get));
        let ours =
            settle::system_transaction(settle::barrier(epoch, isv, height, n as u64, counts));
        let reference: Vec<TransactionEffects> = settlements[..n]
            .iter()
            .map(|b| bcs::from_bytes(b).unwrap())
            .collect();
        let theirs = reference_transaction(reference_barrier(epoch, isv, height, &reference));
        assert_eq!(ours, theirs, "{n} settlements");
    }
}
