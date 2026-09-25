// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Built values that reach every variant and optional field of every root
//! type, which the mainnet corpus does not. Each goes through `bcs`, the
//! parser and the mirror, and what the view derives is checked against what
//! was built.

use std::collections::BTreeMap;
use std::fmt::Debug;

use messages::base::{Digest, ObjectId, ObjectRef, U64Le};
use messages::build::{
    base, checkpoint as ck, effects as fx, execution_status as st, object as ob, signature as sg,
    system_transaction as sys, transaction as tx, type_tag as ty,
};
use messages::checkpoint::{
    CertifiedCheckpointSummary, CheckpointContents, CheckpointData, CheckpointSummary,
    FullCheckpointContents,
};
use messages::effects::{ChangeKind, TransactionEffects, TransactionEvents, VersionedEffects};
use messages::object::Data as ViewData;
use messages::object::Object;
use messages::signature::{MultiSig, MultiSigPublicKey};
use messages::transaction::{
    SenderSignedData, SharedObjectArg, SharedObjectMutability, TransactionData,
};
use messages::tx_index::{
    SUI_AUTHENTICATOR_STATE_OBJECT_ID, SUI_BRIDGE_OBJECT_ID, SUI_CLOCK_OBJECT_ID,
    SUI_RANDOMNESS_STATE_OBJECT_ID, SUI_SYSTEM_STATE_OBJECT_ID, TransactionIndex,
};
use messages::type_tag::TypeTag;
use messages::{Message, ParseError, Wire};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// `bcs` and the parser must agree on the bytes of `built`, and the view
/// must convert back to it.
fn check<T, B>(built: &B) -> Message<T>
where
    T: Wire,
    B: Serialize + DeserializeOwned + PartialEq + Debug + for<'a, 'v> From<&'a T::View<'v>>,
{
    let bytes = bcs::to_bytes(built).unwrap();
    assert_eq!(bcs::from_bytes::<B>(&bytes).unwrap(), *built);
    let message = Message::<T>::parse(bytes).unwrap_or_else(|(e, _)| panic!("{e}"));
    assert_eq!(B::from(message.get()), *built);
    message
}

fn expect_digest(type_name: &str, bytes: &[u8], digest: Digest) {
    assert_eq!(digest, Digest::of(type_name, bytes), "{type_name}");
}

fn bytes32(n: u8) -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes[31] = n;
    bytes
}

fn id(n: u8) -> base::ObjectId {
    base::ObjectId(base::AccountAddress(bytes32(n)))
}

fn address(n: u8) -> base::SuiAddress {
    base::SuiAddress(bytes32(n))
}

fn digest(n: u8) -> base::Digest {
    base::Digest([n; 32])
}

fn object_ref(n: u8) -> base::ObjectRef {
    (
        id(n),
        base::SequenceNumber(u64::from(n)),
        base::ObjectDigest(digest(n)),
    )
}

/// An address balance reservation: the digest ends in twenty `0xac` bytes.
fn reservation_ref(n: u8) -> base::ObjectRef {
    let mut bytes = [0xac; 32];
    bytes[..12].copy_from_slice(&[n; 12]);
    (
        id(n),
        base::SequenceNumber(0),
        base::ObjectDigest(base::Digest(bytes)),
    )
}

fn view_ref(r: &base::ObjectRef) -> ObjectRef {
    ObjectRef {
        id: ObjectId(r.0.0.0),
        version: U64Le::new(r.1.0),
        digest: Digest::new(r.2.0.0),
    }
}

fn authority(n: u8) -> base::AuthorityPublicKeyBytes {
    base::AuthorityPublicKeyBytes([n; 96])
}

fn struct_tag(n: u8, type_params: Vec<ty::TypeTag>) -> ty::StructTag {
    ty::StructTag {
        address: base::AccountAddress(bytes32(n)),
        module: "m".into(),
        name: "S".into(),
        type_params,
    }
}

/// Every `TypeTag` variant, nested three deep, naming packages 0x30 and 0x31.
fn nested_tag() -> ty::TypeTag {
    use ty::TypeTag as T;
    let inner = T::Struct(Box::new(struct_tag(0x31, vec![])));
    let params = vec![
        T::Bool,
        T::U8,
        T::U16,
        T::U32,
        T::U64,
        T::U128,
        T::U256,
        T::Address,
        T::Signer,
        T::Vector(Box::new(inner)),
    ];
    T::Vector(Box::new(T::Struct(Box::new(struct_tag(0x30, params)))))
}

/// The same wire type under its other name.
fn nested_input() -> ty::TypeInput {
    bcs::from_bytes(&bcs::to_bytes(&nested_tag()).unwrap()).unwrap()
}

fn owners() -> Vec<ob::Owner> {
    vec![
        ob::Owner::AddressOwner(address(1)),
        ob::Owner::ObjectOwner(address(2)),
        ob::Owner::Shared {
            initial_shared_version: base::SequenceNumber(3),
        },
        ob::Owner::Immutable,
        ob::Owner::ConsensusAddressOwner {
            start_version: base::SequenceNumber(4),
            owner: address(4),
        },
        ob::Owner::Party {
            start_version: base::SequenceNumber(5),
            permissions: ob::RawPartySerde {
                default_permissions: 1,
                members: vec![(address(5), 7), (address(6), 3)],
            },
        },
    ]
}

fn signatures() -> Vec<sg::GenericSignature> {
    vec![
        sg::GenericSignature(vec![0; 97]),
        sg::GenericSignature(vec![]),
    ]
}

fn gas_summary() -> fx::GasCostSummary {
    fx::GasCostSummary {
        computation_cost: 1,
        storage_cost: 2,
        storage_rebate: 3,
        non_refundable_storage_fee: 4,
    }
}

// Transactions.

fn move_call(package: u8, type_arguments: Vec<ty::TypeInput>) -> tx::Command {
    use tx::Argument as A;
    tx::Command::MoveCall(Box::new(tx::ProgrammableMoveCall {
        package: id(package),
        module: "m".into(),
        function: "f".into(),
        type_arguments,
        arguments: vec![A::GasCoin, A::Input(1), A::Result(0), A::NestedResult(0, 1)],
    }))
}

