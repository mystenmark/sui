// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use messages::Message;
use messages::base::{Digest, ObjectId};
use messages::transaction::SharedObjectMutability;
use protocol_config::{Chain, ProtocolVersion};
use sui_types::Identifier;
use sui_types::base_types::{ObjectID, ObjectRef, SequenceNumber, SuiAddress, random_object_ref};
use sui_types::coin_reservation::ParsedObjectRefWithdrawal;
use sui_types::crypto::{AccountKeyPair, get_key_pair};
use sui_types::digests::{ChainIdentifier, CheckpointDigest};
use sui_types::gas_coin::GAS;
use sui_types::object::{Object, Owner};
use sui_types::transaction::{
    CallArg, Command as SuiCommand, FundsWithdrawalArg, GasData, ObjectArg,
    ProgrammableTransaction, Transaction, TransactionData, TransactionDataAPI, TransactionKind,
};
use sui_types::type_input::TypeInput;
use validator::checks;
use validator::consensus::commit::CommitHandler;
use validator::consensus::sequence::{SharedAccess, SharedOrdinal};
use validator::epoch::EpochParams;
use validator::epoch_store::EpochStore;
use validator::pipeline::items::{
    Chunk, ExecutableTransaction, FundsCommand, InputCommand, InputValue, ScheduledEntry,
    SystemKind, SystemTransaction,
};
use validator::pipeline::keys::{AccumulatorVersion, InputKey, Reservation, StreamId};
use validator::pipeline::tables::{Generation, GenerationTable};
use waiter::{Availability, Command, Waiter};
use workqueue::{Inbox, Processor};

use crate::{InputAvailability, VersionResolver};

use SharedObjectMutability::{Immutable, Mutable, NonExclusiveWrite};

/// The chain identifier, which masks coin reservations' account ids.
const CHAIN: [u8; 32] = [0x5a; 32];

/// An empty generation table, over an epoch store in a directory kept alive
/// with it.
fn generation_table() -> (Arc<GenerationTable>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let (store, _, _) = validator::epoch_store::EpochStore::open(dir.path(), 0);
    (Arc::new(GenerationTable::new(store)), dir)
}

fn epoch() -> Arc<EpochParams> {
    Arc::new(EpochParams::new(
        Chain::Unknown,
        ProtocolVersion::MAX.as_u64(),
        0,
        Digest::new(CHAIN),
        1000,
        4,
    ))
}

type FundsInbox = Inbox<FundsCommand>;

fn resolver() -> (VersionResolver, Inbox<InputCommand>) {
    let (resolver, inbox, _) = resolver_with_funds();
    (resolver, inbox)
}

fn resolver_with_funds() -> (VersionResolver, Inbox<InputCommand>, FundsInbox) {
    let (inputs, inbox) = workqueue::queue(16);
    let (withdrawals, funds) = workqueue::queue(16);
    (
        VersionResolver::new(epoch(), inputs, withdrawals),
        inbox,
        funds,
    )
}

fn stream(n: u8) -> StreamId {
    StreamId {
        id: ObjectId([n; 32]),
        initial_shared_version: 1,
    }
}

fn access(
    stream: StreamId,
    mutability: SharedObjectMutability,
    round: u32,
    writes_before: u32,
) -> SharedAccess {
    SharedAccess {
        id: stream.id,
        initial_shared_version: stream.initial_shared_version,
        mutability,
        ordinal: SharedOrdinal {
            round,
            writes_before,
        },
        base: None,
    }
}

/// An entry with only shared inputs.
fn system(shared: Vec<SharedAccess>) -> ScheduledEntry {
    ScheduledEntry {
        transaction: ExecutableTransaction::System(SystemTransaction {
            kind: SystemKind::Prologue { round: 0 },
            bytes: vec![],
            digest: [0; 32],
        }),
        shared,
        origin: None,
    }
}

fn user(inputs: Vec<CallArg>, commands: Vec<SuiCommand>, gas: ObjectRef) -> ScheduledEntry {
    let (sender, key): (SuiAddress, AccountKeyPair) = get_key_pair();
    let data = TransactionData::new(
        TransactionKind::ProgrammableTransaction(ProgrammableTransaction { inputs, commands }),
        sender,
        gas,
        1_000_000,
        1000,
    );
    signed(data, &key)
}

