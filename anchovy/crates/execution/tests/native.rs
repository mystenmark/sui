// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Anchovy's executor against sui's, transaction by transaction over one store: each transaction
//! runs through both against the same live objects, the outcomes must be identical, and sui's is
//! committed before the next.

use std::sync::Arc;

use execution::native::NativeExecution;
use execution::{Execution, Outcome, genesis};
use move_core_types::ident_str;
use sui_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use sui_types::base_types::{ObjectID, ObjectRef, SuiAddress};
use sui_types::crypto::{AccountKeyPair, get_key_pair};
use sui_types::effects::{TransactionEffects, TransactionEffectsAPI};
use sui_types::metrics::ExecutionMetrics;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{CallArg, Transaction, TransactionData};

const RGP: u64 = 1000;
const BUDGET: u64 = 50_000_000;
const SUI: u64 = 1_000_000_000;

struct Node {
    _dir: tempfile::TempDir,
    store: store::Store,
    reference: Execution,
    native: NativeExecution,
    sender: SuiAddress,
    key: AccountKeyPair,
}

impl Node {
    fn new() -> Node {
        let (sender, key) = get_key_pair::<AccountKeyPair>();
        let reference =
            Execution::new(ProtocolVersion::MAX.as_u64(), Chain::Unknown, 0, 0, RGP).unwrap();
        let native = NativeExecution::new(
            ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Unknown),
            Arc::new(ExecutionMetrics::new(&prometheus::Registry::new())),
            0,
            0,
            RGP,
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = store::Store::open(dir.path()).unwrap();
        let objects = genesis::objects(&reference, &[(sender, 10 * SUI)]).unwrap();
        genesis::commit(&store, &objects).unwrap();
        Node {
            _dir: dir,
            store,
            reference,
            native,
            sender,
            key,
        }
    }

    fn gas(&self) -> ObjectRef {
        let id = ObjectID::derive_id(sui_types::digests::TransactionDigest::genesis_marker(), 0);
        execution::StoreView::new(&self.store)
            .live_object(&id)
            .unwrap()
            .unwrap()
            .compute_object_reference()
    }

    /// Both executors on `data`; the effects, once they agree.
    fn execute_both_and_commit(&self, data: TransactionData) -> TransactionEffects {
        let transaction = Transaction::from_data_and_signer(data, vec![&self.key]);
        let bytes = bcs::to_bytes(transaction.data()).unwrap();
        let reference = self.reference.execute(&self.store, &bytes).unwrap();
        let native = self.native.execute_bytes(&self.store, &bytes).unwrap();
        let effects: TransactionEffects = bcs::from_bytes(&reference.effects).unwrap();
        assert_same(&reference, &native, &effects);
        self.store.commit(reference.commit).unwrap();
        effects
    }
}

fn assert_same(reference: &Outcome, native: &Outcome, effects: &TransactionEffects) {
    if native.effects != reference.effects {
        let native_effects: Result<TransactionEffects, _> = bcs::from_bytes(&native.effects);
        panic!("effects differ:\nreference {effects:#?}\nnative {native_effects:#?}");
    }
    let written = |o: &Outcome| {
        let mut w: Vec<_> = o
            .commit
            .written
            .iter()
            .map(|w| (w.id, w.version, w.digest, w.bytes.clone()))
            .collect();
        w.sort();
        w
    };
    assert_eq!(written(native), written(reference), "written objects");
    let removed = |o: &Outcome| {
        let mut r = o.commit.removed.clone();
        r.sort_by_key(|id| id.0);
        r
    };
    assert_eq!(removed(native), removed(reference), "removed objects");
    let (n, r) = (
        native.commit.executed.as_ref().unwrap(),
        reference.commit.executed.as_ref().unwrap(),
    );
    assert_eq!(n.digest, r.digest);
    assert_eq!(n.effects_digest, r.effects_digest);
    assert_eq!(n.events, r.events, "events");
}

#[test]
fn transfers_match() {
    let node = Node::new();
    let recipient = SuiAddress::random_for_testing_only();
    for _ in 0..3 {
        let effects = node.execute_both_and_commit(TransactionData::new_transfer_sui(
            recipient,
            node.sender,
            Some(SUI),
            node.gas(),
            BUDGET,
            RGP,
        ));
        assert!(effects.status().is_ok(), "{effects:?}");
    }
}

