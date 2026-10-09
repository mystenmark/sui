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