/// `data` signed by its sender's `key` only, which is all parsing needs.
fn signed(data: TransactionData, key: &AccountKeyPair) -> ScheduledEntry {
    let bytes = bcs::to_bytes(&Transaction::from_data_and_signer(data, vec![key])).unwrap();
    let transaction =
        checks::sequenced_by_consensus(Message::parse(bytes).map_err(|(e, _)| e).unwrap());
    ScheduledEntry {
        transaction: ExecutableTransaction::User(transaction),
        shared: vec![],
        origin: None,
    }
}

fn id(n: u8) -> ObjectID {
    ObjectID::new([n; 32])
}

fn version(id: ObjectID, version: u64) -> InputKey {
    InputKey::ObjectVersion {
        id: ObjectId(id.into_bytes()),
        version,
    }
}

fn generation(stream: StreamId, round: u32, generation: u32) -> InputKey {
    InputKey::Generation {
        stream,
        round,
        generation,
    }
}

/// The commit handler, which links each generation-0 access to an earlier
/// round's generation before the resolver sees it.
fn handler() -> (CommitHandler, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let (_, output, _) = EpochStore::open(dir.path(), 0);
    let handler = CommitHandler::new(output, 0, vec![([0; 96], 10_000)], &epoch().config, None);
    (handler, dir)
}

/// `entry`'s keys and resolved bases, its accesses linked by `handler`.
fn resolve(
    resolver: &mut VersionResolver,
    handler: &mut CommitHandler,
    mut entry: ScheduledEntry,
) -> (Vec<InputKey>, Vec<(StreamId, u32, u32)>) {
    handler.link(&mut entry.shared);
    let base = resolver.resolve(&entry, &Digest::new(CHAIN));
    (resolver.keys.clone(), base)
}

#[test]
fn owned_receiving_package_and_gas_inputs_are_exact_keys() {
    let (mut resolver, _inbox) = resolver();
    let (mut handler, _dir) = handler();
    let owned = (id(1), SequenceNumber::from_u64(3), random_object_ref().2);
    let receiving = (id(2), SequenceNumber::from_u64(7), random_object_ref().2);
    let gas = (id(3), SequenceNumber::from_u64(2), random_object_ref().2);
    let (p, q, r) = (id(0x50), id(0x51), id(0x52));
    let t: sui_types::TypeTag = format!("{q}::m::T<vector<{r}::m::U>>").parse().unwrap();
    let call = |package| {
        SuiCommand::move_call(
            package,
            Identifier::new("m").unwrap(),
            Identifier::new("f").unwrap(),
            vec![t.clone()],
            vec![],
        )
    };
    let entry = user(
        vec![
            CallArg::Object(ObjectArg::ImmOrOwnedObject(owned)),
            CallArg::Pure(vec![1]),
            CallArg::Object(ObjectArg::Receiving(receiving)),
        ],
        vec![
            call(p),
            call(p),
            SuiCommand::MakeMoveVec(Some(TypeInput::from(t.clone())), vec![]),
        ],
        gas,
    );
    let (keys, base) = resolve(&mut resolver, &mut handler, entry);
    let package = |id: ObjectID| InputKey::Package(ObjectId(id.into_bytes()));
    assert_eq!(
        keys,
        [
            version(id(1), 3),
            version(id(2), 7),
            package(p),
            package(q),
            package(r),
            version(id(3), 2),
        ]
    );
    assert!(base.is_empty());
}

#[test]
fn an_unseen_stream_read_or_written_non_exclusively_is_live() {
    let (mut resolver, _inbox) = resolver();
    let (mut handler, _dir) = handler();
    let s = stream(1);
    for mutability in [Immutable, NonExclusiveWrite] {
        let (keys, base) = resolve(
            &mut resolver,
            &mut handler,
            system(vec![access(s, mutability, 5, 0)]),
        );
        assert_eq!(keys, [InputKey::StreamLive(s)]);
        assert!(base.is_empty());
    }
    let (keys, _) = resolve(
        &mut resolver,
        &mut handler,
        system(vec![access(s, Immutable, 6, 0)]),
    );
    assert_eq!(keys, [generation(s, 5, 0)]);
}