/// Every input and command variant. Packages named: 0x20 to 0x22 by calls
/// and the upgrade, 0x30 and 0x31 by type arguments, 0x40 and 0x41 as
/// dependencies, 0x20 twice.
fn programmable() -> tx::ProgrammableTransaction {
    use tx::{Argument as A, CallArg, Command, ObjectArg, SharedObjectMutability as M};
    let shared = |n, mutability| {
        CallArg::Object(ObjectArg::SharedObject {
            id: id(n),
            initial_shared_version: base::SequenceNumber(u64::from(n)),
            mutability,
        })
    };
    let withdrawal = |withdraw_from| {
        CallArg::FundsWithdrawal(tx::FundsWithdrawalArg {
            reservation: tx::Reservation::MaxAmountU64(9),
            type_arg: tx::WithdrawalTypeArg::Balance(nested_tag()),
            withdraw_from,
        })
    };
    let modules = vec![vec![0xa1, 0xa2], vec![]];
    tx::ProgrammableTransaction {
        inputs: vec![
            CallArg::Pure(vec![1, 2, 3]),
            CallArg::Object(ObjectArg::ImmOrOwnedObject(object_ref(0x10))),
            CallArg::Object(ObjectArg::ImmOrOwnedObject(reservation_ref(0x11))),
            shared(0x12, M::Immutable),
            shared(0x13, M::Mutable),
            shared(0x14, M::NonExclusiveWrite),
            CallArg::Object(ObjectArg::Receiving(object_ref(0x15))),
            withdrawal(tx::WithdrawFrom::Sender),
            withdrawal(tx::WithdrawFrom::Sponsor),
            withdrawal(tx::WithdrawFrom::SenderAllowance {
                funder: address(0x16),
                allowance: id(0x17),
            }),
        ],
        commands: vec![
            move_call(0x20, vec![nested_input(), ty::TypeInput::U64]),
            Command::TransferObjects(vec![A::Input(1), A::Result(0)], A::Input(0)),
            Command::SplitCoins(A::GasCoin, vec![A::Input(0), A::Input(0)]),
            Command::MergeCoins(A::Input(1), vec![A::NestedResult(2, 0)]),
            Command::Publish(modules.clone(), vec![id(0x40), id(0x20)]),
            Command::MakeMoveVec(Some(nested_input()), vec![A::Result(4)]),
            Command::MakeMoveVec(None, vec![]),
            Command::Upgrade(modules, vec![id(0x41)], id(0x21), A::Result(3)),
            move_call(0x22, vec![]),
        ],
    }
}

fn expiration(n: usize) -> tx::TransactionExpiration {
    use tx::TransactionExpiration as E;
    let chain = base::ChainIdentifier(base::CheckpointDigest(digest(0x74)));
    match n {
        0 => E::None,
        1 => E::Epoch(7),
        2 => E::ValidDuring {
            min_epoch: Some(1),
            max_epoch: None,
            min_timestamp: None,
            max_timestamp: Some(2),
            chain,
            nonce: 3,
        },
        _ => E::Validity {
            min_epoch: None,
            max_epoch: Some(4),
            min_timestamp: Some(5),
            max_timestamp: None,
            chain,
            nonce: 6,
            allowed_proposers: (n == 3).then(|| tx::AllowedProposers {
                epoch: 8,
                proposers: vec![0, 3, 9],
            }),
        },
    }
}

fn transaction(kind: tx::TransactionKind, expiration: usize) -> tx::TransactionData {
    tx::TransactionData::V1(tx::TransactionDataV1 {
        kind,
        sender: address(0x63),
        gas_data: tx::GasData {
            payment: vec![object_ref(0x60), reservation_ref(0x61)],
            owner: address(0x62),
            price: 1000,
            budget: 5_000_000,
        },
        expiration: self::expiration(expiration),
    })
}

fn signed(kind: tx::TransactionKind, expiration: usize) -> tx::Transaction {
    tx::Transaction {
        data: tx::SenderSignedData(tx::SenderSignedTransaction {
            intent_message: tx::IntentMessage {
                intent: tx::Intent {
                    scope: 0,
                    version: 0,
                    app_id: 0,
                },
                value: transaction(kind, expiration),
            },
            tx_signatures: signatures(),
        }),
        auth_signature: sg::EmptySignInfo {},
    }
}

fn authenticator_update() -> tx::TransactionKind {
    tx::TransactionKind::AuthenticatorStateUpdate(sys::AuthenticatorStateUpdate {
        epoch: 1,
        round: 2,
        new_active_jwks: vec![sys::ActiveJwk {
            jwk_id: sys::JwkId {
                iss: "iss".into(),
                kid: "kid".into(),
            },
            jwk: sys::Jwk {
                kty: "RSA".into(),
                e: "AQAB".into(),
                n: "n".into(),
                alg: "RS256".into(),
            },
            epoch: 3,
        }],
        authenticator_obj_initial_shared_version: base::SequenceNumber(21),
    })
}

fn change_epoch() -> sys::ChangeEpoch {
    sys::ChangeEpoch {
        epoch: 7,
        protocol_version: base::ProtocolVersion(70),
        storage_charge: 1,
        computation_charge: 2,
        storage_rebate: 3,
        non_refundable_storage_fee: 4,
        epoch_start_timestamp_ms: 5,
        system_packages: vec![
            (
                base::SequenceNumber(8),
                vec![vec![1, 2, 3]],
                vec![id(1), id(2)],
            ),
            (base::SequenceNumber(9), vec![], vec![]),
        ],
    }
}

fn assignments(v2: bool) -> sys::ConsensusDeterminedVersionAssignments {
    use sys::ConsensusDeterminedVersionAssignments as V;
    let tx = base::TransactionDigest(digest(0x70));
    let (key, version) = ((id(1), base::SequenceNumber(2)), base::SequenceNumber(3));
    if v2 {
        V::CancelledTransactionsV2(vec![(tx, vec![(key, version)]), (tx, vec![])])
    } else {
        V::CancelledTransactions(vec![(tx, vec![key, key])])
    }
}

fn prologue_v4(sub_dag_index: Option<u64>, v2: bool) -> tx::TransactionKind {
    tx::TransactionKind::ConsensusCommitPrologueV4(sys::ConsensusCommitPrologueV4 {
        epoch: 1,
        round: 2,
        sub_dag_index,
        commit_timestamp_ms: 3,
        consensus_commit_digest: base::ConsensusCommitDigest(digest(0x72)),
        consensus_determined_version_assignments: assignments(v2),
        additional_state_digest: base::AdditionalConsensusStateDigest(digest(0x73)),
    })
}

