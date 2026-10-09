// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Genesis into a fresh store, then transactions executed one after another
//! against its live objects, each committed before the next.

use execution::{Execution, Outcome, genesis};
use move_core_types::ident_str;
use sui_protocol_config::{Chain, ProtocolVersion};
use sui_types::base_types::{ObjectID, ObjectRef, SuiAddress};
use sui_types::crypto::{AccountKeyPair, get_key_pair};
use sui_types::effects::{TransactionEffects, TransactionEffectsAPI};
use sui_types::execution_status::ExecutionStatus;
use sui_types::message_envelope::Message as _;
use sui_types::object::Object;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{CallArg, Transaction, TransactionData};

const RGP: u64 = 1000;
const BUDGET: u64 = 50_000_000;
const SUI: u64 = 1_000_000_000;

struct Node {
    _dir: tempfile::TempDir,
    store: store::Store,
    execution: Execution,
    sender: SuiAddress,
    key: AccountKeyPair,
}

impl Node {
    fn new() -> Node {
        let (sender, key) = get_key_pair::<AccountKeyPair>();
        let execution =
            Execution::new(ProtocolVersion::MAX.as_u64(), Chain::Unknown, 0, 0, RGP).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = store::Store::open(dir.path()).unwrap();
        let objects = genesis::objects(&execution, &[(sender, 10 * SUI)]).unwrap();
        genesis::commit(&store, &objects).unwrap();
        assert!(store.has_genesis().unwrap());
        Node {
            _dir: dir,
            store,
            execution,
            sender,
            key,
        }
    }

    fn live(&self, id: ObjectID) -> Object {
        execution::StoreView::new(&self.store)
            .live_object(&id)
            .unwrap()
            .unwrap()
    }

    /// The sender's gas coin, as of now.
    fn gas(&self) -> ObjectRef {
        let id = ObjectID::derive_id(sui_types::digests::TransactionDigest::genesis_marker(), 0);
        self.live(id).compute_object_reference()
    }

    fn execute(&self, data: TransactionData) -> (Outcome, TransactionEffects) {
        let transaction = Transaction::from_data_and_signer(data, vec![&self.key]);
        let bytes = bcs::to_bytes(transaction.data()).unwrap();
        let outcome = self.execution.execute(&self.store, &bytes).unwrap();
        let effects: TransactionEffects = bcs::from_bytes(&outcome.effects).unwrap();
        (outcome, effects)
    }

    fn execute_and_commit(&self, data: TransactionData) -> TransactionEffects {
        let (outcome, effects) = self.execute(data);
        self.store.commit(outcome.commit).unwrap();
        effects
    }
}

#[test]
fn transfers_execute_one_after_another() {
    let node = Node::new();
    let recipient = SuiAddress::random_for_testing_only();
    let first_gas = node.gas();
    let effects = node.execute_and_commit(TransactionData::new_transfer_sui(
        recipient,
        node.sender,
        Some(SUI),
        first_gas,
        BUDGET,
        RGP,
    ));
    assert_eq!(effects.status(), &ExecutionStatus::Success, "{effects:?}");
    // The gas coin moved on to a new version, which the next transfer spends.
    let second_gas = node.gas();
    assert!(second_gas.1 > first_gas.1);
    assert_eq!(
        effects.gas_object().unwrap().0,
        second_gas,
        "the effects name the new gas version"
    );
    let created = effects.created();
    assert_eq!(created.len(), 1);
    let coin = node.live(created[0].0.0);
    assert_eq!(genesis::balance(&coin), Some(SUI));
    let effects = node.execute_and_commit(TransactionData::new_transfer_sui(
        recipient,
        node.sender,
        Some(SUI),
        second_gas,
        BUDGET,
        RGP,
    ));
    assert_eq!(effects.status(), &ExecutionStatus::Success, "{effects:?}");
    // Its results read back as the reference answers for an executed one.
    let executed = execution::executed(&node.store, &effects.transaction_digest().into_inner())
        .unwrap()
        .unwrap();
    assert_eq!(executed.effects_digest, effects.digest().into_inner());
    assert_eq!(executed.effects, bcs::to_bytes(&effects).unwrap());
    assert!(executed.events.is_none(), "a transfer emits no events");
    let decode = |objects: &[Vec<u8>]| -> Vec<ObjectRef> {
        let mut refs: Vec<ObjectRef> = objects
            .iter()
            .map(|o| {
                bcs::from_bytes::<Object>(o)
                    .unwrap()
                    .compute_object_reference()
            })
            .collect();
        refs.sort();
        refs
    };
    assert_eq!(decode(&executed.input_objects), vec![second_gas]);
    let mut changed: Vec<ObjectRef> = effects
        .all_changed_objects()
        .into_iter()
        .map(|(r, _, _)| r)
        .collect();
    changed.sort();
    assert_eq!(decode(&executed.output_objects), changed);
    assert!(
        execution::executed(&node.store, &[7; 32])
            .unwrap()
            .is_none(),
        "never executed"
    );
}

#[test]
fn a_move_call_reads_the_shared_clock() {
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
    let effects = node.execute_and_commit(TransactionData::new_programmable(
        node.sender,
        vec![node.gas()],
        builder.finish(),
        BUDGET,
        RGP,
    ));
    assert_eq!(effects.status(), &ExecutionStatus::Success, "{effects:?}");
}

#[test]
fn a_failed_transaction_still_pays_for_gas() {
    let node = Node::new();
    let before = genesis::balance(&node.live(node.gas().0)).unwrap();
    // More than the coin holds.
    let effects = node.execute_and_commit(TransactionData::new_transfer_sui(
        SuiAddress::random_for_testing_only(),
        node.sender,
        Some(100 * SUI),
        node.gas(),
        BUDGET,
        RGP,
    ));
    assert!(!effects.status().is_ok(), "{effects:?}");
    let after = genesis::balance(&node.live(node.gas().0)).unwrap();
    assert!(after < before, "{before} -> {after}");
    assert!(effects.created().is_empty());
}

#[test]
#[should_panic(expected = "equivocation")]
fn spending_a_consumed_version_is_equivocation() {
    let node = Node::new();
    let gas = node.gas();
    let transfer = |gas| {
        TransactionData::new_transfer_sui(
            SuiAddress::random_for_testing_only(),
            node.sender,
            Some(SUI),
            gas,
            BUDGET,
            RGP,
        )
    };
    node.execute_and_commit(transfer(gas));
    node.execute(transfer(gas));
}
