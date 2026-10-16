// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `SettlementScheduler` and `FundsReserver` driven as processors over a
//! real store: what lands in their outward queues, in what order, and what
//! is visible when it does.

mod support;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use messages::Message;
use messages::base::{Digest, ObjectId};
use messages::effects::TransactionEffects;
use messages::transaction::SharedObjectMutability;
use protocol_config::{Chain, ProtocolVersion};
use settlement::settle::{self, SettlementBuilder};
use settlement::{FundsReserver, SettlementScheduler};
use validator::consensus::sequence::{SharedAccess, SharedOrdinal};
use validator::epoch::EpochParams;
use validator::pipeline::items::{
    ChunkSettlement, EffectsWaitItem, Executable, ExecutableTransaction, FundsCommand,
    InputCommand, InputValue, PendingCheckpoint, ReservationStatus, Settle, SettlementInput,
    SystemKind, SystemTransaction, Withdrawal,
};
use validator::pipeline::keys::{
    AccumulatorVersion, ChunkSettled, EffectsKey, InputKey, Reservation, StreamId, TxDigest,
};
use validator::pipeline::tables::ChunkTable;
use waiter::Command;
use workqueue::{Inbox, Processor, Queue};

const ISV: u64 = 1234;
const SEQ: u64 = 4;
/// The root's version in the store before the first chunk.
const ROOT_VERSION: u64 = 19;

fn epoch() -> Arc<EpochParams> {
    Arc::new(EpochParams::new(
        Chain::Unknown,
        ProtocolVersion::MAX.as_u64(),
        7,
        Digest::ZERO,
        1000,
        4,
    ))
}

fn stream() -> StreamId {
    StreamId {
        id: ObjectId(support::ROOT),
        initial_shared_version: ISV,
    }
}

/// Chunk `height` of commit `version`, in `round`, its last if `last`.
fn chunk(
    height: u64,
    (round, writes_before): (u32, u32),
    (version, last): (u64, bool),
    digests: Vec<TxDigest>,
) -> ChunkSettlement {
    ChunkSettlement {
        version: AccumulatorVersion(version),
        root_version: ROOT_VERSION,
        last_of_commit: last,
        round,
        height,
        checkpoint_seq: SEQ,
        digests,
        root: SharedAccess {
            id: ObjectId(support::ROOT),
            initial_shared_version: ISV,
            mutability: SharedObjectMutability::Mutable,
            ordinal: SharedOrdinal {
                round,
                writes_before,
            },
            base: None,
        },
    }
}

/// The items of the waits queued in `inbox`, the distinct keys they wait
/// for (in the order first named), and the keys it resolved.
fn drain<K: Eq + Clone, T, V>(inbox: &Inbox<Command<K, T, V>>) -> (Vec<T>, Vec<K>, Vec<K>) {
    let (mut items, mut keys, mut resolved) = (Vec::new(), Vec::new(), Vec::new());
    while let Some(command) = inbox.try_pop() {
        match command {
            Command::Wait(batch) => {
                for (item, item_keys) in batch.into_parts() {
                    for key in item_keys {
                        if !keys.contains(&key) {
                            keys.push(key);
                        }
                    }
                    items.push(item);
                }
            }
            Command::Resolve(k) => resolved.extend(k.into_iter().map(|(k, _)| k)),
        }
    }
    (items, keys, resolved)
}

/// What the scheduler told `FundsReserver`.
fn settles(inbox: &Inbox<FundsCommand>) -> Vec<Settle> {
    let mut settles = Vec::new();
    while let Some(command) = inbox.try_pop() {
        let FundsCommand::Settle(s) = command else {
            panic!("the scheduler only settles");
        };
        settles.extend(s);
    }
    settles
}

/// `Settled` of `version` with each of `accounts` at `balance`.
fn settled(version: u64, root_version: u64, accounts: &[ObjectId], balance: u128) -> Settle {
    Settle::Settled {
        balances: accounts.iter().map(|a| (*a, balance)).collect(),
        version: AccumulatorVersion(version),
        root_version,
    }
}

