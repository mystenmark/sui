// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A funded genesis, signed transactions on a shared coin, and an input
//! waiter driven by hand into an `Executor`, checking that every key it
//! notifies is visible by then.

#![allow(dead_code)]

use std::sync::Arc;

use exec_pool::{EffectsAvailability, Executor};
use messages::Message;
use messages::base::ObjectId;
use messages::transaction::SharedObjectMutability;
use move_core_types::ident_str;
use protocol_config::{Chain, ProtocolVersion};
use resolver::InputAvailability;
use sui_types::base_types::{ObjectID, ObjectRef, SequenceNumber, SuiAddress};
use sui_types::crypto::{AccountKeyPair, get_key_pair};
use sui_types::digests::TransactionDigest;
use sui_types::effects::{TransactionEffects, TransactionEffectsAPI};
use sui_types::gas_coin::{GAS, GasCoin};
use sui_types::object::{Object, Owner};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{
    Argument, CallArg, Command as PtbCommand, ObjectArg, Transaction, TransactionData,
};
use validator::checks::{self, VerifiedTransaction};
use validator::consensus::sequence::{SharedAccess, SharedOrdinal};
use validator::epoch::EpochState;
use validator::epoch_store::EpochStore;
use validator::pipeline::items::{
    EffectsWaitItem, Executable, ExecutableTransaction, FundsCommand, InputCommand, InputValue,
    ScheduledEntry,
};
use validator::pipeline::keys::{AccumulatorVersion, EffectsKey, InputKey, StreamId};
use validator::pipeline::tables::GenerationTable;
use waiter::{Availability, Command, Receive, WaitBatch, Waiter};
use workqueue::{Inbox, Processor};

pub const RGP: u64 = 1000;
pub const BUDGET: u64 = 50_000_000;
pub const SUI: u64 = 1_000_000_000;

pub struct Node {
    _dir: tempfile::TempDir,
    pub epoch: Arc<EpochState>,
    pub store: Arc<store::Store>,
    pub epoch_store: EpochStore,
    pub generations: Arc<GenerationTable>,
    pub sender: SuiAddress,
    key: AccountKeyPair,
    pub executor: Executor,
    pub notified_inputs: Inbox<InputCommand>,
    pub notified_effects: Inbox<Command<EffectsKey, EffectsWaitItem>>,
    pub notified_funds: Inbox<FundsCommand>,
}

/// A shared object's stream, as both sides name it.
#[derive(Clone, Copy, Debug)]
pub struct Shared {
    pub id: ObjectID,
    pub initial_shared_version: u64,
}

impl Shared {
    pub fn stream(&self) -> StreamId {
        StreamId {
            id: ObjectId(self.id.into_bytes()),
            initial_shared_version: self.initial_shared_version,
        }
    }

    fn arg(&self, mutable: bool) -> ObjectArg {
        ObjectArg::SharedObject {
            id: self.id,
            initial_shared_version: SequenceNumber::from_u64(self.initial_shared_version),
            mutability: if mutable {
                sui_types::transaction::SharedObjectMutability::Mutable
            } else {
                sui_types::transaction::SharedObjectMutability::Immutable
            },
        }
    }

    /// Its access at generation `k` of `round`.
    pub fn access(&self, mutable: bool, round: u32, k: u32) -> SharedAccess {
        SharedAccess {
            id: ObjectId(self.id.into_bytes()),
            initial_shared_version: self.initial_shared_version,
            mutability: if mutable {
                SharedObjectMutability::Mutable
            } else {
                SharedObjectMutability::Immutable
            },
            ordinal: SharedOrdinal {
                round,
                writes_before: k,
            },
        }
    }
}

impl Node {
    /// A genesis with `coins` gas coins of 10 SUI for the sender.
    pub fn new(coins: usize) -> Node {
        let (sender, key) = get_key_pair::<AccountKeyPair>();
        let epoch = Arc::new(EpochState::new(
            Chain::Unknown,
            ProtocolVersion::MAX.as_u64(),
            0,
            messages::base::Digest::new([0; 32]),
            RGP,
            4,
            [],
        ));
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(store::Store::open(&dir.path().join("perpetual")).unwrap());
        let epoch_store = EpochStore::open(&dir.path().join("epochs"), 0);
        let funded = vec![(sender.to_inner(), 10 * SUI); coins];
        assert!(execution::genesis::init(epoch.execution.reference(), &store, &funded).unwrap());
        let generations = Arc::new(GenerationTable::new());
        let (inputs, notified_inputs) = workqueue::queue(1 << 16);
        let (effects, notified_effects) = workqueue::queue(1 << 16);
        let (funds, notified_funds) = workqueue::queue(1 << 16);
        let executor = Executor::new(
            store.clone(),
            epoch.clone(),
            epoch_store.clone(),
            generations.clone(),
            inputs,
            effects,
            funds,
        );
        Node {
            _dir: dir,
            epoch,
            store,
            epoch_store,
            generations,
            sender,
            key,
            executor,
            notified_inputs,
            notified_effects,
            notified_funds,
        }
    }