#[test]
fn a_failed_transfer_matches() {
    let node = Node::new();
    let effects = node.execute_both_and_commit(TransactionData::new_transfer_sui(
        SuiAddress::random_for_testing_only(),
        node.sender,
        Some(100 * SUI),
        node.gas(),
        BUDGET,
        RGP,
    ));
    assert!(!effects.status().is_ok());
}

#[test]
fn a_shared_clock_read_matches() {
    let node = Node::new();
    let mut builder = ProgrammableTransactionBuilder::new();
    builder
        .move_call(
            sui_types::SUI_FRAMEWORK_ADDRESS.into(),
            ident_str!("clock").to_owned(),
            ident_str!("timestamp_ms").to_owned(),
            vec![],
            vec![CallArg::CLOCK_IMM],
        )
        .unwrap();
    let effects = node.execute_both_and_commit(TransactionData::new_programmable(
        node.sender,
        vec![node.gas()],
        builder.finish(),
        BUDGET,
        RGP,
    ));
    assert!(effects.status().is_ok(), "{effects:?}");
}

#[test]
fn split_and_merge_match() {
    let node = Node::new();
    let mut builder = ProgrammableTransactionBuilder::new();
    // A vector where a u64 is expected.
    let amounts = builder.pure(vec![SUI, 2 * SUI, 3 * SUI]).unwrap();
    builder.command(sui_types::transaction::Command::SplitCoins(
        sui_types::transaction::Argument::GasCoin,
        vec![amounts],
    ));
    let a = builder.pure(SUI).unwrap();
    let b = builder.pure(2 * SUI).unwrap();
    let split = builder.command(sui_types::transaction::Command::SplitCoins(
        sui_types::transaction::Argument::GasCoin,
        vec![a, b],
    ));
    let sui_types::transaction::Argument::Result(i) = split else {
        unreachable!()
    };
    builder.command(sui_types::transaction::Command::MergeCoins(
        sui_types::transaction::Argument::NestedResult(i, 0),
        vec![sui_types::transaction::Argument::NestedResult(i, 1)],
    ));
    builder.transfer_arg(
        node.sender,
        sui_types::transaction::Argument::NestedResult(i, 0),
    );
    let effects = node.execute_both_and_commit(TransactionData::new_programmable(
        node.sender,
        vec![node.gas()],
        builder.finish(),
        BUDGET,
        RGP,
    ));
    // The first split's argument is a vector, not a u64: the command fails, identically.
    assert!(!effects.status().is_ok(), "{effects:?}");

    let mut builder = ProgrammableTransactionBuilder::new();
    let a = builder.pure(SUI).unwrap();
    let b = builder.pure(2 * SUI).unwrap();
    let split = builder.command(sui_types::transaction::Command::SplitCoins(
        sui_types::transaction::Argument::GasCoin,
        vec![a, b],
    ));
    let sui_types::transaction::Argument::Result(i) = split else {
        unreachable!()
    };
    builder.command(sui_types::transaction::Command::MergeCoins(
        sui_types::transaction::Argument::NestedResult(i, 0),
        vec![sui_types::transaction::Argument::NestedResult(i, 1)],
    ));
    builder.transfer_arg(
        node.sender,
        sui_types::transaction::Argument::NestedResult(i, 0),
    );
    let effects = node.execute_both_and_commit(TransactionData::new_programmable(
        node.sender,
        vec![node.gas()],
        builder.finish(),
        BUDGET,
        RGP,
    ));
    assert!(effects.status().is_ok(), "{effects:?}");
}

// Packages: publish, then calls that create, mutate, wrap, unwrap, delete and freeze objects,
// emit events and use dynamic (object) fields.

use sui_types::transaction::ObjectArg;

impl Node {
    fn live_ref(&self, id: ObjectID) -> ObjectRef {
        execution::StoreView::new(&self.store)
            .live_object(&id)
            .unwrap()
            .unwrap()
            .compute_object_reference()
    }

    fn publish(&self, path: &str) -> ObjectID {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../crates/sui-core/src/unit_tests/data")
            .join(path);
        let package = sui_move_build::BuildConfig::new_for_testing()
            .build(&path)
            .unwrap();
        let modules = package.get_package_bytes(false);
        let deps = package.get_dependency_storage_package_ids();
        let effects = self.execute_both_and_commit(TransactionData::new_module(
            self.sender,
            self.gas(),
            modules,
            deps,
            BUDGET * 10,
            RGP,
        ));
        assert!(effects.status().is_ok(), "{effects:?}");
        effects
            .created()
            .into_iter()
            .find(|(_, owner)| *owner == sui_types::object::Owner::Immutable)
            .unwrap()
            .0
            .0
    }