fn observations() -> sys::StoredExecutionTimeObservations {
    use sys::ExecutionTimeObservationKey as K;
    let duration = |secs, nanos| sys::Duration { secs, nanos };
    sys::StoredExecutionTimeObservations::V1(vec![
        (
            K::MoveEntryPoint {
                package: id(0x20),
                module: "m".into(),
                function: "f".into(),
                type_arguments: vec![nested_input()],
            },
            vec![(authority(1), duration(1, 2))],
        ),
        (K::TransferObjects, vec![]),
        (K::SplitCoins, vec![(authority(2), duration(0, 0))]),
        (K::MergeCoins, vec![]),
        (K::Publish, vec![]),
        (K::MakeMoveVec, vec![]),
        (K::Upgrade, vec![]),
    ])
}

fn end_of_epoch_kinds() -> Vec<sys::EndOfEpochTransactionKind> {
    use sys::EndOfEpochTransactionKind as K;
    vec![
        K::ChangeEpoch(change_epoch()),
        K::AuthenticatorStateCreate,
        K::AuthenticatorStateExpire(sys::AuthenticatorStateExpire {
            min_epoch: 3,
            authenticator_obj_initial_shared_version: base::SequenceNumber(11),
        }),
        K::RandomnessStateCreate,
        K::DenyListStateCreate,
        K::BridgeStateCreate(base::ChainIdentifier(base::CheckpointDigest(digest(0x71)))),
        K::BridgeCommitteeInit(base::SequenceNumber(12)),
        K::StoreExecutionTimeObservations(observations()),
        K::AccumulatorRootCreate,
        K::CoinRegistryCreate,
        K::DisplayRegistryCreate,
        K::AddressAliasStateCreate,
        K::WriteAccumulatorStorageCost(sys::WriteAccumulatorStorageCost { storage_cost: 13 }),
        K::ForwardingAddressRegistryCreate,
    ]
}

/// Every system kind with the shared inputs its index must name.
fn system_kinds() -> Vec<(tx::TransactionKind, Vec<SharedObjectArg>)> {
    use tx::TransactionKind as K;
    let shared = |id, version| SharedObjectArg::new(id, version, SharedObjectMutability::Mutable);
    let system = shared(SUI_SYSTEM_STATE_OBJECT_ID, 1);
    let clock = vec![shared(SUI_CLOCK_OBJECT_ID, 1)];
    vec![
        (K::ChangeEpoch(change_epoch()), vec![system]),
        (
            K::Genesis(sys::GenesisTransaction {
                objects: genesis_objects(),
            }),
            vec![],
        ),
        (
            K::ConsensusCommitPrologue(sys::ConsensusCommitPrologue {
                epoch: 1,
                round: 2,
                commit_timestamp_ms: 3,
            }),
            clock.clone(),
        ),
        (
            authenticator_update(),
            vec![shared(SUI_AUTHENTICATOR_STATE_OBJECT_ID, 21)],
        ),
        (
            K::EndOfEpochTransaction(end_of_epoch_kinds()),
            vec![
                system,
                shared(SUI_AUTHENTICATOR_STATE_OBJECT_ID, 11),
                shared(SUI_BRIDGE_OBJECT_ID, 12),
                system,
                system,
                system,
            ],
        ),
        (
            K::RandomnessStateUpdate(sys::RandomnessStateUpdate {
                epoch: 1,
                randomness_round: base::RandomnessRound(2),
                random_bytes: vec![9; 48],
                randomness_obj_initial_shared_version: base::SequenceNumber(22),
            }),
            vec![shared(SUI_RANDOMNESS_STATE_OBJECT_ID, 22)],
        ),
        (
            K::ConsensusCommitPrologueV2(sys::ConsensusCommitPrologueV2 {
                epoch: 1,
                round: 2,
                commit_timestamp_ms: 3,
                consensus_commit_digest: base::ConsensusCommitDigest(digest(0x72)),
            }),
            clock.clone(),
        ),
        (
            K::ConsensusCommitPrologueV3(sys::ConsensusCommitPrologueV3 {
                epoch: 1,
                round: 2,
                sub_dag_index: Some(4),
                commit_timestamp_ms: 3,
                consensus_commit_digest: base::ConsensusCommitDigest(digest(0x72)),
                consensus_determined_version_assignments: assignments(false),
            }),
            clock.clone(),
        ),
        (prologue_v4(None, true), clock.clone()),
        (prologue_v4(Some(5), false), clock),
    ]
}

fn check_programmable_index(index: &TransactionIndex<'_>, user: bool) {
    use SharedObjectMutability as M;
    let shared = |n: u8, m| SharedObjectArg::new(ObjectId(bytes32(n)), u64::from(n), m);
    assert_eq!(
        index.shared_inputs,
        [
            shared(0x12, M::Immutable),
            shared(0x13, M::Mutable),
            shared(0x14, M::NonExclusiveWrite)
        ]
    );
    assert_eq!(
        index.packages,
        [0x20, 0x21, 0x22, 0x30, 0x31, 0x40, 0x41].map(|n| ObjectId(bytes32(n)))
    );
    let owned = view_ref(&object_ref(0x10));
    if user {
        assert_eq!(index.owned_inputs, [owned, view_ref(&object_ref(0x60))]);
        assert_eq!(index.receiving, [view_ref(&object_ref(0x15))]);
        assert_eq!(index.move_calls, [0, 8]);
        assert_eq!(index.funds_withdrawals.len(), 3);
        assert_eq!(
            index.coin_reservations,
            [
                view_ref(&reservation_ref(0x11)),
                view_ref(&reservation_ref(0x61))
            ]
        );
    } else {
        assert_eq!(index.owned_inputs, [owned]);
        assert!(index.receiving.is_empty() && index.move_calls.is_empty());
        assert!(index.funds_withdrawals.is_empty());
        // A reservation in the gas payment is indexed for every kind.
        assert_eq!(index.coin_reservations, [view_ref(&reservation_ref(0x61))]);
    }
}

#[test]
fn programmable_transactions() {
    for user in [true, false] {
        let kind = if user {
            tx::TransactionKind::ProgrammableTransaction(programmable())
        } else {
            tx::TransactionKind::ProgrammableSystemTransaction(programmable())
        };
        let built = transaction(kind, 3);
        let message = check::<TransactionData, tx::TransactionData>(&built);
        let data = message.get();
        expect_digest("TransactionData", data.bytes, *data.digest());
        check_programmable_index(&data.index, user);
        let calls: Vec<usize> = data.move_calls().map(|(i, _)| i).collect();
        assert_eq!(calls, if user { vec![0, 8] } else { vec![] });
    }
}