    pub fn live(&self, id: ObjectID) -> Option<Object> {
        let object = self
            .store
            .live_object(&ObjectId(id.into_bytes()))
            .unwrap()?;
        Some(bcs::from_bytes(object.wire_bytes()).unwrap())
    }

    /// Gas coin `i`, as of now.
    pub fn coin(&self, i: u64) -> ObjectRef {
        let id = ObjectID::derive_id(TransactionDigest::genesis_marker(), i);
        self.live(id).unwrap().compute_object_reference()
    }

    pub fn sign(&self, data: TransactionData) -> VerifiedTransaction {
        let bytes =
            bcs::to_bytes(&Transaction::from_data_and_signer(data, vec![&self.key])).unwrap();
        checks::sequenced_by_consensus(Message::parse(bytes).map_err(|(e, _)| e).unwrap())
    }

    pub fn ptb(
        &self,
        gas: ObjectRef,
        f: impl FnOnce(&mut ProgrammableTransactionBuilder),
    ) -> VerifiedTransaction {
        let mut builder = ProgrammableTransactionBuilder::new();
        f(&mut builder);
        self.sign(TransactionData::new_programmable(
            self.sender,
            vec![gas],
            builder.finish(),
            BUDGET,
            RGP,
        ))
    }

    /// A transfer of 1 SUI to a fresh address, paid with `gas`.
    pub fn transfer(&self, gas: ObjectRef) -> VerifiedTransaction {
        self.sign(TransactionData::new_transfer_sui(
            SuiAddress::random_for_testing_only(),
            self.sender,
            Some(SUI),
            gas,
            BUDGET,
            RGP,
        ))
    }

    /// Splits 1 SUI off `gas` and shares the new coin.
    pub fn share(&self, gas: ObjectRef) -> VerifiedTransaction {
        self.ptb(gas, |b| {
            let amount = b.pure(SUI).unwrap();
            let coin = b.command(PtbCommand::SplitCoins(Argument::GasCoin, vec![amount]));
            b.programmable_move_call(
                sui_types::SUI_FRAMEWORK_PACKAGE_ID,
                ident_str!("transfer").to_owned(),
                ident_str!("public_share_object").to_owned(),
                vec![sui_types::TypeTag::Struct(Box::new(GasCoin::type_()))],
                vec![coin],
            );
        })
    }

    /// Writes the shared coin: splits a MIST off it, to the sender.
    pub fn increment(&self, gas: ObjectRef, shared: Shared) -> VerifiedTransaction {
        self.ptb(gas, |b| {
            b.move_call(
                sui_types::SUI_FRAMEWORK_PACKAGE_ID,
                ident_str!("pay").to_owned(),
                ident_str!("split").to_owned(),
                vec![GAS::type_tag()],
                vec![
                    CallArg::Object(shared.arg(true)),
                    CallArg::Pure(bcs::to_bytes(&1u64).unwrap()),
                ],
            )
            .unwrap();
        })
    }

    /// Reads the shared coin's value.
    pub fn read(&self, gas: ObjectRef, shared: Shared) -> VerifiedTransaction {
        self.ptb(gas, |b| {
            b.move_call(
                sui_types::SUI_FRAMEWORK_PACKAGE_ID,
                ident_str!("coin").to_owned(),
                ident_str!("value").to_owned(),
                vec![GAS::type_tag()],
                vec![CallArg::Object(shared.arg(false))],
            )
            .unwrap();
        })
    }

    /// Merges the shared coin into the gas coin, deleting it.
    pub fn delete(&self, gas: ObjectRef, shared: Shared) -> VerifiedTransaction {
        self.ptb(gas, |b| {
            let coin = b.obj(shared.arg(true)).unwrap();
            b.command(PtbCommand::MergeCoins(Argument::GasCoin, vec![coin]));
        })
    }

    pub fn executable(
        &self,
        transaction: VerifiedTransaction,
        shared: Vec<SharedAccess>,
        resolved_base: Vec<(StreamId, u32, u32)>,
    ) -> Executable {
        let mut executable = Executable {
            version: AccumulatorVersion(1),
            root_version: 1,
            reserved: Vec::new(),
            height: 0,
            entry: ScheduledEntry {
                transaction: ExecutableTransaction::User(transaction),
                shared,
                origin: None,
            },
            resolved_base,
            inputs: Vec::new(),
        };
        // Whatever the input waiter would find resolved now; it fills in the
        // rest.
        let mut found = Vec::new();
        self.lookup().check(&keys(&executable), &mut found);
        for (i, value) in found.into_iter().enumerate() {
            if let Some(value) = value {
                executable.receive(i, value);
            }
        }
        executable
    }

    /// The effects of the transaction with `digest`, if it executed.
    pub fn effects(&self, digest: [u8; 32]) -> Option<TransactionEffects> {
        execution::executed(&self.store, &digest)
            .unwrap()
            .map(|e| bcs::from_bytes(&e.effects).unwrap())
    }