#[test]
fn a_later_round_waits_for_the_generation_zero_of_a_live_round() {
    let (mut resolver, _inbox) = resolver();
    let (mut handler, _dir) = handler();
    let s = stream(1);
    // Round 5 reads the stream live, then writes it non-exclusively.
    for mutability in [Immutable, NonExclusiveWrite, Immutable] {
        let (keys, base) = resolve(
            &mut resolver,
            &mut handler,
            system(vec![access(s, mutability, 5, 0)]),
        );
        assert_eq!(keys, [InputKey::StreamLive(s)]);
        assert!(base.is_empty());
    }
    // Round 6's writer must not run before them: it reads the generation 0
    // round 5's first execution records.
    let (keys, base) = resolve(
        &mut resolver,
        &mut handler,
        system(vec![access(s, Mutable, 6, 0)]),
    );
    assert_eq!(keys, [generation(s, 5, 0)]);
    assert_eq!(base, [(s, 5, 0)]);
    let (keys, base) = resolve(
        &mut resolver,
        &mut handler,
        system(vec![access(s, Immutable, 7, 0)]),
    );
    assert_eq!(keys, [generation(s, 6, 1)]);
    assert_eq!(base, [(s, 6, 1)]);

    let (store, _dir) = store(&[coin(ObjectID::new(s.id.0), 3)]);
    let (generations, _generations_dir) = generation_table();
    let mut availability = InputAvailability {
        store,
        generations: generations.clone(),
    };
    let mut found = Vec::new();
    availability.check(&[generation(s, 5, 0)], &mut found);
    assert!(found[0].is_none());
    generations.get_or_record(s, 5, 0, || Generation::Version {
        version: 3,
        digest: [0; 32],
    });
    availability.check(&[generation(s, 5, 0)], &mut found);
    let Some(InputValue::Generation(Generation::Version { version: 3, .. }, Some(object))) =
        &found[0]
    else {
        panic!("{found:?}");
    };
    assert_eq!(object.get().version(), 3);
}

#[test]
fn a_live_write_in_a_round_moves_the_stream_past_generation_zero() {
    let (mut resolver, _inbox) = resolver();
    let (mut handler, _dir) = handler();
    let s = stream(1);
    resolve(
        &mut resolver,
        &mut handler,
        system(vec![access(s, Immutable, 5, 0)]),
    );
    let (keys, _) = resolve(
        &mut resolver,
        &mut handler,
        system(vec![access(s, Mutable, 5, 0)]),
    );
    assert_eq!(keys, [InputKey::StreamLive(s)]);
    let (keys, _) = resolve(
        &mut resolver,
        &mut handler,
        system(vec![access(s, Immutable, 6, 0)]),
    );
    assert_eq!(keys, [generation(s, 5, 1)]);
}

#[test]
fn exclusive_writes_chain_generations_within_a_round() {
    let (mut resolver, _inbox) = resolver();
    let (mut handler, _dir) = handler();
    let s = stream(1);
    let steps = [
        (Mutable, 0, InputKey::StreamLive(s)),
        (Immutable, 1, generation(s, 5, 1)),
        (Mutable, 1, generation(s, 5, 1)),
        (NonExclusiveWrite, 2, generation(s, 5, 2)),
        (Immutable, 2, generation(s, 5, 2)),
    ];
    for (mutability, k, expected) in steps {
        let (keys, base) = resolve(
            &mut resolver,
            &mut handler,
            system(vec![access(s, mutability, 5, k)]),
        );
        assert_eq!(keys, [expected]);
        assert!(base.is_empty());
    }
    let (keys, _) = resolve(
        &mut resolver,
        &mut handler,
        system(vec![access(s, Immutable, 6, 0)]),
    );
    assert_eq!(keys, [generation(s, 5, 2)]);
}

