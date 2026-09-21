// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Traces the `build` types with serde-reflection and expects sui's format
//! snapshot, following the procedure of sui's own `tests/format.rs`.

use std::path::Path;

use anchovy_types::build::base::{
    AuthorityPublicKeyBytes, Digest, ObjectId, ProtocolVersion, SuiAddress,
};
use anchovy_types::build::checkpoint::{
    CertifiedCheckpointSummary, CheckpointCommitment, CheckpointContents, CheckpointData,
    CheckpointSummary, CheckpointTransaction, FullCheckpointContents,
};
use anchovy_types::build::effects::{
    AccumulatorOperation, AccumulatorValue, DeleteKind, IdOperation, ObjectIn, ObjectOut,
    TransactionEffects, TransactionEvents, UnchangedConsensusKind,
};
use anchovy_types::build::execution_status::{
    CommandArgumentError, ExecutionErrorKind, ExecutionStatus, PackageUpgradeError,
    TypeArgumentError,
};
use anchovy_types::build::object::{
    Data, GenesisObject, MoveObjectType, MoveObjectTypeInner, Object, ObjectInfoRequestKind, Owner,
};
use anchovy_types::build::signature::{CompressedSignature, MultiSig, PublicKey};
use anchovy_types::build::system_transaction::{
    ConsensusDeterminedVersionAssignments, EndOfEpochTransactionKind, ExecutionTimeObservationKey,
    StoredExecutionTimeObservations,
};
use anchovy_types::build::transaction::{
    Argument, CallArg, Command, GasData, Intent, IntentMessage, ObjectArg, Reservation,
    SenderSignedData, SenderSignedTransaction, SharedObjectMutability, TransactionData,
    TransactionDataV1, TransactionExpiration, TransactionKind, WithdrawFrom, WithdrawalTypeArg,
};
use anchovy_types::build::type_tag::{StructInput, TypeInput, TypeTag};
use serde_reflection::{Registry, Samples, Tracer, TracerConfig};

fn sender_signed_data_sample() -> SenderSignedData {
    SenderSignedData(SenderSignedTransaction {
        intent_message: IntentMessage {
            intent: Intent {
                scope: 0,
                version: 0,
                app_id: 0,
            },
            value: TransactionData::V1(TransactionDataV1 {
                kind: TransactionKind::EndOfEpochTransaction(Vec::new()),
                sender: SuiAddress([0; 32]),
                gas_data: GasData {
                    payment: Vec::new(),
                    owner: SuiAddress([0; 32]),
                    price: 0,
                    budget: 0,
                },
                expiration: TransactionExpiration::None,
            }),
        },
        tx_signatures: Vec::new(),
    })
}