#[test]
fn system_transactions() {
    for (kind, shared) in system_kinds() {
        let built = transaction(kind, 0);
        let message = check::<TransactionData, tx::TransactionData>(&built);
        let index = &message.get().index;
        assert_eq!(index.shared_inputs, shared, "{built:?}");
        assert!(index.owned_inputs.is_empty() && index.packages.is_empty());
        assert!(index.receiving.is_empty() && index.move_calls.is_empty());
        assert!(index.funds_withdrawals.is_empty());
        assert_eq!(index.coin_reservations, [view_ref(&reservation_ref(0x61))]);
    }
}

/// Every expiration, on a signed transaction.
#[test]
fn expirations_and_sender_signed_data() {
    for n in 0..5 {
        let built = signed(
            tx::TransactionKind::ProgrammableTransaction(programmable()),
            n,
        )
        .data;
        let message = check::<SenderSignedData, tx::SenderSignedData>(&built);
        let data = message.get();
        assert_eq!(
            *data.digest(),
            Digest::of("TransactionData", data.data.bytes)
        );
        assert_eq!(data.tx_signatures.len(), 2);
    }
    // The envelope adds no bytes, so it parses as the data it wraps.
    let envelope = signed(tx::TransactionKind::ChangeEpoch(change_epoch()), 1);
    check::<SenderSignedData, tx::Transaction>(&envelope);
}

#[test]
fn sender_signed_data_must_hold_one_transaction() {
    let body = &bcs::to_bytes(&signed(prologue_v4(None, false), 0).data).unwrap()[1..];
    let mut two = vec![2];
    two.extend_from_slice(body);
    two.extend_from_slice(body);
    for bytes in [vec![0], two] {
        assert!(bcs::from_bytes::<tx::SenderSignedData>(&bytes).is_err());
        let (e, _) = Message::<SenderSignedData>::parse(bytes).unwrap_err();
        assert_eq!(e, ParseError::NotOneTransaction);
    }
}

// Effects.

fn effects_v1() -> fx::TransactionEffects {
    let mut owners = owners().into_iter();
    let mut owned = |n| (object_ref(n), owners.next().unwrap());
    fx::TransactionEffects::V1(Box::new(fx::TransactionEffectsV1 {
        status: st::ExecutionStatus::Failure(st::ExecutionFailure {
            error: st::ExecutionErrorKind::InsufficientGas,
            command: None,
        }),
        executed_epoch: 3,
        gas_used: gas_summary(),
        modified_at_versions: vec![(id(1), base::SequenceNumber(2))],
        shared_objects: vec![object_ref(3)],
        transaction_digest: base::TransactionDigest(digest(4)),
        created: vec![owned(5), owned(6)],
        mutated: vec![owned(7)],
        unwrapped: vec![owned(8)],
        deleted: vec![object_ref(9)],
        unwrapped_then_deleted: vec![object_ref(10)],
        wrapped: vec![object_ref(11)],
        gas_object: owned(12),
        events_digest: Some(base::TransactionEventsDigest(digest(13))),
        dependencies: vec![base::TransactionDigest(digest(14))],
    }))
}

/// Every `(ObjectIn, ObjectOut, IDOperation)` combination with the class the
/// reference's accessors give it, then an accumulator write of each
/// operation and value. Outputs and operations are indexed in wire order.
fn every_change() -> Vec<(fx::EffectsObjectChange, ChangeKind)> {
    use ChangeKind as C;
    use fx::{IdOperation as I, ObjectIn, ObjectOut as O};
    let accumulator = |n: usize, value| {
        O::AccumulatorWriteV1(fx::AccumulatorWriteV1 {
            address: fx::AccumulatorAddress {
                address: address(0x40),
                ty: nested_tag(),
            },
            operation: [
                fx::AccumulatorOperation::Merge,
                fx::AccumulatorOperation::Split,
            ][n % 2],
            value,
        })
    };
    let outputs = [
        O::NotExist,
        O::ObjectWrite((base::ObjectDigest(digest(2)), ob::Owner::Immutable)),
        O::PackageWrite((base::SequenceNumber(3), base::ObjectDigest(digest(3)))),
        accumulator(0, fx::AccumulatorValue::Integer(1)),
        accumulator(1, fx::AccumulatorValue::IntegerTuple(1, 2)),
        accumulator(
            2,
            fx::AccumulatorValue::EventDigest(vec![(0, digest(5)), (1, digest(6))]),
        ),
    ];
    let inputs = [
        ObjectIn::NotExist,
        ObjectIn::Exist((
            (base::SequenceNumber(1), base::ObjectDigest(digest(1))),
            ob::Owner::AddressOwner(address(1)),
        )),
    ];
    let operations = [I::None, I::Created, I::Deleted];
    // Rows are (input, output, operation) indices.
    let table = [
        (0, 0, 0, C::Unclassified),
        (0, 0, 1, C::Transient),
        (0, 0, 2, C::UnwrappedThenDeleted),
        (0, 1, 0, C::Unwrapped),
        (0, 1, 1, C::Created),
        (0, 1, 2, C::Unclassified),
        (0, 2, 0, C::Unclassified),
        (0, 2, 1, C::Created),
        (0, 2, 2, C::Unclassified),
        (1, 0, 0, C::Wrapped),
        (1, 0, 1, C::Unclassified),
        (1, 0, 2, C::Deleted),
        (1, 1, 0, C::Mutated),
        (1, 1, 1, C::Mutated),
        (1, 1, 2, C::Mutated),
        (1, 2, 0, C::Mutated),
        (1, 2, 1, C::Mutated),
        (1, 2, 2, C::Mutated),
        (1, 3, 0, C::AccumulatorWrite),
        (0, 4, 1, C::AccumulatorWrite),
        (0, 5, 2, C::AccumulatorWrite),
    ];
    table
        .into_iter()
        .map(|(input, output, operation, kind)| {
            let change = fx::EffectsObjectChange {
                input_state: inputs[input].clone(),
                output_state: outputs[output].clone(),
                id_operation: operations[operation],
            };
            (change, kind)
        })
        .collect()
}