#[test]
fn generation_zero_reads_the_last_generation_of_an_earlier_round() {
    let (mut resolver, _inbox) = resolver();
    let (mut handler, _dir) = handler();
    let (s, other) = (stream(1), stream(2));
    resolve(
        &mut resolver,
        &mut handler,
        system(vec![access(s, Mutable, 3, 0)]),
    );
    resolve(
        &mut resolver,
        &mut handler,
        system(vec![access(s, Mutable, 3, 1)]),
    );
    // Round 4 only reads and writes non-exclusively: the stream stays at
    // round 3's last generation.
    for mutability in [Immutable, NonExclusiveWrite] {
        let (keys, base) = resolve(
            &mut resolver,
            &mut handler,
            system(vec![access(s, mutability, 4, 0)]),
        );
        assert_eq!(keys, [generation(s, 3, 2)]);
        assert_eq!(base, [(s, 3, 2)]);
    }
    // Round 7's writer reads it too, alongside an unseen stream.
    let (keys, base) = resolve(
        &mut resolver,
        &mut handler,
        system(vec![
            access(other, Immutable, 7, 0),
            access(s, Mutable, 7, 0),
        ]),
    );
    assert_eq!(keys, [InputKey::StreamLive(other), generation(s, 3, 2)]);
    assert_eq!(base, [(s, 3, 2)]);
    let (keys, base) = resolve(
        &mut resolver,
        &mut handler,
        system(vec![access(s, Immutable, 8, 0)]),
    );
    assert_eq!(keys, [generation(s, 7, 1)]);
    assert_eq!(base, [(s, 7, 1)]);
}

#[test]
#[should_panic(expected = "after a write in that round")]
fn generation_zero_after_a_write_in_its_round_panics() {
    let (mut resolver, _inbox) = resolver();
    let (mut handler, _dir) = handler();
    let s = stream(1);
    resolve(
        &mut resolver,
        &mut handler,
        system(vec![access(s, Mutable, 3, 0)]),
    );
    resolve(
        &mut resolver,
        &mut handler,
        system(vec![access(s, Immutable, 3, 0)]),
    );
}

/// A store holding `objects`, in a directory kept alive with it.
fn store(objects: &[Object]) -> (Arc<store::Store>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = store::Store::open(dir.path()).unwrap();
    store
        .write_through(store::Commit {
            written: objects.iter().map(written).collect(),
            ..store::Commit::default()
        })
        .unwrap();
    (Arc::new(store), dir)
}

fn written(object: &Object) -> store::Written {
    store::Written {
        id: ObjectId(object.id().into_bytes()),
        version: object.version().value(),
        digest: Digest::new(object.digest().into_inner()),
        bytes: bcs::to_bytes(object).unwrap(),
    }
}

/// `object` as the input waiter resolves a key to it.
fn value(object: &Object) -> Message<messages::object::Object<'static>> {
    Message::parse(bcs::to_bytes(object).unwrap())
        .map_err(|(e, _)| e)
        .unwrap()
}

fn coin(id: ObjectID, version: u64) -> Object {
    Object::with_id_owner_version_for_testing(
        id,
        SequenceNumber::from_u64(version),
        Owner::AddressOwner(SuiAddress::ZERO),
    )
}

#[test]
fn availability_reads_the_store_and_the_generation_table() {
    let package = id(0x50);
    let s = stream(9);
    let (store, _dir) = store(&[
        coin(id(1), 5),
        coin(package, 1),
        coin(ObjectID::new(s.id.0), 4),
    ]);
    let (generations, _generations_dir) = generation_table();
    generations.record(
        s,
        1,
        1,
        Generation::Version {
            version: 4,
            digest: [0; 32],
        },
        [0; 32],
        true,
    );
    generations.record(
        s,
        1,
        2,
        Generation::Ended {
            version: 5,
            transaction: [0; 32],
        },
        [0; 32],
        true,
    );
    let mut availability = InputAvailability { store, generations };
    let keys = [
        version(id(1), 5),
        version(id(1), 6),
        version(id(2), 5),
        InputKey::Package(ObjectId(package.into_bytes())),
        InputKey::Package(ObjectId([0x51; 32])),
        generation(s, 1, 1),
        // A stream's end counts: its readers resolve to it.
        generation(s, 1, 2),
        generation(s, 1, 3),
        generation(s, 2, 1),
        InputKey::StreamLive(stream(3)),
    ];
    let mut found = Vec::new();
    availability.check(&keys, &mut found);
    assert_eq!(
        found.iter().map(Option::is_some).collect::<Vec<_>>(),
        [
            true, false, false, true, false, true, true, false, false, true
        ]
    );
    // Each found key resolves to what it names.
    assert!(matches!(&found[0], Some(InputValue::Object(o)) if o.get().version() == 5));
    assert!(matches!(&found[3], Some(InputValue::Object(o)) if o.get().version() == 1));
    assert!(matches!(
        &found[5],
        Some(InputValue::Generation(Generation::Version { version: 4, .. }, Some(o)))
            if o.get().version() == 4
    ));
    assert!(matches!(
        &found[6],
        Some(InputValue::Generation(Generation::Ended { .. }, None))
    ));
    assert!(matches!(&found[9], Some(InputValue::Live)));
    // `Reserved` resolves only when `FundsReserver` resolves it.
    availability.check(&[InputKey::Reserved([1; 32])], &mut found);
    assert!(found[0].is_none());
}