    /// Calls `examples::object_basics::<function>`; the objects it created.
    fn call(&self, package: ObjectID, function: &str, args: Vec<CallArg>) -> Vec<ObjectID> {
        let mut builder = ProgrammableTransactionBuilder::new();
        builder
            .move_call(
                package,
                ident_str!("object_basics").to_owned(),
                move_core_types::identifier::Identifier::new(function).unwrap(),
                vec![],
                args,
            )
            .unwrap();
        let effects = self.execute_both_and_commit(TransactionData::new_programmable(
            self.sender,
            vec![self.gas()],
            builder.finish(),
            BUDGET,
            RGP,
        ));
        assert!(effects.status().is_ok(), "{function}: {effects:?}");
        effects.created().into_iter().map(|(r, _)| r.0).collect()
    }

    fn owned(&self, id: ObjectID) -> CallArg {
        CallArg::Object(ObjectArg::ImmOrOwnedObject(self.live_ref(id)))
    }
}

#[test]
fn object_basics_match() {
    let node = Node::new();
    let package = node.publish("object_basics");
    macro_rules! pure {
        ($v:expr) => {
            CallArg::Pure(bcs::to_bytes($v).unwrap())
        };
    }

    let a = node.call(package, "create", vec![pure!(&7u64), pure!(&node.sender)])[0];
    let b = node.call(package, "create", vec![pure!(&9u64), pure!(&node.sender)])[0];
    // Reads b, writes a, emits an event.
    node.call(package, "update", vec![node.owned(a), node.owned(b)]);
    node.call(package, "set_value", vec![node.owned(a), pure!(&11u64)]);
    // Wrapped, then unwrapped.
    let wrapper = node.call(package, "wrap", vec![node.owned(b)])[0];
    node.call(package, "unwrap", vec![node.owned(wrapper)]);
    // A dynamic object field, added and removed; then a dynamic field.
    node.call(package, "add_ofield", vec![node.owned(a), node.owned(b)]);
    node.call(package, "remove_ofield", vec![node.owned(a)]);
    node.call(package, "add_field", vec![node.owned(a), node.owned(b)]);
    node.call(package, "remove_field", vec![node.owned(a)]);
    // Shared, frozen, deleted.
    node.call(package, "share", vec![]);
    node.call(package, "freeze_object", vec![node.owned(b)]);
    node.call(package, "delete", vec![node.owned(a)]);
}

fn build(path: &str) -> sui_move_build::CompiledPackage {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../crates/sui-core/src/unit_tests/data")
        .join(path);
    sui_move_build::BuildConfig::new_for_testing()
        .build(&path)
        .unwrap()
}

impl Node {
    /// Calls `<package>::base::return_0`; whether it succeeded.
    fn return_0(&self, package: ObjectID) -> bool {
        let mut builder = ProgrammableTransactionBuilder::new();
        builder
            .move_call(
                package,
                ident_str!("base").to_owned(),
                ident_str!("return_0").to_owned(),
                vec![],
                vec![],
            )
            .unwrap();
        let effects = self.execute_both_and_commit(TransactionData::new_programmable(
            self.sender,
            vec![self.gas()],
            builder.finish(),
            BUDGET,
            RGP,
        ));
        effects.status().is_ok()
    }
}