fn effects_v2(
    status: st::ExecutionStatus,
    some: bool,
) -> (fx::TransactionEffects, Vec<ChangeKind>) {
    use fx::UnchangedConsensusKind as U;
    let (changes, kinds): (Vec<_>, Vec<_>) = every_change().into_iter().unzip();
    let effects = fx::TransactionEffectsV2 {
        status,
        executed_epoch: 9,
        gas_used: gas_summary(),
        transaction_digest: base::TransactionDigest(digest(0x75)),
        gas_object_index: some.then_some(3),
        events_digest: some.then(|| base::TransactionEventsDigest(digest(0x76))),
        dependencies: vec![base::TransactionDigest(digest(0x77))],
        lamport_version: base::SequenceNumber(99),
        changed_objects: changes
            .into_iter()
            .enumerate()
            .map(|(i, c)| (id(u8::try_from(i).unwrap()), c))
            .collect(),
        unchanged_consensus_objects: vec![
            (
                id(0x80),
                U::ReadOnlyRoot((base::SequenceNumber(1), base::ObjectDigest(digest(1)))),
            ),
            (
                id(0x81),
                U::MutateConsensusStreamEnded(base::SequenceNumber(2)),
            ),
            (
                id(0x82),
                U::ReadConsensusStreamEnded(base::SequenceNumber(3)),
            ),
            (id(0x83), U::Cancelled(base::SequenceNumber(4))),
            (id(0x84), U::PerEpochConfig),
        ],
        aux_data_digest: some.then(|| base::EffectsAuxDataDigest(digest(0x85))),
    };
    (fx::TransactionEffects::V2(Box::new(effects)), kinds)
}

fn location(named: bool) -> st::MoveLocation {
    st::MoveLocation {
        module: st::ModuleId {
            address: base::AccountAddress(bytes32(0x20)),
            name: "m".into(),
        },
        function: 1,
        instruction: 2,
        function_name: named.then(|| "f".into()),
    }
}

/// Variants 0 to 20, in order.
fn early_errors() -> Vec<st::ExecutionErrorKind> {
    use st::ExecutionErrorKind as E;
    vec![
        E::InsufficientGas,
        E::InvalidGasObject,
        E::InvariantViolation,
        E::FeatureNotYetSupported,
        E::MoveObjectTooBig {
            object_size: 1,
            max_object_size: 2,
        },
        E::MovePackageTooBig {
            object_size: 1,
            max_object_size: 2,
        },
        E::CircularObjectOwnership { object: id(1) },
        E::InsufficientCoinBalance,
        E::CoinBalanceOverflow,
        E::PublishErrorNonZeroAddress,
        E::SuiMoveVerificationError,
        E::MovePrimitiveRuntimeError(st::MoveLocationOpt(None)),
        E::MovePrimitiveRuntimeError(st::MoveLocationOpt(Some(location(false)))),
        E::MoveAbort(location(true), 7),
        E::VMVerificationOrDeserializationError,
        E::VMInvariantViolation,
        E::FunctionNotFound,
        E::ArityMismatch,
        E::TypeArityMismatch,
        E::NonEntryFunctionInvoked,
    ]
}

/// Variants 21 to 41, in order, with 19, 20 and 27 in every shape.
fn late_errors() -> Vec<st::ExecutionErrorKind> {
    use st::ExecutionErrorKind as E;
    let mut errors = vec![
        E::UnusedValueWithoutDrop {
            result_idx: 1,
            secondary_idx: 2,
        },
        E::InvalidPublicFunctionReturnType { idx: 1 },
        E::InvalidTransferObject,
        E::EffectsTooLarge {
            current_size: 1,
            max_size: 2,
        },
        E::PublishUpgradeMissingDependency,
        E::PublishUpgradeDependencyDowngrade,
        E::WrittenObjectsTooLarge {
            current_size: 1,
            max_size: 2,
        },
        E::CertificateDenied,
        E::SuiMoveVerificationTimedout,
        E::SharedObjectOperationNotAllowed,
        E::InputObjectDeleted,
        E::ExecutionCancelledDueToSharedObjectCongestion {
            congested_objects: st::CongestedObjects(vec![id(1), id(2)]),
        },
        E::AddressDeniedForCoin {
            address: address(1),
            coin_type: "0x2::sui::SUI".into(),
        },
        E::CoinTypeGlobalPause {
            coin_type: "0x2::sui::SUI".into(),
        },
        E::ExecutionCancelledDueToRandomnessUnavailable,
        E::MoveVectorElemTooBig {
            value_size: 1,
            max_scaled_size: 2,
        },
        E::MoveRawValueTooBig {
            value_size: 1,
            max_scaled_size: 2,
        },
        E::InvalidLinkage,
        E::InsufficientFundsForWithdraw,
        E::NonExclusiveWriteInputObjectModified { id: id(1) },
    ];
    errors.extend(
        every_argument_error()
            .into_iter()
            .map(|kind| E::CommandArgumentError { arg_idx: 1, kind }),
    );
    for kind in [
        st::TypeArgumentError::TypeNotFound,
        st::TypeArgumentError::ConstraintNotSatisfied,
    ] {
        errors.push(E::TypeArgumentError {
            argument_idx: 2,
            kind,
        });
    }
    errors.extend(
        every_upgrade_error()
            .into_iter()
            .map(|upgrade_error| E::PackageUpgradeError { upgrade_error }),
    );
    errors
}

fn every_error() -> Vec<st::ExecutionErrorKind> {
    let mut errors = early_errors();
    errors.extend(late_errors());
    errors
}

fn every_argument_error() -> Vec<st::CommandArgumentError> {
    use st::CommandArgumentError as E;
    vec![
        E::TypeMismatch,
        E::InvalidBCSBytes,
        E::InvalidUsageOfPureArg,
        E::InvalidArgumentToPrivateEntryFunction,
        E::IndexOutOfBounds { idx: 1 },
        E::SecondaryIndexOutOfBounds {
            result_idx: 1,
            secondary_idx: 2,
        },
        E::InvalidResultArity { result_idx: 1 },
        E::InvalidGasCoinUsage,
        E::InvalidValueUsage,
        E::InvalidObjectByValue,
        E::InvalidObjectByMutRef,
        E::SharedObjectOperationNotAllowed,
        E::InvalidArgumentArity,
        E::InvalidTransferObject,
        E::InvalidMakeMoveVecNonObjectArgument,
        E::ArgumentWithoutValue,
        E::CannotMoveBorrowedValue,
        E::CannotWriteToExtendedReference,
        E::InvalidReferenceArgument,
        E::InvalidTxContext,
    ]
}

