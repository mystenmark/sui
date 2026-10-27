// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Congestion control in the commit handler: with the budget overridden to
//! nothing, transactions writing a consensus object after the first one in a
//! commit are deferred, cancelled past the deferral limit, and an object's
//! cost carries into later rounds as debt (`docs/congestion_control_reference.md`).

use std::sync::Arc;

use consensus::BlockRef;
use messages::Message;
use messages::base::Hashed;
use sui_protocol_config::{
    Chain, ExecutionTimeEstimateParams, PerObjectCongestionControlMode, ProtocolVersion,
};
use sui_types::base_types::{ObjectID, SequenceNumber, SuiAddress, random_object_ref};
use sui_types::crypto::{AccountKeyPair, get_key_pair};
use sui_types::messages_consensus::ConsensusDeterminedVersionAssignments;
use sui_types::transaction::{
    Argument, CallArg, Command, ObjectArg, ProgrammableTransaction, SharedObjectMutability,
    Transaction, TransactionData, TransactionDataAPI as _, TransactionKind,
};
use validator::checks::{self, VerifiedTransaction};
use validator::consensus::commit::{CommitHandler, CommitStage, EpochStart, HandledCommit};
use validator::consensus::decode::{CommittedTransaction, DecodedCommit};
use validator::consensus::sequence::{CancelledInput, Scheduled};
use validator::epoch::EpochParams;
use validator::epoch_store::{ConsensusOutput, EpochStore};
use validator::pipeline::items::ExecutableTransaction;

/// An epoch whose commits have no congestion budget, deferring for at most
/// `max_rounds` rounds.
fn epoch(max_rounds: u64) -> Arc<EpochParams> {
    let mut epoch = EpochParams::new(
        Chain::Unknown,
        ProtocolVersion::MAX.as_u64(),
        0,
        messages::base::Digest::new([0; 32]),
        1000,
        4,
    );
    let PerObjectCongestionControlMode::ExecutionTimeEstimate(params) =
        epoch.config.per_object_congestion_control_mode()
    else {
        unreachable!("estimates");
    };
    epoch
        .config
        .set_per_object_congestion_control_mode_for_testing(
            PerObjectCongestionControlMode::ExecutionTimeEstimate(ExecutionTimeEstimateParams {
                target_utilization: 0,
                allowed_txn_cost_overage_burst_limit_us: 0,
                ..params
            }),
        );
    epoch
        .config
        .set_max_deferral_rounds_for_congestion_control_for_testing(max_rounds);
    Arc::new(epoch)
}

fn consensus_output() -> (ConsensusOutput, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let (_, output, _) = EpochStore::open(dir.path(), 0);
    (output, dir)
}

fn handler(epoch: &EpochParams, output: ConsensusOutput) -> CommitHandler {
    CommitHandler::new(output, 0, vec![([0; 96], 10_000)], &epoch.config, None)
}

fn object(n: u8) -> ObjectID {
    ObjectID::from_single_byte(n)
}

/// A transaction with a consensus object input per `(object, mutability)`,
/// estimated at 1 ms: one command, with no observations.
fn transaction(inputs: &[(u8, SharedObjectMutability)]) -> VerifiedTransaction {
    let (sender, key): (SuiAddress, AccountKeyPair) = get_key_pair();
    let mut args: Vec<CallArg> = inputs
        .iter()
        .map(|&(n, mutability)| {
            CallArg::Object(ObjectArg::SharedObject {
                id: object(n),
                initial_shared_version: SequenceNumber::from_u64(1),
                mutability,
            })
        })
        .collect();
    args.push(CallArg::Pure(bcs::to_bytes(&1u64).unwrap()));
    let pt = ProgrammableTransaction {
        commands: vec![Command::SplitCoins(
            Argument::GasCoin,
            vec![Argument::Input(inputs.len() as u16)],
        )],
        inputs: args,
    };
    let data = TransactionData::new(
        TransactionKind::ProgrammableTransaction(pt),
        sender,
        random_object_ref(),
        1_000_000,
        1000,
    );
    let bytes = bcs::to_bytes(&Transaction::from_data_and_signer(data, vec![&key])).unwrap();
    checks::sequenced_by_consensus(Message::parse(bytes).map_err(|(e, _)| e).unwrap())
}

fn writer(n: u8) -> VerifiedTransaction {
    transaction(&[(n, SharedObjectMutability::Mutable)])
}

/// Commit `index` in `round`, one block of `transactions`.
fn commit(index: u32, round: u32, transactions: &[&VerifiedTransaction]) -> DecodedCommit {
    let bump = containers::Bump::default();
    let block = BlockRef {
        round,
        author: 1,
        digest: [index as u8; 32],
    };
    DecodedCommit {
        commit_index: index,
        leader: block,
        timestamp_ms: 1000 * u64::from(round),
        digest: [index as u8; 32],
        transactions: transactions
            .iter()
            .enumerate()
            .map(|(i, t)| CommittedTransaction::new(block, i as u16, (*t).clone(), &[], &bump))
            .collect(),
        observations: Vec::new(),
        jwks: Vec::new(),
        capabilities: Vec::new(),
        rejected: Vec::new(),
        cache_hits: 0,
        cache_misses: transactions.len(),
        handled: None,
    }
}

fn digest(t: &VerifiedTransaction) -> Hashed {
    *t.get().0.digest()
}

/// The user transactions scheduled, and of those cancelled, their inputs.
fn scheduled(handled: &HandledCommit) -> Vec<(Hashed, Option<Vec<CancelledInput>>)> {
    handled
        .schedule
        .transactions
        .iter()
        .filter_map(|t| match t {
            Scheduled::User(t) => Some((digest(t), None)),
            Scheduled::Cancelled(t, inputs) => Some((digest(t), Some(inputs.clone()))),
            Scheduled::Prologue(_) | Scheduled::AuthenticatorStateUpdate(_) => None,
        })
        .collect()
}