/// Two versions of a package, called alternately: each ID resolves to its own version.
#[test]
fn upgraded_package_versions_match() {
    use sui_types::transaction::{Argument, Command, ObjectArg};
    let node = Node::new();
    let base = build("move_upgrade/base");
    let effects = node.execute_both_and_commit(TransactionData::new_module(
        node.sender,
        node.gas(),
        base.get_package_bytes(false),
        base.get_dependency_storage_package_ids(),
        BUDGET * 10,
        RGP,
    ));
    assert!(effects.status().is_ok(), "{effects:?}");
    let created = effects.created();
    let v1 = created
        .iter()
        .find(|(_, o)| *o == sui_types::object::Owner::Immutable)
        .unwrap()
        .0
        .0;
    let cap = created
        .iter()
        .find(|(_, o)| *o != sui_types::object::Owner::Immutable)
        .unwrap()
        .0
        .0;
    // Version 1's `return_0` aborts.
    assert!(!node.return_0(v1));

    let upgrade = build("move_upgrade/stage1_basic_compatibility_valid");
    let mut builder = ProgrammableTransactionBuilder::new();
    let cap_arg = builder
        .obj(ObjectArg::ImmOrOwnedObject(node.live_ref(cap)))
        .unwrap();
    let policy = builder.pure(0u8).unwrap();
    let digest = builder
        .pure(upgrade.get_package_digest(false).to_vec())
        .unwrap();
    let ticket = builder.programmable_move_call(
        sui_types::SUI_FRAMEWORK_PACKAGE_ID,
        ident_str!("package").to_owned(),
        ident_str!("authorize_upgrade").to_owned(),
        vec![],
        vec![cap_arg, policy, digest],
    );
    let receipt = builder.command(Command::Upgrade(
        upgrade.get_package_bytes(false),
        upgrade.get_dependency_storage_package_ids(),
        v1,
        ticket,
    ));
    builder.programmable_move_call(
        sui_types::SUI_FRAMEWORK_PACKAGE_ID,
        ident_str!("package").to_owned(),
        ident_str!("commit_upgrade").to_owned(),
        vec![],
        vec![cap_arg, receipt],
    );
    let _: Argument = receipt;
    let effects = node.execute_both_and_commit(TransactionData::new_programmable(
        node.sender,
        vec![node.gas()],
        builder.finish(),
        BUDGET * 10,
        RGP,
    ));
    assert!(effects.status().is_ok(), "{effects:?}");
    let v2 = effects
        .created()
        .into_iter()
        .find(|(_, o)| *o == sui_types::object::Owner::Immutable)
        .unwrap()
        .0
        .0;

    // Version 2's returns; version 1's still aborts.
    assert!(node.return_0(v2));
    assert!(!node.return_0(v1));
    assert!(node.return_0(v2));
}

/// Move calls with type arguments, including ones that fail in loading or typing.
#[test]
#[allow(clippy::too_many_lines, clippy::many_single_char_names)]
fn generic_calls_match() {
    use move_core_types::language_storage::{StructTag, TypeTag};
    use sui_types::transaction::{Argument, Command};
    let node = Node::new();
    let sui = sui_types::gas_coin::GAS::type_tag();
    let framework = sui_types::SUI_FRAMEWORK_PACKAGE_ID;
    let stdlib = sui_types::MOVE_STDLIB_PACKAGE_ID;
    let run = |builder: ProgrammableTransactionBuilder| {
        node.execute_both_and_commit(TransactionData::new_programmable(
            node.sender,
            vec![node.gas()],
            builder.finish(),
            BUDGET,
            RGP,
        ))
    };

    // A zero coin, made and destroyed.
    let mut b = ProgrammableTransactionBuilder::new();
    let coin = b.programmable_move_call(
        framework,
        ident_str!("coin").to_owned(),
        ident_str!("zero").to_owned(),
        vec![sui.clone()],
        vec![],
    );
    b.programmable_move_call(
        framework,
        ident_str!("coin").to_owned(),
        ident_str!("destroy_zero").to_owned(),
        vec![sui.clone()],
        vec![coin],
    );
    assert!(run(b).status().is_ok());

    // Options and vectors of primitives, and a typed vector.
    let mut b = ProgrammableTransactionBuilder::new();
    let t = b.pure(true).unwrap();
    b.programmable_move_call(
        stdlib,
        ident_str!("option").to_owned(),
        ident_str!("some").to_owned(),
        vec![TypeTag::Bool],
        vec![t],
    );
    let bytes = b.pure(vec![1u8, 2, 3]).unwrap();
    b.programmable_move_call(
        stdlib,
        ident_str!("vector").to_owned(),
        ident_str!("length").to_owned(),
        vec![TypeTag::U8],
        vec![bytes],
    );
    let addresses = b.pure(vec![node.sender]).unwrap();
    b.programmable_move_call(
        stdlib,
        ident_str!("option").to_owned(),
        ident_str!("some").to_owned(),
        vec![TypeTag::Vector(Box::new(TypeTag::Address))],
        vec![addresses],
    );
    let (x, y) = (b.pure(1u64).unwrap(), b.pure(2u64).unwrap());
    b.command(Command::MakeMoveVec(Some(TypeTag::U64.into()), vec![x, y]));
    assert!(run(b).status().is_ok());

    // A type argument naming a type that does not exist.
    let mut b = ProgrammableTransactionBuilder::new();
    b.programmable_move_call(
        framework,
        ident_str!("coin").to_owned(),
        ident_str!("zero").to_owned(),
        vec![TypeTag::Struct(Box::new(StructTag {
            address: sui_types::SUI_FRAMEWORK_ADDRESS,
            module: ident_str!("coin").to_owned(),
            name: ident_str!("NoSuchType").to_owned(),
            type_params: vec![],
        }))],
        vec![],
    );
    assert!(!run(b).status().is_ok());

    // The wrong number of type arguments.
    let mut b = ProgrammableTransactionBuilder::new();
    b.programmable_move_call(
        framework,
        ident_str!("coin").to_owned(),
        ident_str!("zero").to_owned(),
        vec![sui.clone(), sui.clone()],
        vec![],
    );
    assert!(!run(b).status().is_ok());

    // A type argument without the abilities the function requires (`key + store`).
    let mut b = ProgrammableTransactionBuilder::new();
    let (v, to) = (b.pure(7u64).unwrap(), b.pure(node.sender).unwrap());
    b.programmable_move_call(
        framework,
        ident_str!("transfer").to_owned(),
        ident_str!("public_transfer").to_owned(),
        vec![TypeTag::U64],
        vec![v, to],
    );
    assert!(!run(b).status().is_ok());

    // Two coins of one type merged: object inputs of the same type.
    let mut b = ProgrammableTransactionBuilder::new();
    let (a1, a2) = (b.pure(SUI).unwrap(), b.pure(SUI).unwrap());
    let split = b.command(Command::SplitCoins(Argument::GasCoin, vec![a1, a2]));
    let Argument::Result(i) = split else {
        unreachable!()
    };
    b.transfer_arg(node.sender, Argument::NestedResult(i, 0));
    b.transfer_arg(node.sender, Argument::NestedResult(i, 1));
    let effects = run(b);
    assert!(effects.status().is_ok());
    let coins: Vec<ObjectID> = effects.created().into_iter().map(|(r, _)| r.0).collect();
    let mut b = ProgrammableTransactionBuilder::new();
    let first = b
        .obj(sui_types::transaction::ObjectArg::ImmOrOwnedObject(
            node.live_ref(coins[0]),
        ))
        .unwrap();
    let second = b
        .obj(sui_types::transaction::ObjectArg::ImmOrOwnedObject(
            node.live_ref(coins[1]),
        ))
        .unwrap();
    b.command(Command::MergeCoins(first, vec![second]));
    assert!(run(b).status().is_ok());
}