fn every_upgrade_error() -> Vec<st::PackageUpgradeError> {
    use st::PackageUpgradeError as E;
    vec![
        E::UnableToFetchPackage { package_id: id(1) },
        E::NotAPackage { object_id: id(1) },
        E::IncompatibleUpgrade,
        E::DigestDoesNotMatch { digest: vec![1, 2] },
        E::UnknownUpgradePolicy { policy: 3 },
        E::PackageIDDoesNotMatch {
            package_id: id(1),
            ticket_id: id(2),
        },
    ]
}

#[test]
fn effects() {
    let message = check::<TransactionEffects, fx::TransactionEffects>(&effects_v1());
    let view = message.get();
    expect_digest("TransactionEffects", view.bytes, view.digest);
    assert!(matches!(view.version, VersionedEffects::V1(_)));

    for some in [true, false] {
        let (built, kinds) = effects_v2(st::ExecutionStatus::Success, some);
        let message = check::<TransactionEffects, fx::TransactionEffects>(&built);
        let view = message.get();
        expect_digest("TransactionEffects", view.bytes, view.digest);
        let VersionedEffects::V2(v2) = &view.version else {
            panic!("not V2")
        };
        let found: Vec<ChangeKind> = v2.changed_objects.iter().map(|c| c.kind).collect();
        assert_eq!(found, kinds);
        assert_eq!(v2.changes(ChangeKind::Mutated).count(), 6);
        assert_eq!(v2.changes(ChangeKind::AccumulatorWrite).count(), 3);
        assert_eq!(v2.gas_object_index.is_some(), some);
        assert_eq!(v2.events_digest.is_some(), some);
        assert_eq!(v2.aux_data_digest.is_some(), some);
    }
}

/// All 42 error kinds, with every argument, type argument and upgrade error
/// and both `command` states.
#[test]
fn execution_statuses() {
    let errors = every_error();
    // 39 plain kinds, the second `MovePrimitiveRuntimeError` shape, and the
    // three kinds expanded to every inner variant.
    assert_eq!(errors.len(), 39 + 1 + 20 + 2 + 6);
    for (i, error) in errors.into_iter().enumerate() {
        let status = st::ExecutionStatus::Failure(st::ExecutionFailure {
            error,
            command: (i % 2 == 0).then_some(i as u64),
        });
        let (built, _) = effects_v2(status, false);
        check::<TransactionEffects, fx::TransactionEffects>(&built);
    }
}

fn events() -> fx::TransactionEvents {
    let event = |contents| fx::Event {
        package_id: id(0x20),
        transaction_module: "m".into(),
        sender: address(1),
        type_: struct_tag(0x30, vec![nested_tag()]),
        contents,
    };
    fx::TransactionEvents {
        data: vec![event(vec![1, 2, 3]), event(vec![])],
    }
}

#[test]
fn transaction_events() {
    let message = check::<TransactionEvents, fx::TransactionEvents>(&events());
    let view = message.get();
    expect_digest("TransactionEvents", view.bytes, view.digest());
    assert_eq!(view.data.len(), 2);
}

// Objects.

fn upgrade(n: u8) -> ob::UpgradeInfo {
    ob::UpgradeInfo {
        upgraded_id: id(n),
        upgraded_version: base::SequenceNumber(u64::from(n)),
    }
}

fn package() -> ob::MovePackage {
    ob::MovePackage {
        id: id(0x50),
        version: base::SequenceNumber(2),
        module_map: BTreeMap::from([("a".into(), vec![1]), ("b".into(), vec![])]),
        type_origin_table: vec![ob::TypeOrigin {
            module_name: "a".into(),
            datatype_name: "T".into(),
            package: id(0x50),
        }],
        linkage_table: BTreeMap::from([(id(1), upgrade(2)), (id(3), upgrade(4))]),
    }
}

/// A Move object of each type, then a package.
fn every_data() -> Vec<ob::Data> {
    use ob::MoveObjectTypeInner as T;
    let types = [
        T::Other(struct_tag(0x30, vec![])),
        T::GasCoin,
        T::StakedSui,
        T::Coin(nested_tag()),
        T::SuiBalanceAccumulatorField,
        T::BalanceAccumulatorField(ty::TypeTag::U8),
    ];
    let mut data: Vec<_> = types
        .into_iter()
        .enumerate()
        .map(|(i, inner)| {
            ob::Data::Move(ob::MoveObject {
                type_: ob::MoveObjectType(inner),
                has_public_transfer: i % 2 == 0,
                version: base::SequenceNumber(i as u64),
                contents: vec![7; i],
            })
        })
        .collect();
    data.push(ob::Data::Package(package()));
    data
}

/// Each kind of data with an owner, the owners cycled.
fn owned_data() -> Vec<(ob::Data, ob::Owner)> {
    let owners = owners();
    every_data()
        .into_iter()
        .enumerate()
        .map(|(i, data)| (data, owners[i % owners.len()].clone()))
        .collect()
}

fn objects() -> Vec<ob::Object> {
    owned_data()
        .into_iter()
        .map(|(data, owner)| ob::Object {
            data,
            owner,
            previous_transaction: base::TransactionDigest(digest(0x51)),
            storage_rebate: 100,
        })
        .collect()
}

fn genesis_objects() -> Vec<ob::GenesisObject> {
    owned_data()
        .into_iter()
        .map(|(data, owner)| ob::GenesisObject::RawObject { data, owner })
        .collect()
}

#[test]
fn every_object() {
    for built in objects() {
        let message = check::<Object, ob::Object>(&built);
        let view = message.get();
        expect_digest("Object", view.bytes, view.digest());
        if let (ViewData::Move(m), ob::Data::Move(b)) = (&view.data, &built.data) {
            assert_eq!(ob::MoveObjectType::from(&m.type_), b.type_);
        }
    }
}

/// `MovePackage` with its maps as sequences, to put entries out of order.
#[derive(Serialize)]
struct LoosePackage {
    id: base::ObjectId,
    version: base::SequenceNumber,
    module_map: Vec<(String, serde_bytes::ByteBuf)>,
    type_origin_table: Vec<ob::TypeOrigin>,
    linkage_table: Vec<(base::ObjectId, ob::UpgradeInfo)>,
}