/// An object owned by `owner`, with `contents`, as BCS.
fn object(owner: messages::build::object::Owner, contents: Vec<u8>) -> Vec<u8> {
    use messages::build::base::{Digest as D, SequenceNumber, TransactionDigest};
    use messages::build::object::{Data, MoveObject, MoveObjectType, MoveObjectTypeInner, Object};
    bcs::to_bytes(&Object {
        data: Data::Move(MoveObject {
            type_: MoveObjectType(MoveObjectTypeInner::SuiBalanceAccumulatorField),
            has_public_transfer: false,
            version: SequenceNumber(3),
            contents,
        }),
        owner,
        previous_transaction: TransactionDigest(D([0; 32])),
        storage_rebate: 0,
    })
    .unwrap()
}

/// An accumulator account object holding `balance`.
fn account_object(balance: u128) -> Vec<u8> {
    use messages::build::base::SuiAddress;
    let mut contents = vec![0; 64];
    contents.extend_from_slice(&balance.to_le_bytes());
    object(
        messages::build::object::Owner::ObjectOwner(SuiAddress(support::ROOT)),
        contents,
    )
}

fn write(store: &store::Store, id: ObjectId, version: u64, bytes: Vec<u8>) {
    store
        .write_through(store::Commit {
            written: vec![store::Written {
                id,
                version,
                digest: Digest::new([1; 32]),
                bytes,
            }],
            ..store::Commit::default()
        })
        .unwrap();
}

fn system(executable: &Executable) -> &SystemTransaction {
    match &executable.entry.transaction {
        ExecutableTransaction::System(system) => system,
        ExecutableTransaction::User(_) => panic!("a system transaction"),
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    _epochs: tempfile::TempDir,
    store: Arc<store::Store>,
    chunks: Arc<ChunkTable>,
    scheduler: SettlementScheduler,
    effects: Inbox<Command<EffectsKey, EffectsWaitItem>>,
    inputs: Inbox<InputCommand>,
    checkpoints: Inbox<Command<ChunkSettled, PendingCheckpoint>>,
    checkpoints_queue: Queue<Command<ChunkSettled, PendingCheckpoint>>,
    funds: Inbox<FundsCommand>,
}

impl Harness {
    fn new(checkpoint_capacity: usize, funds_capacity: usize) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(store::Store::open(dir.path()).unwrap());
        let epochs = tempfile::tempdir().unwrap();
        let (epoch_store, _, _) = validator::epoch_store::EpochStore::open(epochs.path(), 0);
        let chunks = Arc::new(ChunkTable::new(epoch_store));
        write(
            &store,
            ObjectId(support::ROOT),
            ROOT_VERSION,
            support::root_object(ISV),
        );
        let (effects_q, effects) = workqueue::queue(64);
        let (inputs_q, inputs) = workqueue::queue(64);
        let (checkpoints_queue, checkpoints) = workqueue::queue(checkpoint_capacity);
        let (funds_q, funds) = workqueue::queue(funds_capacity);
        let scheduler = SettlementScheduler::new(
            epoch(),
            store.clone(),
            chunks.clone(),
            effects_q,
            inputs_q,
            checkpoints_queue.clone(),
            funds_q,
        );
        Harness {
            _dir: dir,
            _epochs: epochs,
            store,
            chunks,
            scheduler,
            effects,
            inputs,
            checkpoints,
            checkpoints_queue,
            funds,
        }
    }

    /// Commits `effects` as the executed effects of their transaction.
    fn execute(&self, effects: &[u8]) {
        let view = Message::<TransactionEffects<'static>>::parse(effects.to_vec())
            .map_err(|(e, _)| e)
            .unwrap();
        let digest = match &view.get().version {
            messages::effects::VersionedEffects::V2(v2) => *v2.transaction_digest,
            messages::effects::VersionedEffects::V1(_) => unreachable!(),
        };
        self.store
            .write_through(store::Commit {
                executed: Some(store::Executed {
                    digest,
                    transaction: Vec::new(),
                    effects_digest: *view.digest(),
                    effects: effects.to_vec(),
                    events: None,
                }),
                ..store::Commit::default()
            })
            .unwrap();
    }

    fn process(&mut self, input: SettlementInput) {
        self.scheduler.process(input);
    }

    /// Settles the chunk the scheduler is settling: executes its
    /// settlement transactions (if any) and its barrier, returning both.
    fn settle_current(&mut self, height: u64) -> (Vec<Executable>, Executable) {
        let sent = drain(&self.inputs).0;
        self.settle_sent(height, sent)
    }

    /// `settle_current`, its first transactions already taken off the
    /// input queue.
    fn settle_sent(
        &mut self,
        height: u64,
        mut sent: Vec<Executable>,
    ) -> (Vec<Executable>, Executable) {
        let (waits, _, _) = drain(&self.effects);
        let settlements = if matches!(system(&sent[0]).kind, SystemKind::Settlement { .. }) {
            for s in &sent {
                self.execute(&support::effects(system(s).digest, 20, &[], vec![]));
            }
            let digests: Vec<TxDigest> = sent.iter().map(|s| system(s).digest).collect();
            assert!(
                waits.iter().any(|w| matches!(w,
                    EffectsWaitItem::SettlementExecuted { height: h, digests: d }
                        if *h == height && *d == digests)),
                "a wait for the settlements"
            );
            self.process(SettlementInput::SettlementExecuted { height, digests });
            let settlements = std::mem::take(&mut sent);
            sent = drain(&self.inputs).0;
            settlements
        } else {
            Vec::new()
        };
        let barrier = sent.pop().expect("the barrier");
        assert!(sent.is_empty());
        let digest = system(&barrier).digest;
        self.execute(&support::effects(
            digest,
            21,
            &[],
            vec![support::root_mutated(ISV, 20)],
        ));
        self.process(SettlementInput::SettlementExecuted {
            height,
            digests: vec![digest],
        });
        (settlements, barrier)
    }
}