#[test]
fn a_resolved_chunk_waits_in_the_input_waiter_until_its_keys_are_available() {
    let gas = coin(id(3), 1);
    let (store, _dir) = store(std::slice::from_ref(&gas));
    let (generations, _generations_dir) = generation_table();
    let mut waiter = Waiter::new(InputAvailability {
        store: store.clone(),
        generations: generations.clone(),
    });
    let (mut resolver, inbox) = resolver();
    let (mut handler, _handler_dir) = handler();
    let mut link = |mut chunk: Chunk| {
        for entry in &mut chunk.entries {
            handler.link(&mut entry.shared);
        }
        chunk
    };
    let clock = stream(6);
    let owned = (id(1), SequenceNumber::from_u64(5), random_object_ref().2);
    let mut spend = user(
        vec![CallArg::Object(ObjectArg::ImmOrOwnedObject(owned))],
        vec![],
        gas.compute_object_reference(),
    );
    spend.shared = vec![access(clock, Immutable, 1, 1)];
    let chunk = Chunk {
        version: AccumulatorVersion(7),
        root_version: 1,
        round: 1,
        height: 4,
        checkpoint_seq: 2,
        entries: vec![
            system(vec![access(clock, Mutable, 1, 0)]),
            system(vec![access(clock, Immutable, 1, 1)]),
            spend,
        ],
    };
    assert!(resolver.process(link(chunk)).is_none());
    let Some(Command::Wait(mut batch)) = inbox.try_pop() else {
        panic!("a wait");
    };
    assert!(inbox.try_pop().is_none());
    assert_eq!(batch.len(), 3);
    waiter.wait_for(&mut batch);
    let mut ready = Vec::new();
    waiter.get_ready(&mut ready);
    // The prologue reads the clock as the store holds it.
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].height, 4);
    assert!(ready[0].resolved_base.is_empty());
    assert_eq!(ready[0].entry.shared[0].mutability, Mutable);

    store
        .write_through(store::Commit {
            written: vec![written(&coin(ObjectID::new(clock.id.0), 2))],
            ..store::Commit::default()
        })
        .unwrap();
    generations.record(
        clock,
        1,
        1,
        Generation::Version {
            version: 2,
            digest: [0; 32],
        },
        [0; 32],
        true,
    );
    waiter.resolve([(
        generation(clock, 1, 1),
        InputValue::Generation(
            Generation::Version {
                version: 2,
                digest: [0; 32],
            },
            Some(value(&coin(ObjectID::new(clock.id.0), 2))),
        ),
    )]);
    ready.clear();
    waiter.get_ready(&mut ready);
    assert_eq!(ready.len(), 1);
    assert!(matches!(
        ready[0].entry.transaction,
        ExecutableTransaction::System(_)
    ));

    store
        .write_through(store::Commit {
            written: vec![written(&coin(id(1), 5))],
            ..store::Commit::default()
        })
        .unwrap();
    waiter.resolve([(
        version(id(1), 5),
        InputValue::Object(value(&coin(id(1), 5))),
    )]);
    ready.clear();
    waiter.get_ready(&mut ready);
    assert_eq!(ready.len(), 1);
    assert!(matches!(
        ready[0].entry.transaction,
        ExecutableTransaction::User(_)
    ));
    assert_eq!(waiter.waiting(), 0);

    // The next round's prologue reads the clock as this one left it.
    let next = Chunk {
        version: AccumulatorVersion(7),
        root_version: 1,
        round: 2,
        height: 5,
        checkpoint_seq: 2,
        entries: vec![system(vec![access(clock, Mutable, 2, 0)])],
    };
    resolver.process(link(next));
    let Some(Command::Wait(mut batch)) = inbox.try_pop() else {
        panic!("a wait");
    };
    waiter.wait_for(&mut batch);
    ready.clear();
    waiter.get_ready(&mut ready);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].resolved_base, [(clock, 1, 1)]);
}