/// An `Object` holding a package with the given module names and linkage
/// keys, in that order.
fn loose_object(modules: &[&str], linkage: &[u8]) -> Vec<u8> {
    let module = |name: &&str| ((*name).to_owned(), serde_bytes::ByteBuf::from(vec![1]));
    let package = LoosePackage {
        id: id(0x50),
        version: base::SequenceNumber(2),
        module_map: modules.iter().map(module).collect(),
        type_origin_table: vec![],
        linkage_table: linkage.iter().map(|&n| (id(n), upgrade(n))).collect(),
    };
    // `Data::Package` is variant 1; the object's other fields follow.
    let mut bytes = vec![1];
    bytes.extend(bcs::to_bytes(&package).unwrap());
    let tail = (ob::Owner::Immutable, digest(0x51), 0u64);
    bytes.extend(bcs::to_bytes(&tail).unwrap());
    bytes
}

#[test]
fn package_maps_must_be_sorted() {
    let sorted = loose_object(&["a", "b"], &[1, 2]);
    let object: ob::Object = bcs::from_bytes(&sorted).unwrap();
    assert_eq!(bcs::to_bytes(&object).unwrap(), sorted);
    check::<Object, ob::Object>(&object);

    let unsorted_modules = loose_object(&["b", "a"], &[1, 2]);
    let unsorted_linkage = loose_object(&["a", "b"], &[2, 1]);
    let repeated_linkage = loose_object(&["a"], &[1, 1]);
    for bytes in [unsorted_modules, unsorted_linkage, repeated_linkage] {
        assert!(bcs::from_bytes::<ob::Object>(&bytes).is_err());
        let (e, _) = Message::<Object>::parse(bytes).unwrap_err();
        assert_eq!(e, ParseError::NonCanonicalMap);
    }
}

/// Shortens the length-prefixed byte string that starts at `pos` by one.
fn shorten(bytes: &mut Vec<u8>, pos: usize) {
    bytes[pos] -= 1;
    bytes.remove(pos + 1);
}

#[test]
fn fixed_lengths_are_checked() {
    // The object's last 41 bytes are its previous transaction and rebate.
    let mut object = bcs::to_bytes(&objects()[1]).unwrap();
    let pos = object.len() - 41;
    assert_eq!(object[pos], 32);
    shorten(&mut object, pos);
    assert!(bcs::from_bytes::<ob::Object>(&object).is_err());
    let (e, _) = Message::<Object>::parse(object).unwrap_err();
    assert_eq!(
        e,
        ParseError::WrongLength {
            ty: "Digest",
            expected: 32,
            actual: 31
        }
    );

    let mut summary = bcs::to_bytes(&summary(true)).unwrap();
    let pos = summary
        .windows(3)
        .position(|w| w == [96, 0x77, 0x77])
        .unwrap();
    shorten(&mut summary, pos);
    assert!(bcs::from_bytes::<ck::CheckpointSummary>(&summary).is_err());
    let (e, _) = Message::<CheckpointSummary>::parse(summary).unwrap_err();
    assert_eq!(
        e,
        ParseError::WrongLength {
            ty: "AuthorityPublicKeyBytes",
            expected: 96,
            actual: 95
        }
    );
}

// Checkpoints.

fn commitments() -> Vec<ck::CheckpointCommitment> {
    vec![
        ck::CheckpointCommitment::EcmhLiveObjectSetDigest(base::EcmhLiveObjectSetDigest {
            digest: digest(0x90),
        }),
        ck::CheckpointCommitment::CheckpointArtifactsDigest(base::CheckpointArtifactsDigest(
            digest(0x91),
        )),
    ]
}

fn summary(full: bool) -> ck::CheckpointSummary {
    ck::CheckpointSummary {
        epoch: 5,
        sequence_number: 100,
        network_total_transactions: 1000,
        content_digest: base::CheckpointContentsDigest(digest(0x92)),
        previous_digest: full.then(|| base::CheckpointDigest(digest(0x93))),
        epoch_rolling_gas_cost_summary: gas_summary(),
        timestamp_ms: 7,
        checkpoint_commitments: if full { commitments() } else { vec![] },
        end_of_epoch_data: full.then(|| ck::EndOfEpochData {
            next_epoch_committee: vec![(authority(0x77), 10), (authority(0x78), 20)],
            next_epoch_protocol_version: base::ProtocolVersion(71),
            epoch_commitments: commitments(),
        }),
        version_specific_data: if full { vec![1, 2] } else { vec![] },
    }
}

fn certified(full: bool) -> ck::CertifiedCheckpointSummary {
    ck::CertifiedCheckpointSummary {
        data: summary(full),
        auth_signature: sg::AuthorityQuorumSignInfo {
            epoch: 5,
            signature: [0x55; 48],
            signers_map: vec![1, 2, 3],
        },
    }
}

fn contents(v2: bool) -> ck::CheckpointContents {
    let digests = ck::ExecutionDigests {
        transaction: base::TransactionDigest(digest(0xa0)),
        effects: base::TransactionEffectsDigest(digest(0xa1)),
    };
    if v2 {
        let [first, second] = signatures().try_into().unwrap();
        ck::CheckpointContents::V2(ck::CheckpointContentsV2 {
            transactions: vec![
                ck::CheckpointTransactionContents {
                    digest: digests,
                    user_signatures: vec![(first, Some(base::SequenceNumber(4))), (second, None)],
                },
                ck::CheckpointTransactionContents {
                    digest: digests,
                    user_signatures: vec![],
                },
            ],
        })
    } else {
        ck::CheckpointContents::V1(ck::CheckpointContentsV1 {
            transactions: vec![digests, digests],
            user_signatures: vec![signatures(), vec![]],
        })
    }
}

#[test]
fn checkpoint_summaries() {
    for full in [true, false] {
        let message = check::<CheckpointSummary, ck::CheckpointSummary>(&summary(full));
        let view = message.get();
        expect_digest("CheckpointSummary", view.bytes, view.digest);
        assert_eq!(view.end_of_epoch_data.is_some(), full);
        assert_eq!(view.previous_digest.is_some(), full);

        let message =
            check::<CertifiedCheckpointSummary, ck::CertifiedCheckpointSummary>(&certified(full));
        let view = message.get();
        expect_digest("CheckpointSummary", view.data.bytes, view.data.digest);
        assert_eq!(view.auth_signature.signers_map, [1, 2, 3]);
    }
}

#[test]
fn checkpoint_contents() {
    for v2 in [true, false] {
        let message = check::<CheckpointContents, ck::CheckpointContents>(&contents(v2));
        let view = message.get();
        expect_digest("CheckpointContents", view.bytes, view.digest());
    }
}