    /// Executes `transaction` alone, with no shared inputs, through the
    /// executor; its effects.
    pub fn execute_owned(&mut self, transaction: VerifiedTransaction) -> TransactionEffects {
        let digest = digest(&transaction);
        let executable = self.executable(transaction, vec![], vec![]);
        self.executor.process(vec![executable]);
        self.drain_checked();
        self.effects(digest).unwrap()
    }

    /// Shares a coin split off gas coin `i`.
    pub fn new_shared(&mut self, i: u64) -> Shared {
        let effects = self.execute_owned(self.share(self.coin(i)));
        let (object, owner) = effects
            .created()
            .into_iter()
            .find(|(_, owner)| matches!(owner, Owner::Shared { .. }))
            .unwrap();
        let Owner::Shared {
            initial_shared_version,
        } = owner
        else {
            unreachable!()
        };
        Shared {
            id: object.0,
            initial_shared_version: initial_shared_version.value(),
        }
    }

    /// The input waiter's lookup.
    pub fn lookup(&self) -> InputAvailability {
        InputAvailability {
            store: self.store.clone(),
            generations: self.generations.clone(),
        }
    }

    /// Takes what the executor resolved, asserting each key is visible to the
    /// lookup already; the input keys and effects keys.
    pub fn drain_checked(&mut self) -> (Vec<(InputKey, InputValue)>, Vec<EffectsKey>) {
        let mut lookup = self.lookup();
        let mut effects_visible = EffectsAvailability {
            store: self.store.clone(),
        };
        let mut inputs = Vec::new();
        while let Some(command) = self.notified_inputs.try_pop() {
            let Command::Resolve(resolved) = command else {
                panic!("the executor only resolves");
            };
            let keys: Vec<InputKey> = resolved.iter().map(|(k, _)| *k).collect();
            let mut found = Vec::new();
            lookup.check(&keys, &mut found);
            for (i, key) in keys.iter().enumerate() {
                assert!(found[i].is_some(), "{key:?} resolved before it is visible");
            }
            inputs.extend(resolved);
        }
        let mut effects = Vec::new();
        while let Some(command) = self.notified_effects.try_pop() {
            let Command::Resolve(resolved) = command else {
                panic!("the executor only notifies");
            };
            let keys: Vec<EffectsKey> = resolved.into_iter().map(|(k, ())| k).collect();
            let mut found = Vec::new();
            effects_visible.check(&keys, &mut found);
            assert!(
                found.iter().all(Option::is_some),
                "effects notified before stored"
            );
            effects.extend(keys);
        }
        (inputs, effects)
    }
}

pub fn digest(transaction: &VerifiedTransaction) -> [u8; 32] {
    transaction.get().0.digest().bytes
}

/// The input waiter's keys for `executable`, as the resolver gives them: its
/// gas coins' versions and its shared accesses' generations.
pub fn keys(executable: &Executable) -> Vec<InputKey> {
    let ExecutableTransaction::User(transaction) = &executable.entry.transaction else {
        unreachable!()
    };
    // Shared accesses first, as the resolver keys them.
    let mut keys = Vec::new();
    for access in &executable.entry.shared {
        let stream = StreamId {
            id: access.id,
            initial_shared_version: access.initial_shared_version,
        };
        let SharedOrdinal {
            round,
            writes_before,
        } = access.ordinal;
        keys.push(if writes_before > 0 {
            InputKey::Generation {
                stream,
                round,
                generation: writes_before,
            }
        } else if let Some((_, round, generation)) =
            executable.resolved_base.iter().find(|(s, ..)| *s == stream)
        {
            InputKey::Generation {
                stream,
                round: *round,
                generation: *generation,
            }
        } else {
            InputKey::StreamLive(stream)
        });
    }
    keys.extend(
        transaction
            .get()
            .0
            .data()
            .gas_data()
            .payment
            .iter()
            .map(|r| InputKey::ObjectVersion {
                id: r.id,
                version: r.version.get(),
            }),
    );
    keys
}

/// An input waiter feeding `node`'s executor, resolved by it.
pub struct InputWaiter {
    waiter: Waiter<InputKey, Executable, InputAvailability, InputValue>,
}

impl InputWaiter {
    pub fn new(node: &Node) -> InputWaiter {
        InputWaiter {
            waiter: Waiter::new(node.lookup()),
        }
    }

    /// Waits each of `executables` on its keys, then executes whatever is
    /// ready, feeding the executor's notifications back, until nothing more
    /// is. How many batches it executed.
    pub fn run(&mut self, node: &mut Node, executables: Vec<Executable>) -> usize {
        let mut batch = WaitBatch::new();
        for executable in executables {
            let keys = keys(&executable);
            batch.push(executable, keys);
        }
        self.waiter.wait_for(&mut batch);
        let mut batches = 0;
        loop {
            let mut ready = Vec::new();
            self.waiter.get_ready(&mut ready);
            if ready.is_empty() {
                return batches;
            }
            batches += 1;
            node.executor.process(ready);
            let (inputs, _) = node.drain_checked();
            self.waiter.resolve(inputs);
        }
    }

    pub fn waiting(&self) -> usize {
        self.waiter.waiting()
    }
}