fn traced_registry() -> Registry {
    let config = TracerConfig::default()
        .record_samples_for_structs(true)
        .record_samples_for_newtype_structs(true);
    let mut tracer = Tracer::new(config);
    let mut samples = Samples::new();

    // The tracer's default byte string is empty, which these two reject.
    tracer.trace_value(&mut samples, &Digest([0; 32])).unwrap();
    tracer
        .trace_value(&mut samples, &AuthorityPublicKeyBytes([0; 96]))
        .unwrap();

    tracer.trace_type::<StructInput>(&samples).unwrap();
    tracer.trace_type::<TypeInput>(&samples).unwrap();
    tracer.trace_type::<Owner>(&samples).unwrap();
    tracer.trace_type::<ExecutionStatus>(&samples).unwrap();
    tracer.trace_type::<ExecutionErrorKind>(&samples).unwrap();
    tracer.trace_type::<Reservation>(&samples).unwrap();
    tracer.trace_type::<WithdrawFrom>(&samples).unwrap();
    tracer.trace_type::<WithdrawalTypeArg>(&samples).unwrap();
    tracer.trace_type::<CallArg>(&samples).unwrap();
    tracer.trace_type::<ObjectArg>(&samples).unwrap();
    tracer
        .trace_type::<SharedObjectMutability>(&samples)
        .unwrap();
    tracer.trace_type::<Data>(&samples).unwrap();
    tracer.trace_type::<TypeTag>(&samples).unwrap();
    tracer
        .trace_type::<ObjectInfoRequestKind>(&samples)
        .unwrap();
    tracer.trace_type::<TransactionKind>(&samples).unwrap();
    tracer
        .trace_type::<ConsensusDeterminedVersionAssignments>(&samples)
        .unwrap();
    tracer.trace_type::<MoveObjectType>(&samples).unwrap();
    tracer.trace_type::<MoveObjectTypeInner>(&samples).unwrap();
    tracer.trace_type::<SuiAddress>(&samples).unwrap();
    tracer.trace_type::<DeleteKind>(&samples).unwrap();
    tracer.trace_type::<Argument>(&samples).unwrap();
    tracer.trace_type::<Command>(&samples).unwrap();
    tracer.trace_type::<CommandArgumentError>(&samples).unwrap();
    tracer.trace_type::<TypeArgumentError>(&samples).unwrap();
    tracer.trace_type::<PackageUpgradeError>(&samples).unwrap();
    tracer
        .trace_type::<TransactionExpiration>(&samples)
        .unwrap();
    tracer
        .trace_type::<ExecutionTimeObservationKey>(&samples)
        .unwrap();
    tracer
        .trace_type::<StoredExecutionTimeObservations>(&samples)
        .unwrap();
    tracer
        .trace_type::<EndOfEpochTransactionKind>(&samples)
        .unwrap();
    tracer.trace_type::<IdOperation>(&samples).unwrap();
    tracer.trace_type::<ObjectIn>(&samples).unwrap();
    tracer.trace_type::<ObjectOut>(&samples).unwrap();
    tracer
        .trace_type::<UnchangedConsensusKind>(&samples)
        .unwrap();
    tracer.trace_type::<AccumulatorValue>(&samples).unwrap();
    tracer.trace_type::<AccumulatorOperation>(&samples).unwrap();
    tracer.trace_type::<TransactionEffects>(&samples).unwrap();

    // sui reaches these four only through sampled signatures.
    tracer.trace_type::<CompressedSignature>(&samples).unwrap();
    tracer.trace_type::<PublicKey>(&samples).unwrap();
    tracer.trace_type::<MultiSig>(&samples).unwrap();

    tracer
        .trace_type::<FullCheckpointContents>(&samples)
        .unwrap();
    tracer.trace_type::<CheckpointContents>(&samples).unwrap();
    tracer.trace_type::<CheckpointSummary>(&samples).unwrap();

    // The tracer offers a one-element sequence only the first time it sees
    // a type, so from here on `SenderSignedData` needs a sample.
    tracer
        .trace_value(&mut samples, &sender_signed_data_sample())
        .unwrap();

    tracer
        .trace_type::<CertifiedCheckpointSummary>(&samples)
        .unwrap();
    tracer.trace_type::<Object>(&samples).unwrap();
    tracer.trace_type::<TransactionEvents>(&samples).unwrap();
    tracer
        .trace_type::<CheckpointTransaction>(&samples)
        .unwrap();
    tracer.trace_type::<CheckpointData>(&samples).unwrap();
    tracer.trace_type::<TransactionData>(&samples).unwrap();
    tracer.trace_type::<GenesisObject>(&samples).unwrap();
    tracer.trace_type::<CheckpointCommitment>(&samples).unwrap();

    // Newtypes that nothing above contains.
    tracer.trace_type::<ObjectId>(&samples).unwrap();
    tracer.trace_type::<ProtocolVersion>(&samples).unwrap();

    tracer.registry().unwrap()
}

fn snapshot_registry() -> Registry {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/format__sui.yaml.snap");
    let text = std::fs::read_to_string(path).unwrap();
    // An insta header sits between two `---` lines.
    let body = text
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map(|(_, body)| body)
        .unwrap();
    // insta writes enums as one-entry maps rather than YAML tags.
    let yaml = serde_yaml::Deserializer::from_str(body);
    serde_yaml::with::singleton_map_recursive::deserialize(yaml).unwrap()
}

#[test]
fn build_types_match_the_sui_snapshot() {
    let traced = traced_registry();
    let snapshot = snapshot_registry();
    assert_eq!(snapshot.len(), 124);

    let traced_names: Vec<_> = traced.keys().collect();
    let snapshot_names: Vec<_> = snapshot.keys().collect();
    assert_eq!(traced_names, snapshot_names);

    for (name, expected) in &snapshot {
        assert_eq!(&traced[name], expected, "{name}");
    }
    assert_eq!(traced, snapshot);
}