/// A user transaction with V2 effects, then a genesis with V1 effects.
fn executed() -> [(tx::Transaction, fx::TransactionEffects); 2] {
    let genesis = tx::TransactionKind::Genesis(sys::GenesisTransaction {
        objects: genesis_objects(),
    });
    [
        (
            signed(
                tx::TransactionKind::ProgrammableTransaction(programmable()),
                3,
            ),
            effects_v2(st::ExecutionStatus::Success, true).0,
        ),
        (signed(genesis, 0), effects_v1()),
    ]
}

fn check_executed(transaction: &SenderSignedData<'_>, effects: &TransactionEffects<'_>) {
    expect_digest(
        "TransactionData",
        transaction.data.bytes,
        *transaction.data.digest(),
    );
    expect_digest("TransactionEffects", effects.bytes, effects.digest);
}

#[test]
fn full_checkpoint_contents() {
    let built = ck::FullCheckpointContents {
        transactions: executed()
            .into_iter()
            .map(|(transaction, effects)| ck::ExecutionData {
                transaction,
                effects,
            })
            .collect(),
        user_signatures: vec![signatures(), vec![]],
    };
    let message = check::<FullCheckpointContents, ck::FullCheckpointContents>(&built);
    for t in message.get().transactions {
        check_executed(&t.transaction, &t.effects);
    }
}

#[test]
fn checkpoint_data() {
    let [(user, user_effects), (genesis, genesis_effects)] = executed();
    let built = ck::CheckpointData {
        checkpoint_summary: certified(true),
        checkpoint_contents: contents(true),
        transactions: vec![
            ck::CheckpointTransaction {
                transaction: user,
                effects: user_effects,
                events: Some(events()),
                input_objects: objects(),
                output_objects: vec![],
            },
            ck::CheckpointTransaction {
                transaction: genesis,
                effects: genesis_effects,
                events: None,
                input_objects: vec![],
                output_objects: objects(),
            },
        ],
    };
    let message = check::<CheckpointData, ck::CheckpointData>(&built);
    let view = message.get();
    let summary = &view.checkpoint_summary.data;
    expect_digest("CheckpointSummary", summary.bytes, summary.digest);
    let contents = &view.checkpoint_contents;
    expect_digest("CheckpointContents", contents.bytes, contents.digest());
    for t in view.transactions {
        check_executed(&t.transaction, &t.effects);
        if let Some(events) = &t.events {
            expect_digest("TransactionEvents", events.bytes, events.digest());
        }
        for object in t.input_objects.iter().chain(t.output_objects) {
            expect_digest("Object", object.bytes, object.digest());
        }
    }
    check_programmable_index(&view.transactions[0].transaction.data.index, true);
}

// Signatures and type tags.

#[test]
fn multisig() {
    use sg::{CompressedSignature as S, PublicKey as K};
    let multisig_pk = sg::MultiSigPublicKey {
        pk_map: vec![
            (K::Ed25519([5; 32]), 1),
            (K::Secp256k1([6; 33]), 2),
            (K::Secp256r1([7; 33]), 3),
            (K::ZkLogin(sg::ZkLoginPublicIdentifier(vec![8; 40])), 4),
        ],
        threshold: 5,
    };
    let bytes = bcs::to_bytes(&multisig_pk).unwrap();
    let message = Message::<MultiSigPublicKey>::parse(bytes).unwrap();
    assert_eq!(
        sg::MultiSigPublicKey::try_from(message.get()),
        Ok(multisig_pk.clone())
    );

    let built = sg::MultiSig {
        sigs: vec![
            S::Ed25519([1; 64]),
            S::Secp256k1([2; 64]),
            S::Secp256r1([3; 64]),
            S::ZkLogin(sg::ZkLoginAuthenticatorAsBytes(vec![4; 200])),
        ],
        bitmap: 0b1111,
        multisig_pk,
    };
    let bytes = bcs::to_bytes(&built).unwrap();
    assert_eq!(bcs::from_bytes::<sg::MultiSig>(&bytes).unwrap(), built);
    let message = Message::<MultiSig>::parse(bytes).unwrap();
    assert_eq!(sg::MultiSig::try_from(message.get()), Ok(built));
}

/// `n` struct tags each holding the next as its one type parameter, by hand:
/// the builder cannot encode past the depth `bcs` allows.
fn struct_chain(n: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for i in 0..n {
        bytes.push(7);
        bytes.extend_from_slice(&[0; 32]);
        bytes.extend_from_slice(b"\x01m\x01S");
        bytes.push(u8::from(i + 1 < n));
    }
    bytes
}

/// Each vector and each struct is one container to `bcs` and a struct's
/// address is one more: 500 are accepted and 501 are not, on both sides.
#[test]
fn type_tag_depth() {
    let mut tag = ty::TypeTag::U8;
    for _ in 0..499 {
        tag = ty::TypeTag::Vector(Box::new(tag));
    }
    let mut bytes = vec![6; 499];
    bytes.push(1);
    assert_eq!(bcs::to_bytes(&tag).unwrap(), bytes);
    check::<TypeTag, ty::TypeTag>(&tag);

    let deep = ty::TypeTag::Vector(Box::new(tag));
    assert!(bcs::to_bytes(&deep).is_err());
    let mut bytes = vec![6; 500];
    bytes.push(1);
    assert!(bcs::from_bytes::<ty::TypeTag>(&bytes).is_err());
    let (e, _) = Message::<TypeTag>::parse(bytes).unwrap_err();
    assert_eq!(e, ParseError::ContainerDepthExceeded);

    let mut tag = ty::TypeTag::Struct(Box::new(struct_tag(0, vec![])));
    for _ in 1..249 {
        tag = ty::TypeTag::Struct(Box::new(struct_tag(0, vec![tag])));
    }
    assert_eq!(bcs::to_bytes(&tag).unwrap(), struct_chain(249));
    check::<TypeTag, ty::TypeTag>(&tag);
    let deep = ty::TypeTag::Struct(Box::new(struct_tag(0, vec![tag])));
    assert!(bcs::to_bytes(&deep).is_err());
    assert!(bcs::from_bytes::<ty::TypeTag>(&struct_chain(250)).is_err());
    let (e, _) = Message::<TypeTag>::parse(struct_chain(250)).unwrap_err();
    assert_eq!(e, ParseError::ContainerDepthExceeded);
}