/// The settlements `settlement` builds for `effects` at `offset`.
fn expected_settlements(effects: &[Vec<u8>], height: u64, offset: u64) -> Vec<settle::Built> {
    let parsed: Vec<Message<TransactionEffects<'static>>> = effects
        .iter()
        .map(|b| Message::parse(b.clone()).map_err(|(e, _)| e).unwrap())
        .collect();
    let mut builder = SettlementBuilder::new(SEQ, offset);
    for (i, e) in parsed.iter().enumerate() {
        builder.add(i as u64, e.get());
    }
    builder
        .build(
            epoch().config.max_updates_per_settlement_txn,
            7,
            ISV,
            height,
        )
        .into_iter()
        .map(settle::system_transaction)
        .collect()
}

fn digests_of(effects: &[Vec<u8>]) -> Vec<TxDigest> {
    effects
        .iter()
        .map(|b| {
            let view = Message::<TransactionEffects<'static>>::parse(b.clone())
                .map_err(|(e, _)| e)
                .unwrap();
            match &view.get().version {
                messages::effects::VersionedEffects::V2(v2) => v2.transaction_digest.bytes,
                messages::effects::VersionedEffects::V1(_) => unreachable!(),
            }
        })
        .collect()
}

#[test]
fn settles_a_chunk_end_to_end() {
    let mut h = Harness::new(8, 8);
    let effects = support::fixed_chunk();
    let digests = digests_of(&effects);
    let c = chunk(10, (3, 0), (1, true), digests.clone());

    h.process(SettlementInput::Chunk(c.clone()));
    // The first chunk seen starts from its own root version.
    assert_eq!(settles(&h.funds), [settled(1, ROOT_VERSION, &[], 0)]);
    let (waits, keys, _) = drain(&h.effects);
    assert!(matches!(&waits[..], [EffectsWaitItem::ChunkExecuted(w)] if w.height == 10));
    assert_eq!(
        keys,
        digests.iter().copied().map(EffectsKey).collect::<Vec<_>>()
    );

    for e in &effects {
        h.execute(e);
    }
    h.process(SettlementInput::ChunkExecuted(c));
    let (sent, keys, _) = drain(&h.inputs);
    let expected = expected_settlements(&effects, 10, 0);
    assert!(!expected.is_empty());
    assert_eq!(
        sent.iter().map(|e| system(e).digest).collect::<Vec<_>>(),
        expected.iter().map(|b| b.digest).collect::<Vec<_>>()
    );
    for (i, e) in sent.iter().enumerate() {
        assert_eq!(system(e).bytes, expected[i].bytes);
        assert_eq!(
            system(e).kind,
            SystemKind::Settlement {
                height: 10,
                index: i as u32
            }
        );
        assert_eq!(
            e.entry.shared[0].mutability,
            SharedObjectMutability::NonExclusiveWrite
        );
        assert!(e.resolved_base.is_empty());
    }
    // No chunk before it: the root as the store holds it.
    assert_eq!(keys, vec![InputKey::StreamLive(stream())]);
    let (waits, keys, _) = drain(&h.effects);
    assert!(matches!(
        &waits[..],
        [EffectsWaitItem::SettlementExecuted { height: 10, .. }]
    ));
    assert_eq!(keys.len(), expected.len());

    // The settlements created two accumulator objects.
    let settlement_effects = support::effects(
        expected[0].digest,
        20,
        &[],
        vec![
            support::created_or_deleted(support::id(0x2001), true),
            support::created_or_deleted(support::id(0x2002), true),
        ],
    );
    h.execute(&settlement_effects);
    h.process(SettlementInput::SettlementExecuted {
        height: 10,
        digests: expected.iter().map(|b| b.digest).collect(),
    });
    let (sent, _, _) = drain(&h.inputs);
    let [barrier] = &sent[..] else {
        panic!("one barrier");
    };
    let expected_barrier =
        settle::system_transaction(settle::barrier(7, ISV, 10, expected.len() as u64, (2, 0)));
    assert_eq!(system(barrier).digest, expected_barrier.digest);
    assert_eq!(system(barrier).kind, SystemKind::Barrier { height: 10 });
    assert_eq!(
        barrier.entry.shared[0].mutability,
        SharedObjectMutability::Mutable
    );
    let (waits, keys, _) = drain(&h.effects);
    assert!(
        matches!(&waits[..], [EffectsWaitItem::SettlementExecuted { height: 10, digests }]
        if digests == &[expected_barrier.digest])
    );
    assert_eq!(keys, vec![EffectsKey(expected_barrier.digest)]);
    assert!(h.chunks.get(10).is_none());

    h.execute(&support::effects(
        expected_barrier.digest,
        21,
        &[],
        vec![support::root_mutated(ISV, 20)],
    ));
    h.process(SettlementInput::SettlementExecuted {
        height: 10,
        digests: vec![expected_barrier.digest],
    });
    let chunk = h.chunks.get(10).expect("the chunk table holds the chunk");
    assert_eq!(chunk.checkpoint_seq, SEQ);
    let mut order = digests.clone();
    order.extend(expected.iter().map(|b| b.digest));
    order.push(expected_barrier.digest);
    assert_eq!(
        chunk.effects.iter().map(|e| e.digest).collect::<Vec<_>>(),
        order
    );
    assert_eq!(drain(&h.checkpoints).2, vec![ChunkSettled(10)]);
    // Every balance account the chunk wrote, as of the barrier's version
    // (none exists in this store), but not the event stream.
    assert_eq!(
        settles(&h.funds),
        [settled(2, 21, &support::fixed_chunk_accounts(), 0)]
    );
}