#[test]
fn an_empty_chunk_waits_for_nothing() {
    let (mut resolver, inbox) = resolver();
    let chunk = Chunk {
        version: AccumulatorVersion(7),
        root_version: 1,
        round: 1,
        height: 0,
        checkpoint_seq: 0,
        entries: vec![],
    };
    resolver.process(chunk);
    assert!(inbox.try_pop().is_none());
}

/// A user transaction of `sender` with `inputs` and no commands, whose gas
/// `owner` pays from `payment` (from its address balance if empty).
fn withdrawing(
    sender: SuiAddress,
    inputs: Vec<CallArg>,
    owner: SuiAddress,
    payment: Vec<ObjectRef>,
    budget: u64,
) -> TransactionData {
    TransactionData::new_with_gas_data(
        TransactionKind::ProgrammableTransaction(ProgrammableTransaction {
            inputs,
            commands: vec![],
        }),
        sender,
        GasData {
            payment,
            owner,
            price: 1000,
            budget,
        },
    )
}

/// The reference's withdrawals of `data`, as the resolver gives them.
fn reference_amounts(data: &TransactionData) -> Vec<(ObjectId, u64)> {
    data.process_funds_withdrawals_for_execution(ChainIdentifier::from(CheckpointDigest::new(
        CHAIN,
    )))
    .into_iter()
    .map(|(account, amount)| (ObjectId(account.inner().into_bytes()), amount))
    .collect()
}

/// Asserts the resolver computes the reference's withdrawals for `data`,
/// `accounts` of them.
fn assert_amounts_match(data: TransactionData, key: &AccountKeyPair, accounts: usize) {
    let expected = reference_amounts(&data);
    assert_eq!(expected.len(), accounts);
    let (mut resolver, _inbox) = resolver();
    let (mut handler, _dir) = handler();
    resolve(&mut resolver, &mut handler, signed(data, key));
    assert_eq!(resolver.amounts, expected);
}

fn reservation(account: ObjectID, amount: u64) -> ObjectRef {
    ParsedObjectRefWithdrawal::new(account, 0, amount).encode(
        SequenceNumber::from_u64(1),
        ChainIdentifier::from(CheckpointDigest::new(CHAIN)),
    )
}

#[test]
fn funds_withdrawal_inputs_debit_sender_sponsor_and_funder() {
    let (sender, key): (SuiAddress, AccountKeyPair) = get_key_pair();
    let (sponsor, funder) = (
        SuiAddress::random_for_testing_only(),
        SuiAddress::random_for_testing_only(),
    );
    let other: sui_types::TypeTag = format!("{}::m::C", id(0x50)).parse().unwrap();
    let withdraw = |w| CallArg::FundsWithdrawal(w);
    let inputs = vec![
        withdraw(FundsWithdrawalArg::balance_from_sender(5, GAS::type_tag())),
        CallArg::Pure(vec![1]),
        // Summed with the first.
        withdraw(FundsWithdrawalArg::balance_from_sender(7, GAS::type_tag())),
        withdraw(FundsWithdrawalArg::balance_from_sender(11, other)),
        withdraw(FundsWithdrawalArg::balance_from_sponsor(
            13,
            GAS::type_tag(),
        )),
        withdraw(FundsWithdrawalArg::balance_from_allowance(
            17,
            GAS::type_tag(),
            funder,
            id(0x60),
        )),
    ];
    let data = withdrawing(sender, inputs, sponsor, vec![random_object_ref()], 1000);
    assert_amounts_match(data, &key, 4);
}

#[test]
fn gas_paid_from_an_address_balance_debits_the_gas_owner() {
    let (sender, key): (SuiAddress, AccountKeyPair) = get_key_pair();
    let sponsor = SuiAddress::random_for_testing_only();
    assert_amounts_match(withdrawing(sender, vec![], sender, vec![], 1000), &key, 1);
    let sponsored = withdrawing(
        sender,
        vec![CallArg::FundsWithdrawal(
            FundsWithdrawalArg::balance_from_sender(3, GAS::type_tag()),
        )],
        sponsor,
        vec![],
        1000,
    );
    assert_amounts_match(sponsored, &key, 2);
    // No budget, nothing to withdraw.
    assert_amounts_match(withdrawing(sender, vec![], sender, vec![], 0), &key, 0);
}

