// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The system transactions the pipeline does not make yet, through anchovy's executor and sui's
//! over a genesis with a system state: their outcomes must be identical.

mod genesis;

use std::sync::Arc;

use execution::native::NativeExecution;
use execution::{Outcome, Reference, StoreView};
use fastcrypto::traits::KeyPair as _;
use fastcrypto_zkp::bn254::zk_login::{JWK, JwkId};
use sui_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use sui_types::authenticator_state::ActiveJwk;
use sui_types::base_types::{ObjectID, SequenceNumber, SuiAddress};
use sui_types::effects::{TransactionEffects, TransactionEffectsAPI};
use sui_types::execution_status::ExecutionStatus;
use sui_types::metrics::ExecutionMetrics;
use sui_types::object::{Object, Owner};
use sui_types::transaction::{EndOfEpochTransactionKind, VerifiedTransaction};

const RGP: u64 = 1000;

struct Node {
    _dir: tempfile::TempDir,
    store: store::Store,
    reference: Reference,
    native: NativeExecution,
}

impl Node {
    /// A genesis store, but without the objects `leave_out` names.
    fn new(leave_out: impl Fn(&Object) -> bool) -> Node {
        let genesis = genesis::build(SuiAddress::ZERO, 1, 1_000_000_000, 60_000);
        let objects: Vec<Object> = genesis
            .objects
            .into_iter()
            .filter(|o| !leave_out(o))
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let store = store::Store::open(dir.path()).unwrap();
        execution::genesis::commit(&store, &objects).unwrap();
        let reference =
            Reference::new(ProtocolVersion::MAX.as_u64(), Chain::Unknown, 0, 0, RGP).unwrap();
        let native = NativeExecution::new(
            ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Unknown),
            Arc::new(ExecutionMetrics::new(&prometheus::Registry::new())),
            0,
            0,
            RGP,
        )
        .unwrap();
        Node {
            _dir: dir,
            store,
            reference,
            native,
        }
    }

    fn live(&self, id: ObjectID) -> Option<Object> {
        StoreView::new(&self.store).live_object(&id).unwrap()
    }

    fn initial_shared_version(&self, id: ObjectID) -> SequenceNumber {
        match self.live(id).unwrap().owner.clone() {
            Owner::Shared {
                initial_shared_version,
            } => initial_shared_version,
            owner => panic!("{id} is {owner:?}"),
        }
    }

    /// Both executors on `transaction`; once they agree, its effects, committed.
    fn execute(&self, transaction: &VerifiedTransaction) -> TransactionEffects {
        let bytes = bcs::to_bytes(transaction.data()).unwrap();
        let reference: Outcome = self.reference.execute(&self.store, &bytes).unwrap();
        let native = self.native.execute_bytes(&self.store, &bytes).unwrap();
        execution::assert_same(&reference, &native);
        let effects: TransactionEffects = bcs::from_bytes(&reference.effects).unwrap();
        assert_eq!(effects.status(), &ExecutionStatus::Success, "{effects:?}");
        self.store.write_through(reference.commit).unwrap();
        effects
    }
}

/// Every system package, as a `ChangeEpoch` carries them, at `version`.
fn system_packages(
    version: impl Fn(ObjectID) -> SequenceNumber,
) -> Vec<(SequenceNumber, Vec<Vec<u8>>, Vec<ObjectID>)> {
    sui_framework::BuiltInFramework::iter_system_packages()
        .map(|p| (version(p.id), p.bytes.clone(), p.dependencies.clone()))
        .collect()
}

fn change_epoch(
    system_packages: Vec<(SequenceNumber, Vec<Vec<u8>>, Vec<ObjectID>)>,
) -> EndOfEpochTransactionKind {
    EndOfEpochTransactionKind::new_change_epoch(
        1,
        ProtocolVersion::MAX,
        1_000,
        2_000,
        500,
        10,
        60_000,
        system_packages,
    )
}