#[test]
fn chunks_settle_in_height_order_with_offsets_and_root_generations() {
    let mut h = Harness::new(8, 8);
    let first = support::fixed_chunk();
    // The second chunk feeds the event stream, so its settlement depends on
    // its offset in the checkpoint.
    let second = vec![
        support::effects(support::digest(50), 5, &[], vec![]),
        support::effects(
            support::digest(51),
            5,
            &[],
            vec![support::events(
                support::id(0x1005),
                support::id(0xc1),
                &[(0, 77)],
            )],
        ),
    ];
    let third = vec![support::effects(support::digest(60), 5, &[], vec![])];
    // Chunks 10 and 11 are one commit's, 12 the next's.
    let c10 = chunk(10, (3, 0), (1, false), digests_of(&first));
    let c11 = chunk(11, (3, 1), (1, true), digests_of(&second));
    let c12 = chunk(12, (5, 0), (2, true), digests_of(&third));
    for c in [&c10, &c11, &c12] {
        h.process(SettlementInput::Chunk(c.clone()));
    }
    drain(&h.effects);
    settles(&h.funds);
    for e in first.iter().chain(&second).chain(&third) {
        h.execute(e);
    }

    // Chunk 11's effects arrive first: it waits for chunk 10.
    h.process(SettlementInput::ChunkExecuted(c11));
    h.process(SettlementInput::ChunkExecuted(c12));
    assert!(drain(&h.inputs).0.is_empty());

    h.process(SettlementInput::ChunkExecuted(c10));
    let (settlements, _) = h.settle_current(10);
    assert_eq!(settlements.len(), 1);
    assert_eq!(drain(&h.checkpoints).2, vec![ChunkSettled(10)]);

    // Chunk 11 starts once 10 settled: its events are indexed after chunk
    // 10's five transactions, its settlement and its barrier.
    let (sent, keys, _) = drain(&h.inputs);
    let expected = expected_settlements(&second, 11, 7);
    assert_eq!(system(&sent[0]).digest, expected[0].digest);
    assert_ne!(expected, expected_settlements(&second, 11, 0));
    // Generation 1 of round 3: written by chunk 10's barrier.
    assert_eq!(
        keys,
        vec![InputKey::Generation {
            stream: stream(),
            round: 3,
            generation: 1
        }]
    );
    h.settle_sent(11, sent);

    // Chunk 12, in a later round, reads generation 2 of round 3, which chunk
    // 11's barrier writes; it has no settlements, only a barrier.
    let (sent, keys, _) = drain(&h.inputs);
    let [barrier] = &sent[..] else {
        panic!("only a barrier");
    };
    assert_eq!(system(barrier).kind, SystemKind::Barrier { height: 12 });
    assert_eq!(
        keys,
        vec![InputKey::Generation {
            stream: stream(),
            round: 3,
            generation: 2
        }]
    );
    assert_eq!(barrier.resolved_base, vec![(stream(), 3, 2)]);
    h.execute(&support::effects(
        system(barrier).digest,
        30,
        &[],
        vec![support::root_mutated(ISV, 29)],
    ));
    drain(&h.effects);
    h.process(SettlementInput::SettlementExecuted {
        height: 12,
        digests: vec![system(barrier).digest],
    });
    assert_eq!(
        drain(&h.checkpoints).2,
        vec![ChunkSettled(11), ChunkSettled(12)]
    );
    // One settlement per commit, once its last chunk settled, naming the
    // accounts of all its chunks.
    assert_eq!(
        settles(&h.funds),
        [
            settled(2, 21, &support::fixed_chunk_accounts(), 0),
            settled(3, 30, &[], 0)
        ]
    );
    assert_eq!(h.chunks.get(12).unwrap().effects.len(), 2);
}