#[test]
fn coin_reservations_debit_the_unmasked_account() {
    let (sender, key): (SuiAddress, AccountKeyPair) = get_key_pair();
    let inputs = vec![
        CallArg::Object(ObjectArg::ImmOrOwnedObject(reservation(id(1), 5))),
        CallArg::Object(ObjectArg::ImmOrOwnedObject(reservation(id(2), 7))),
    ];
    let gas = vec![reservation(id(1), 11), random_object_ref()];
    let data = withdrawing(sender, inputs, sender, gas, 1000);
    let expected = reference_amounts(&data);
    assert_eq!(expected, [(ObjectId([1; 32]), 16), (ObjectId([2; 32]), 7)]);
    assert_amounts_match(data, &key, 2);
}

#[test]
fn a_withdrawing_transaction_waits_on_its_reservations_instead_of_its_inputs() {
    let (mut resolver, inbox, funds) = resolver_with_funds();
    let (sender, key): (SuiAddress, AccountKeyPair) = get_key_pair();
    let owned = (id(1), SequenceNumber::from_u64(5), random_object_ref().2);
    let gas = (id(3), SequenceNumber::from_u64(2), random_object_ref().2);
    let data = withdrawing(
        sender,
        vec![
            CallArg::Object(ObjectArg::ImmOrOwnedObject(owned)),
            CallArg::FundsWithdrawal(FundsWithdrawalArg::balance_from_sender(9, GAS::type_tag())),
        ],
        sender,
        vec![gas],
        1000,
    );
    let amounts = reference_amounts(&data);
    assert_eq!(amounts.len(), 1);
    let s = stream(4);
    let mut spend = signed(data, &key);
    spend.shared = vec![access(s, Immutable, 1, 0)];
    let ExecutableTransaction::User(transaction) = &spend.transaction else {
        unreachable!()
    };
    let digest = transaction.get().0.digest().bytes;
    let chunk = Chunk {
        version: AccumulatorVersion(7),
        root_version: 1,
        round: 1,
        height: 4,
        checkpoint_seq: 2,
        entries: vec![
            system(vec![access(s, Immutable, 1, 0)]),
            spend,
            user(vec![], vec![], gas),
        ],
    };
    resolver.process(chunk);
    let expected: Vec<Reservation> = amounts
        .iter()
        .map(|&(account, amount)| Reservation {
            account,
            amount,
            version: AccumulatorVersion(7),
        })
        .collect();

    // The reservations go to `FundsReserver`, in consensus order...
    let Some(FundsCommand::Reserve(withdrawals)) = funds.try_pop() else {
        panic!("a reservation");
    };
    assert!(funds.try_pop().is_none());
    let [withdrawal] = &withdrawals[..] else {
        panic!("one withdrawal");
    };
    assert_eq!(withdrawal.digest, digest);
    assert_eq!(withdrawal.reservations, expected);

    // ...while the transaction waits in the input waiter like the others, on
    // its inputs and on its reservations resolving.
    let Some(Command::Wait(mut batch)) = inbox.try_pop() else {
        panic!("a wait");
    };
    assert_eq!(batch.len(), 3);
    let checked = std::cell::RefCell::new(Vec::new());
    let mut waiter = Waiter::new(|key: &InputKey| {
        checked.borrow_mut().push(*key);
        true
    });
    waiter.wait_for(&mut batch);
    let mut ready = Vec::new();
    waiter.get_ready(&mut ready);
    let [spend] = &ready
        .iter()
        .filter(|e| !e.reserved.is_empty())
        .collect::<Vec<_>>()[..]
    else {
        panic!("one withdrawing transaction");
    };
    assert_eq!(spend.height, 4);
    assert_eq!(spend.version, AccumulatorVersion(7));
    assert_eq!(spend.reserved, expected);
    drop(waiter);
    let checked = checked.into_inner();
    for key in [
        InputKey::StreamLive(s),
        version(id(1), 5),
        version(id(3), 2),
        InputKey::Reserved(digest),
    ] {
        assert!(checked.contains(&key), "{key:?}");
    }
    assert_eq!(
        checked
            .iter()
            .filter(|k| matches!(k, InputKey::Reserved(_)))
            .count(),
        1
    );
}