#[test]
fn an_epoch_change_upgrading_every_system_package_matches() {
    let node = Node::new(|_| false);
    let next = |id| node.live(id).unwrap().version().next();
    let effects = node.execute(&VerifiedTransaction::new_end_of_epoch_transaction(vec![
        change_epoch(system_packages(next)),
    ]));
    for id in sui_framework::BuiltInFramework::all_package_ids() {
        assert!(
            effects.mutated().iter().any(|(r, _)| r.0 == id),
            "{id} is not upgraded"
        );
    }
}

#[test]
fn a_legacy_change_epoch_matches() {
    let node = Node::new(|_| false);
    node.execute(&VerifiedTransaction::new_change_epoch(
        1,
        ProtocolVersion::MAX,
        1_000,
        2_000,
        500,
        10,
        60_000,
        vec![],
    ));
}

#[test]
fn a_new_system_package_is_published_as_the_reference_publishes_it() {
    let bridge = ObjectID::from_single_byte(0xb);
    let node = Node::new(|o| o.id() == bridge);
    assert!(node.live(bridge).is_none());
    let package = sui_framework::BuiltInFramework::get_package_by_id(&bridge);
    let effects = node.execute(&VerifiedTransaction::new_end_of_epoch_transaction(vec![
        change_epoch(vec![(
            SequenceNumber::from_u64(1),
            package.bytes.clone(),
            package.dependencies.clone(),
        )]),
    ]));
    assert!(effects.created().iter().any(|(r, _)| r.0 == bridge));
}

#[test]
fn authenticator_state_transactions_match() {
    let node = Node::new(|_| false);
    let state = ObjectID::from_single_byte(0x7);
    let version = node.initial_shared_version(state);
    let jwk = ActiveJwk {
        jwk_id: JwkId::new("https://accounts.google.com".to_owned(), "key".to_owned()),
        jwk: JWK {
            kty: "RSA".to_owned(),
            e: "AQAB".to_owned(),
            n: "abcd".to_owned(),
            alg: "RS256".to_owned(),
        },
        epoch: 0,
    };
    node.execute(&VerifiedTransaction::new_authenticator_state_update(
        0,
        1,
        vec![jwk],
        version,
    ));
    node.execute(&VerifiedTransaction::new_end_of_epoch_transaction(vec![
        EndOfEpochTransactionKind::new_authenticator_state_expire(1, version),
        change_epoch(vec![]),
    ]));
}

#[test]
fn end_of_epoch_writes_to_the_system_state_match() {
    use sui_protocol_config::PerObjectCongestionControlMode;
    use sui_types::execution::ExecutionTimeObservationKey;
    use sui_types::transaction::StoredExecutionTimeObservations;

    let node = Node::new(|_| false);
    let config = ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Unknown);
    let PerObjectCongestionControlMode::ExecutionTimeEstimate(params) =
        config.per_object_congestion_control_mode()
    else {
        panic!("the protocol stores no execution time observations");
    };
    let chunk_size = params.observations_chunk_size.unwrap();
    let authority = genesis::protocol_key_pair().public().into();
    // More entries than a chunk holds, so that they are stored in two.
    let observations = StoredExecutionTimeObservations::V1(
        (0..chunk_size + 2)
            .map(|i| {
                let key = ExecutionTimeObservationKey::MoveEntryPoint {
                    package: ObjectID::from_single_byte(2),
                    module: "coin".to_owned(),
                    function: format!("f{i}"),
                    type_arguments: vec![],
                };
                (key, vec![(authority, std::time::Duration::from_millis(i))])
            })
            .collect(),
    );
    let mut transactions =
        vec![EndOfEpochTransactionKind::new_store_execution_time_observations(observations)];
    if config.enable_accumulators() {
        transactions.push(EndOfEpochTransactionKind::new_write_accumulator_storage_cost(1_234));
    }
    transactions.push(change_epoch(vec![]));
    node.execute(&VerifiedTransaction::new_end_of_epoch_transaction(
        transactions,
    ));
}