#[test]
fn the_chunk_table_is_written_before_the_checkpoint_waiter_is_notified() {
    let mut h = Harness::new(1, 8);
    let effects = support::fixed_chunk();
    let c = chunk(10, (3, 0), (1, true), digests_of(&effects));
    h.process(SettlementInput::Chunk(c.clone()));
    for e in &effects {
        h.execute(e);
    }
    h.process(SettlementInput::ChunkExecuted(c));
    // A full checkpoint queue refuses the notification: whatever was done
    // before it is visible, whatever after is not.
    assert!(
        h.checkpoints_queue
            .try_push(Command::notify(vec![]))
            .is_ok()
    );
    let refused = catch_unwind(AssertUnwindSafe(|| h.settle_current(10)));
    assert!(refused.is_err());
    assert!(h.chunks.get(10).is_some());
    assert_eq!(settles(&h.funds), [settled(1, ROOT_VERSION, &[], 0)]);
}

#[test]
fn the_chunk_table_is_written_before_the_settlement_is_sent() {
    let mut h = Harness::new(8, 1);
    let effects = support::fixed_chunk();
    let c = chunk(10, (3, 0), (1, true), digests_of(&effects));
    h.process(SettlementInput::Chunk(c.clone()));
    for e in &effects {
        h.execute(e);
    }
    h.process(SettlementInput::ChunkExecuted(c));
    // The first `Settled` fills the funds queue, which refuses the next.
    let refused = catch_unwind(AssertUnwindSafe(|| h.settle_current(10)));
    assert!(refused.is_err());
    assert!(h.chunks.get(10).is_some());
    assert_eq!(drain(&h.checkpoints).2, vec![ChunkSettled(10)]);
}