// Dynamic fields through the framework's collections: children added, borrowed mutably (a linked
// table's push and pop relink neighbouring nodes), mutated and removed, within one transaction and
// across transactions, and re-added with the same or a different value.

use move_core_types::language_storage::TypeTag;
use sui_types::transaction::{Argument, Command};

impl Node {
    /// A PTB built by `f`, paid with the gas coin; the effects.
    fn ptb(&self, f: impl FnOnce(&mut ProgrammableTransactionBuilder)) -> TransactionEffects {
        let mut builder = ProgrammableTransactionBuilder::new();
        f(&mut builder);
        self.execute_both_and_commit(TransactionData::new_programmable(
            self.sender,
            vec![self.gas()],
            builder.finish(),
            BUDGET,
            RGP,
        ))
    }

    /// The created object whose type's name is `name`.
    fn created_named(&self, effects: &TransactionEffects, name: &str) -> ObjectID {
        let view = execution::StoreView::new(&self.store);
        effects
            .created()
            .into_iter()
            .map(|(r, _)| r.0)
            .find(|id| {
                view.live_object(id)
                    .unwrap()
                    .unwrap()
                    .struct_tag()
                    .is_some_and(|t| t.name.as_str() == name)
            })
            .unwrap()
    }

    fn owned_arg(&self, b: &mut ProgrammableTransactionBuilder, id: ObjectID) -> Argument {
        b.obj(ObjectArg::ImmOrOwnedObject(self.live_ref(id)))
            .unwrap()
    }
}

fn framework(
    b: &mut ProgrammableTransactionBuilder,
    module: &str,
    function: &str,
    type_args: Vec<TypeTag>,
    args: Vec<Argument>,
) -> Argument {
    b.programmable_move_call(
        sui_types::SUI_FRAMEWORK_PACKAGE_ID,
        move_core_types::identifier::Identifier::new(module).unwrap(),
        move_core_types::identifier::Identifier::new(function).unwrap(),
        type_args,
        args,
    )
}

/// `collection`, a key and a value, as a call's arguments.
fn ckv(
    b: &mut ProgrammableTransactionBuilder,
    collection: Argument,
    k: u64,
    v: u64,
) -> Vec<Argument> {
    vec![collection, b.pure(k).unwrap(), b.pure(v).unwrap()]
}