fn deferred(handled: &HandledCommit) -> Vec<Hashed> {
    handled.deferred.iter().map(|(.., d)| *d).collect()
}

fn cancelled(n: u8, marker: SequenceNumber) -> CancelledInput {
    CancelledInput {
        id: messages::base::ObjectId(object(n).into_bytes()),
        initial_shared_version: 1,
        marker: marker.value(),
    }
}

#[test]
fn past_the_budget_a_commit_defers_later_writers() {
    let bump = containers::Bump::default();
    let epoch = epoch(10);
    let (output, _dir) = consensus_output();
    let mut handler = handler(&epoch, output);
    let (first, second, third) = (writer(1), writer(1), writer(2));
    let decided = handler
        .handle(&epoch, commit(1, 1, &[&first, &second, &third]), &bump)
        .unwrap();
    // The first starts at nothing, however large; the second after it.
    assert_eq!(
        scheduled(&decided),
        vec![(digest(&first), None), (digest(&third), None)]
    );
    assert_eq!(deferred(&decided), vec![digest(&second)]);
}

#[test]
fn past_the_deferral_limit_a_transaction_is_cancelled_in_place() {
    let bump = containers::Bump::default();
    let epoch = epoch(0);
    let (output, _dir) = consensus_output();
    let mut handler = handler(&epoch, output);
    let first = writer(1);
    // Reads the congested object, and writes another, which it does not
    // charge.
    let reader = transaction(&[
        (1, SharedObjectMutability::Immutable),
        (2, SharedObjectMutability::Mutable),
    ]);
    let other = writer(2);
    let decided = handler
        .handle(&epoch, commit(1, 1, &[&first, &reader, &other]), &bump)
        .unwrap();
    assert_eq!(
        scheduled(&decided),
        vec![
            (digest(&first), None),
            (
                digest(&reader),
                Some(vec![
                    cancelled(1, SequenceNumber::CONGESTED),
                    cancelled(2, SequenceNumber::CANCELLED_READ),
                ])
            ),
            (digest(&other), None),
        ]
    );
    assert!(deferred(&decided).is_empty());
}

#[test]
fn an_object_s_debt_carries_into_the_next_round_and_decays_after() {
    let bump = containers::Bump::default();
    let epoch = epoch(10);
    let (output, _dir) = consensus_output();
    let mut handler = handler(&epoch, output);
    let (first, second, third) = (writer(1), writer(1), writer(1));
    let decided = handler
        .handle(&epoch, commit(1, 1, &[&first]), &bump)
        .unwrap();
    assert_eq!(scheduled(&decided), vec![(digest(&first), None)]);
    // The next round starts at the first's 1 ms.
    let decided = handler
        .handle(&epoch, commit(2, 2, &[&second]), &bump)
        .unwrap();
    assert_eq!(deferred(&decided), vec![digest(&second)]);
    // A skipped round takes 37 s off the debt.
    let decided = handler
        .handle(&epoch, commit(3, 4, &[&third]), &bump)
        .unwrap();
    assert_eq!(scheduled(&decided), vec![(digest(&second), None)]);
    assert_eq!(deferred(&decided), vec![digest(&third)]);
}

/// The prologue lists each cancelled transaction, with its inputs' markers,
/// as the reference's `ConsensusDeterminedVersionAssignments`.
#[test]
fn the_prologue_lists_cancelled_transactions() {
    let bump = containers::Bump::default();
    let epoch = epoch(0);
    let (output, _dir) = consensus_output();
    let (chunks, _chunks) = workqueue::queue(16);
    let (settlements, _settlements) = workqueue::queue(16);
    let (checkpoints, _checkpoints) = workqueue::queue(16);
    let start = EpochStart {
        previous_checkpoint: 0,
        accumulator_root_initial_shared_version: 1,
        authenticator_initial_shared_version: None,
        accumulator_root_version: 1,
        epoch_start_timestamp_ms: 0,
        epoch_duration_ms: u64::MAX,
    };
    let mut stage = CommitStage::new(
        epoch.clone(),
        start,
        output,
        chunks,
        settlements,
        checkpoints,
        Arc::new(validator::consensus::status::ConsensusTxStatusCache::new()),
        vec![([0; 96], 10_000)],
    );
    let (first, second) = (writer(1), writer(1));
    let decided = stage
        .decide(commit(1, 1, &[&first, &second]), &bump)
        .unwrap();
    let entries = &decided.chunks[0].entries;
    let ExecutableTransaction::System(prologue) = &entries[0].transaction else {
        panic!("the prologue first");
    };
    let signed: sui_types::transaction::SenderSignedData =
        bcs::from_bytes(&prologue.bytes).unwrap();
    let sui_types::transaction::TransactionKind::ConsensusCommitPrologueV4(prologue) =
        signed.transaction_data().kind()
    else {
        panic!("a V4 prologue");
    };
    assert_eq!(
        prologue.consensus_determined_version_assignments,
        ConsensusDeterminedVersionAssignments::CancelledTransactionsV2(vec![(
            sui_types::digests::TransactionDigest::new(digest(&second).bytes),
            vec![(
                (object(1), SequenceNumber::from_u64(1)),
                SequenceNumber::CONGESTED
            )],
        )])
    );
    // The cancelled transaction is scheduled in place, reading its markers.
    assert_eq!(
        entries[2].cancelled,
        vec![(
            messages::base::ObjectId(object(1).into_bytes()),
            SequenceNumber::CONGESTED.value()
        )]
    );
    assert!(entries[2].shared.is_empty());
}