#[test]
fn settled_balances_are_read_at_the_barriers_version() {
    let mut h = Harness::new(8, 8);
    let effects = support::fixed_chunk();
    let accounts = support::fixed_chunk_accounts();
    // Written by this settlement (at the barrier's version, 21), and by a
    // later one (22), which must not be read.
    write(&h.store, accounts[0], 21, account_object(500));
    write(&h.store, accounts[0], 22, account_object(9));
    let c = chunk(10, (3, 0), (1, true), digests_of(&effects));
    h.process(SettlementInput::Chunk(c.clone()));
    for e in &effects {
        h.execute(e);
    }
    h.process(SettlementInput::ChunkExecuted(c));
    h.settle_current(10);
    let all = settles(&h.funds);
    let Some(Settle::Settled { balances, .. }) = all.last() else {
        panic!("settled");
    };
    assert_eq!(balances[0], (accounts[0], 500));
    assert!(balances[1..].iter().all(|(_, b)| *b == 0));
}

fn withdrawal(n: u8, version: u64, amounts: &[(u16, u64)]) -> Withdrawal {
    let version = AccumulatorVersion(version);
    Withdrawal {
        digest: [n; 32],
        reservations: amounts
            .iter()
            .map(|&(account, amount)| Reservation {
                account: ObjectId(support::id(account)),
                amount,
                version,
            })
            .collect(),
    }
}

/// A reserver, and the input waiter commands it sends.
fn reserver(store: Arc<store::Store>) -> (FundsReserver, Inbox<InputCommand>) {
    let (inputs_q, inputs) = workqueue::queue(8);
    (FundsReserver::new(store, inputs_q), inputs)
}

/// Each transaction whose reservations resolved, by its digest's first
/// byte, with how they resolved, as resolved in the input waiter.
fn released(inputs: &Inbox<InputCommand>) -> Vec<(u8, ReservationStatus)> {
    let mut released = Vec::new();
    while let Some(command) = inputs.try_pop() {
        let Command::Resolve(resolved) = command else {
            panic!("the reserver only resolves");
        };
        for (key, value) in resolved {
            let (InputKey::Reserved(digest), InputValue::Reserved(status)) = (key, value) else {
                panic!("only `Reserved` keys");
            };
            released.push((digest[0], status));
        }
    }
    released
}

#[test]
fn reservations_resolve_in_consensus_order_against_the_settled_balance() {
    use ReservationStatus::{Cancelled, Fulfilled};
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(store::Store::open(dir.path()).unwrap());
    let (a, b, c) = (0x3001, 0x3002, 0x3003);
    // Balances at version 10, whose root is at version 3; `a` later changes.
    write(&store, ObjectId(support::id(a)), 3, account_object(100));
    write(&store, ObjectId(support::id(b)), 3, account_object(50));
    write(&store, ObjectId(support::id(a)), 4, account_object(1));
    let (mut reserver, inputs) = reserver(store);
    reserver.process(FundsCommand::Settle(vec![settled(10, 3, &[], 0)]));

    reserver.process(FundsCommand::Reserve(vec![
        withdrawal(1, 10, &[(a, 60)]),
        // 40 left in `a`.
        withdrawal(2, 10, &[(a, 50)]),
        // `b` falls short, but `a` is charged anyway, as in the reference.
        withdrawal(3, 10, &[(a, 40), (b, 60)]),
        withdrawal(4, 10, &[(a, 1)]),
        withdrawal(5, 10, &[(b, 50)]),
        // An account never funded holds nothing.
        withdrawal(6, 10, &[(c, 0)]),
        withdrawal(7, 10, &[(c, 1)]),
    ]));
    assert_eq!(
        released(&inputs),
        [
            (1, Fulfilled),
            (2, Cancelled),
            (3, Cancelled),
            (4, Cancelled),
            (5, Fulfilled),
            (6, Fulfilled),
            (7, Cancelled),
        ]
    );
}