#[test]
fn dynamic_fields_match() {
    let node = Node::new();
    let u64s = || vec![TypeTag::U64, TypeTag::U64];
    let coin = TypeTag::Struct(Box::new(sui_types::gas_coin::GasCoin::type_()));
    let bag_args = || vec![TypeTag::U64, coin.clone()];

    // Created, filled, relinked and partly emptied in one transaction.
    let effects = node.ptb(|b| {
        let lt = framework(b, "linked_table", "new", u64s(), vec![]);
        for (k, v) in [(1, 10), (2, 20), (3, 30)] {
            let args = ckv(b, lt, k, v);
            framework(b, "linked_table", "push_back", u64s(), args);
        }
        framework(b, "linked_table", "pop_front", u64s(), vec![lt]);
        let t = framework(b, "table", "new", u64s(), vec![]);
        for (k, v) in [(1, 100), (2, 200)] {
            let args = ckv(b, t, k, v);
            framework(b, "table", "add", u64s(), args);
        }
        let two = b.pure(2u64).unwrap();
        framework(b, "table", "remove", u64s(), vec![t, two]);
        let bag = framework(b, "object_bag", "new", vec![], vec![]);
        let amount = b.pure(1000u64).unwrap();
        let c = b.command(Command::SplitCoins(Argument::GasCoin, vec![amount]));
        let one = b.pure(1u64).unwrap();
        framework(b, "object_bag", "add", bag_args(), vec![bag, one, c]);
        b.transfer_args(node.sender, vec![lt, t, bag]);
    });
    assert!(effects.status().is_ok(), "{effects:?}");
    let lt = node.created_named(&effects, "LinkedTable");
    let t = node.created_named(&effects, "Table");
    let bag = node.created_named(&effects, "ObjectBag");

    // Stored children loaded, relinked and removed; one removed and re-added unchanged; one new.
    let effects = node.ptb(|b| {
        let lt = node.owned_arg(b, lt);
        let args = ckv(b, lt, 4, 40);
        framework(b, "linked_table", "push_back", u64s(), args);
        let two = b.pure(2u64).unwrap();
        framework(b, "linked_table", "remove", u64s(), vec![lt, two]);
        let three = b.pure(3u64).unwrap();
        framework(b, "linked_table", "contains", u64s(), vec![lt, three]);
        let t = node.owned_arg(b, t);
        let one = b.pure(1u64).unwrap();
        let v = framework(b, "table", "remove", u64s(), vec![t, one]);
        framework(b, "table", "add", u64s(), vec![t, one, v]);
        let args = ckv(b, t, 3, 300);
        framework(b, "table", "add", u64s(), args);
        let bag = node.owned_arg(b, bag);
        let c = framework(b, "object_bag", "remove", bag_args(), vec![bag, one]);
        b.command(Command::MergeCoins(Argument::GasCoin, vec![c]));
    });
    assert!(effects.status().is_ok(), "{effects:?}");

    // Stored children mutated: re-added with new values, relinked at both ends.
    let effects = node.ptb(|b| {
        let t = node.owned_arg(b, t);
        for k in [1u64, 3] {
            let key = b.pure(k).unwrap();
            framework(b, "table", "remove", u64s(), vec![t, key]);
            let args = ckv(b, t, k, k * 1000 + 1);
            framework(b, "table", "add", u64s(), args);
        }
        let lt = node.owned_arg(b, lt);
        framework(b, "linked_table", "pop_front", u64s(), vec![lt]);
        let args = ckv(b, lt, 6, 60);
        framework(b, "linked_table", "push_front", u64s(), args);
        let args = ckv(b, lt, 5, 50);
        framework(b, "linked_table", "push_back", u64s(), args);
    });
    assert!(effects.status().is_ok(), "{effects:?}");

    // An abort after children were loaded and changed.
    let effects = node.ptb(|b| {
        let t = node.owned_arg(b, t);
        let args = ckv(b, t, 4, 400);
        framework(b, "table", "add", u64s(), args);
        let missing = b.pure(99u64).unwrap();
        framework(b, "table", "remove", u64s(), vec![t, missing]);
    });
    assert!(!effects.status().is_ok(), "{effects:?}");

    // Emptied and destroyed.
    let effects = node.ptb(|b| {
        let lt = node.owned_arg(b, lt);
        for _ in 0..3 {
            framework(b, "linked_table", "pop_back", u64s(), vec![lt]);
        }
        framework(b, "linked_table", "destroy_empty", u64s(), vec![lt]);
    });
    assert!(effects.status().is_ok(), "{effects:?}");
}
