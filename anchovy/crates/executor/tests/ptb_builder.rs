// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The arena PTB builder against sui-types': the same calls give the same inputs and commands.

use containers::Bump;
use executor::ptb_builder::{CLOCK_MUT, ProgrammableTransactionBuilder, SUI_SYSTEM_MUT};
use messages::arena::BumpAlloc;
use messages::base::ObjectId;
use messages::reader::Reader;
use messages::transaction::{CallArg, Command, ObjectArg, SharedObjectMutability};
use move_core_types::identifier::Identifier;
use sui_types::base_types::{ObjectID, SequenceNumber};
use sui_types::transaction as reference;

fn parse<'a, T>(
    bump: &'a Bump,
    value: &impl serde::Serialize,
    parse: impl FnOnce(&mut Reader<'a>, &mut BumpAlloc<'a>) -> messages::Result<T>,
) -> T {
    let bytes = containers::alloc_slice_copy(bump, &bcs::to_bytes(value).unwrap());
    parse(&mut Reader::new(bytes), &mut BumpAlloc(bump)).unwrap()
}

// The lengths are checked equal before each zip.
#[allow(clippy::disallowed_methods)]
fn assert_same(
    bump: &Bump,
    port: messages::transaction::ProgrammableTransaction<'_>,
    reference: &reference::ProgrammableTransaction,
) {
    assert_eq!(port.inputs.len(), reference.inputs.len());
    for (p, r) in port.inputs.iter().zip(&reference.inputs) {
        assert_eq!(*p, parse(bump, r, CallArg::parse));
    }
    assert_eq!(port.commands.len(), reference.commands.len());
    for (p, r) in port.commands.iter().zip(&reference.commands) {
        assert_eq!(*p, parse(bump, r, Command::parse));
    }
}

fn ident(s: &str) -> Identifier {
    Identifier::new(s).unwrap()
}

#[test]
fn builder_matches_reference() {
    let bump = Bump::with_capacity(1 << 16);
    let sui = executor::accumulator_root::sui_type(&bump);
    let gas_type = containers::alloc_slice_copy(&bump, &[sui]);

    let mut port = ProgrammableTransactionBuilder::new(&bump);
    let mut refb =
        sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder::new();

    // A move_call with object and pure inputs.
    let ts = port.bcs(&1234u64);
    port.move_call(
        ObjectId::from_u16(2),
        "clock",
        "consensus_commit_prologue",
        &[],
        &[
            CallArg::Object(ObjectArg::SharedObject(&CLOCK_MUT)),
            CallArg::Pure(ts),
        ],
    )
    .unwrap();
    refb.move_call(
        ObjectID::from_single_byte(2),
        ident("clock"),
        ident("consensus_commit_prologue"),
        vec![],
        vec![
            reference::CallArg::CLOCK_MUT,
            reference::CallArg::Pure(bcs::to_bytes(&1234u64).unwrap()),
        ],
    )
    .unwrap();

    // Pure inputs dedup; a shared object input becomes mutable if either use is.
    let a = port.pure(&1234u64);
    let b = refb.pure(1234u64).unwrap();
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
    let p_obj = port
        .shared_obj(
            ObjectId::from_u16(0x5),
            1,
            SharedObjectMutability::Immutable,
        )
        .unwrap();
    let r_obj = refb
        .obj(reference::ObjectArg::SharedObject {
            id: ObjectID::from_single_byte(5),
            initial_shared_version: SequenceNumber::from(1),
            mutability: reference::SharedObjectMutability::Immutable,
        })
        .unwrap();
    assert_eq!(format!("{p_obj:?}"), format!("{r_obj:?}"));
    port.obj(ObjectArg::SharedObject(&SUI_SYSTEM_MUT)).unwrap();
    refb.obj(reference::ObjectArg::SUI_SYSTEM_MUT).unwrap();

    // A programmable call with type arguments, on a result.
    let r1 = port.programmable_move_call(
        ObjectId::from_u16(2),
        "balance",
        "create_staking_rewards",
        gas_type,
        &[a],
    );
    let r2 = refb.programmable_move_call(
        ObjectID::from_single_byte(2),
        ident("balance"),
        ident("create_staking_rewards"),
        vec![sui_types::gas_coin::GAS::type_tag()],
        vec![b],
    );
    assert_eq!(format!("{r1:?}"), format!("{r2:?}"));
    port.programmable_move_call(
        ObjectId::from_u16(3),
        "sui_system",
        "advance_epoch",
        &[],
        &[r1, p_obj],
    );
    refb.programmable_move_call(
        ObjectID::from_single_byte(3),
        ident("sui_system"),
        ident("advance_epoch"),
        vec![],
        vec![r2, r_obj],
    );

    assert_same(&bump, port.finish(), &refb.finish());
}