#[test]
fn a_transaction_waits_for_all_its_reservations() {
    use ReservationStatus::{Cancelled, Fulfilled};
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(store::Store::open(dir.path()).unwrap());
    let (a, b) = (0x3001, 0x3002);
    write(&store, ObjectId(support::id(a)), 3, account_object(100));
    let (mut reserver, inputs) = reserver(store);
    reserver.process(FundsCommand::Settle(vec![settled(10, 3, &[], 0)]));

    // Version 11 is unsettled: `a` is certain, `b` (empty) is not.
    reserver.process(FundsCommand::Reserve(vec![
        withdrawal(1, 11, &[(a, 30), (b, 5)]),
        withdrawal(2, 11, &[(a, 30)]),
    ]));
    assert_eq!(released(&inputs), [(2, Fulfilled)]);
    // A deposit commit 10 made into `b` settles the doubt early.
    reserver.process(FundsCommand::Settle(vec![Settle::Deposit(
        ObjectId(support::id(b)),
        5,
        AccumulatorVersion(11),
    )]));
    assert_eq!(released(&inputs), [(1, Fulfilled)]);

    // Version 12: `b` will hold at least 5 − 5; this waits for settlement.
    reserver.process(FundsCommand::Reserve(vec![withdrawal(3, 12, &[(b, 1)])]));
    assert!(released(&inputs).is_empty());
    reserver.process(FundsCommand::Settle(vec![settled(11, 4, &[], 0)]));
    assert!(released(&inputs).is_empty());
    let b_id = ObjectId(support::id(b));
    reserver.process(FundsCommand::Settle(vec![Settle::Settled {
        balances: vec![(b_id, 0)],
        version: AccumulatorVersion(12),
        root_version: 5,
    }]));
    assert_eq!(released(&inputs), [(3, Cancelled)]);
}

#[test]
fn unwithdrawn_reservations_are_credited_to_later_versions() {
    use ReservationStatus::{Cancelled, Fulfilled};
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(store::Store::open(dir.path()).unwrap());
    let (a, b) = (0x3001, 0x3002);
    write(&store, ObjectId(support::id(a)), 3, account_object(100));
    let (mut reserver, inputs) = reserver(store);
    reserver.process(FundsCommand::Settle(vec![settled(10, 3, &[], 0)]));

    // `a` is certain at 11, `b` (empty) is not; 12 has nothing left of `a`.
    reserver.process(FundsCommand::Reserve(vec![
        withdrawal(1, 11, &[(a, 100), (b, 5)]),
        withdrawal(2, 12, &[(a, 50)]),
    ]));
    assert!(released(&inputs).is_empty());
    // 11 settles: `b` cancels the first, which withdraws nothing, so its 100
    // from `a` is credited back and the second is certain.
    reserver.process(FundsCommand::Settle(vec![settled(11, 4, &[], 0)]));
    assert_eq!(released(&inputs), [(1, Cancelled), (2, Fulfilled)]);

    // L(13) = 100 − 50: a reservation of 90 waits until the second's
    // transaction turns out to have withdrawn only 10.
    reserver.process(FundsCommand::Reserve(vec![withdrawal(3, 13, &[(a, 90)])]));
    assert!(released(&inputs).is_empty());
    reserver.process(FundsCommand::Settle(vec![Settle::Credit(
        ObjectId(support::id(a)),
        40,
        AccumulatorVersion(12),
    )]));
    assert_eq!(released(&inputs), [(3, Fulfilled)]);
}
