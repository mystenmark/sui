// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Owned-object locks in the commit handler: the first transaction in
//! consensus order to use an owned reference wins it for the epoch, later
//! ones are dropped, inputs claimed immutable are not locked, and the
//! decisions are a function of the commits alone.

use std::sync::Arc;

use consensus::{Block, BlockRef, CommittedSubDag};
use messages::Message;
use messages::base::Hashed;
use sui_protocol_config::{Chain, ProtocolVersion};
use sui_types::base_types::{ObjectRef, SequenceNumber, SuiAddress, random_object_ref};
use sui_types::crypto::{AccountKeyPair, get_key_pair};
use sui_types::transaction::{
    CallArg, ObjectArg, ProgrammableTransaction, Transaction, TransactionData, TransactionKind,
};
use validator::checks::{self, VerifiedTransaction};
use validator::consensus::cache::ConsensusTxCache;
use validator::consensus::commit::{
    CommitHandler, CommitStage, DropReason, EpochStart, HandledCommit,
};
use validator::consensus::decode::{CommitDecoder, CommittedTransaction, DecodedCommit};
use validator::consensus::sequence::Scheduled;
use validator::epoch::EpochParams;
use validator::epoch_store::{ConsensusOutput, EpochStore};
use validator::pipeline::items::ExecutableTransaction;

fn epoch() -> Arc<EpochParams> {
    Arc::new(EpochParams::new(
        Chain::Unknown,
        ProtocolVersion::MAX.as_u64(),
        0,
        messages::base::Digest::new([0; 32]),
        1000,
        4,
    ))
}

/// The one writer of a fresh epoch store's consensus output, and the
/// directory the store is in.
fn consensus_output() -> (ConsensusOutput, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let (_, output, _) = EpochStore::open(dir.path(), 0);
    (output, dir)
}

struct Sender {
    address: SuiAddress,
    key: AccountKeyPair,
}

impl Sender {
    fn new() -> Sender {
        let (address, key) = get_key_pair();
        Sender { address, key }
    }

    /// A signed transaction paying with `gas`, taking `inputs` as owned or
    /// immutable objects, as `Transaction` BCS.
    fn bytes(&self, gas: ObjectRef, inputs: &[ObjectRef]) -> Vec<u8> {
        let pt = ProgrammableTransaction {
            inputs: inputs
                .iter()
                .map(|r| CallArg::Object(ObjectArg::ImmOrOwnedObject(*r)))
                .collect(),
            commands: vec![],
        };
        let data = TransactionData::new(
            TransactionKind::ProgrammableTransaction(pt),
            self.address,
            gas,
            1_000_000,
            1000,
        );
        bcs::to_bytes(&Transaction::from_data_and_signer(data, vec![&self.key])).unwrap()
    }

    fn transaction(&self, gas: ObjectRef, inputs: &[ObjectRef]) -> Tx {
        let bytes = self.bytes(gas, inputs);
        let digest = digest_of(&parse(bytes.clone()));
        Tx { bytes, digest }
    }
}

/// A signed transaction's BCS, and its digest.
struct Tx {
    bytes: Vec<u8>,
    digest: Hashed,
}

fn parse(bytes: Vec<u8>) -> VerifiedTransaction {
    checks::sequenced_by_consensus(Message::parse(bytes).map_err(|(e, _)| e).unwrap())
}

fn id(r: &ObjectRef) -> messages::base::ObjectId {
    messages::base::ObjectId(r.0.into_bytes())
}

fn digest_of(t: &VerifiedTransaction) -> Hashed {
    *t.get().0.digest()
}

#[tokio::test]
async fn the_stage_records_each_committed_transaction_s_status() {
    use validator::consensus::status::{
        ConsensusPosition, ConsensusTxStatus, ConsensusTxStatusCache, StatusResult,
    };
    use workqueue::Processor;
    let mut bump = containers::Bump::default();
    let epoch = epoch();
    let sender = Sender::new();
    let object = random_object_ref();
    let first = sender.transaction(random_object_ref(), &[object]);
    let second = sender.transaction(random_object_ref(), &[object]);
    let (chunks, _chunk_inbox) = workqueue::queue(16);
    let (settlements, _settlement_inbox) = workqueue::queue(16);
    let (checkpoints, _checkpoint_inbox) = workqueue::queue(16);
    let statuses = Arc::new(ConsensusTxStatusCache::new());
    let (output, _dir) = consensus_output();
    let mut stage = CommitStage::new(
        epoch.clone(),
        EpochStart {
            previous_checkpoint: 0,
            accumulator_root_initial_shared_version: 1,
            authenticator_initial_shared_version: None,
            accumulator_root_version: 1,
            epoch_start_timestamp_ms: 0,
            epoch_duration_ms: u64::MAX,
        },
        output,
        chunks,
        settlements,
        checkpoints,
        statuses.clone(),
        vec![([0; 96], 1)],
    );
    let mut decoded = commit(1, &[(&first, &[]), (&second, &[])]);
    let block = decoded.leader;
    decoded.rejected = vec![(block, 2)];
    stage.process(decoded, &mut bump);
    let status = |index| {
        statuses.notify_read(ConsensusPosition {
            epoch: epoch.epoch,
            block,
            index,
        })
    };
    assert_eq!(
        status(0).await,
        StatusResult::Status(ConsensusTxStatus::Finalized)
    );
    // It lost `object`'s lock to the first.
    assert_eq!(
        status(1).await,
        StatusResult::Status(ConsensusTxStatus::Dropped)
    );
    assert_eq!(
        status(2).await,
        StatusResult::Status(ConsensusTxStatus::Rejected)
    );
}

fn digest(t: &Tx) -> Hashed {
    t.digest
}

/// A transaction with the ids its submitter claimed immutable.
type Claimed<'a> = (&'a Tx, &'a [ObjectRef]);

/// Commit `index`, one block, of `transactions` in consensus order.
fn commit(index: u32, transactions: &[Claimed<'_>]) -> DecodedCommit {
    let bump = containers::Bump::default();
    let block = BlockRef {
        round: index,
        author: 1,
        digest: [index as u8; 32],
    };
    DecodedCommit {
        commit_index: index,
        leader: block,
        timestamp_ms: 1000 * u64::from(index),
        digest: [index as u8; 32],
        transactions: transactions
            .iter()
            .enumerate()
            .map(|(i, (t, immutable))| {
                let immutable: Vec<_> = immutable.iter().map(id).collect();
                CommittedTransaction::new(
                    block,
                    i as u16,
                    parse(t.bytes.clone()),
                    &immutable,
                    &bump,
                )
            })
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

/// The digests of the user transactions scheduled.
fn scheduled(handled: &HandledCommit) -> Vec<Hashed> {
    handled
        .schedule
        .transactions
        .iter()
        .filter_map(|t| match t {
            Scheduled::User(t) => Some(digest_of(t)),
            Scheduled::Prologue(_)
            | Scheduled::AuthenticatorStateUpdate(_)
            | Scheduled::Cancelled(..) => None,
        })
        .collect()
}

/// The digests of the user transactions deferred to a later round.
fn deferred(handled: &HandledCommit) -> Vec<Hashed> {
    handled
        .deferred
        .iter()
        .map(|(.., digest)| *digest)
        .collect()
}

/// Each dropped transaction's digest, and the reference and holder of the
/// lock it lost.
fn conflicts(handled: &HandledCommit) -> Vec<(Hashed, messages::base::ObjectRef, Hashed)> {
    handled
        .dropped
        .iter()
        .map(|(_, _, digest, reason)| match reason {
            DropReason::LockConflict { object, holder } => (*digest, *object, *holder),
            DropReason::Inputs(e) => panic!("dropped for its inputs: {e:?}"),
            DropReason::EpochClosed => panic!("dropped after the epoch closed"),
        })
        .collect()
}

fn wire(r: &ObjectRef) -> messages::base::ObjectRef {
    messages::base::ObjectRef {
        id: id(r),
        version: messages::base::U64Le::new(r.1.value()),
        digest: messages::base::Digest::new(r.2.into_inner()),
    }
}

/// A handler of a committee of one, that has handled no commit.
fn new_handler(output: validator::epoch_store::ConsensusOutput) -> CommitHandler {
    let config = sui_protocol_config::ProtocolConfig::get_for_version(
        sui_protocol_config::ProtocolVersion::MAX,
        sui_protocol_config::Chain::Unknown,
    );
    CommitHandler::new(output, 0, vec![([0; 96], 10_000)], &config, None)
}

#[test]
fn the_later_of_two_transactions_spending_one_object_in_a_commit_is_dropped() {
    let bump = containers::Bump::default();
    let epoch = epoch();
    let sender = Sender::new();
    let object = random_object_ref();
    let first = sender.transaction(random_object_ref(), &[object]);
    let second = sender.transaction(random_object_ref(), &[object]);
    let (output, _dir) = consensus_output();
    let mut handler = new_handler(output);
    let decided = handler
        .handle(&epoch, commit(1, &[(&first, &[]), (&second, &[])]), &bump)
        .unwrap();
    // The holder of the contested lock waits a round, as a penalty.
    assert!(scheduled(&decided).is_empty());
    assert_eq!(deferred(&decided), vec![digest(&first)]);
    assert_eq!(
        conflicts(&decided),
        vec![(digest(&second), wire(&object), digest(&first))]
    );
    // Included again, the dropped one loses the lock again, as locks are
    // taken before deduplication; the deferred one is scheduled.
    let decided = handler
        .handle(&epoch, commit(2, &[(&second, &[])]), &bump)
        .unwrap();
    assert_eq!(scheduled(&decided), vec![digest(&first)]);
    assert_eq!(
        conflicts(&decided),
        vec![(digest(&second), wire(&object), digest(&first))]
    );
    assert!(decided.duplicates.is_empty());
}

#[test]
fn a_deferred_transaction_outlives_a_restart() {
    let bump = containers::Bump::default();
    let epoch = epoch();
    let sender = Sender::new();
    let object = random_object_ref();
    let first = sender.transaction(random_object_ref(), &[object]);
    let second = sender.transaction(random_object_ref(), &[object]);
    let dir = tempfile::tempdir().unwrap();
    let (_, output, _) = EpochStore::open(dir.path(), 0);
    let committed = output.committed_checkpoints();
    let mut handler = new_handler(output);
    let decided = handler
        .handle(&epoch, commit(1, &[(&first, &[]), (&second, &[])]), &bump)
        .unwrap();
    assert_eq!(deferred(&decided), vec![digest(&first)]);
    // The commit's output is written once its checkpoint commits.
    let record = validator::consensus::commit::CommitRecord {
        commit_index: 1,
        leader_round: 1,
        height: 1,
        next_seq: 2,
        last_built_timestamp: 0,
        root_version: 1,
        closed: false,
        certs_closed: false,
    };
    handler.finish(&record, 1, true);
    committed.committed_through(1);
    drop((handler, committed));

    let (_, output, _) = EpochStore::open(dir.path(), 0);
    let mut handler = CommitHandler::new(output, 1, vec![([0; 96], 10_000)], &epoch.config, None);
    let decided = handler.handle(&epoch, commit(2, &[]), &bump).unwrap();
    assert_eq!(scheduled(&decided), vec![digest(&first)]);
}

/// The keys a commit's authenticator state update adds.
fn updated_keys(handled: &HandledCommit) -> Vec<String> {
    handled
        .schedule
        .transactions
        .iter()
        .filter_map(|t| match t {
            Scheduled::AuthenticatorStateUpdate(keys) => Some(keys),
            _ => None,
        })
        .flatten()
        .map(|(id, _)| id.kid.clone())
        .collect()
}

#[test]
fn a_key_a_quorum_voted_for_is_added_by_the_next_commit_across_a_restart() {
    use messages::build::system_transaction::{Jwk, JwkId};
    use validator::consensus::jwk::JwkVote;
    let bump = containers::Bump::default();
    let epoch = epoch();
    // Two of three: 6,667 of 10,000, a quorum.
    let members: Vec<_> = [3_334, 3_333, 3_333]
        .into_iter()
        .zip(0u8..)
        .map(|(stake, i)| ([i; 96], stake))
        .collect();
    let vote = |author: u8, kid: &str| JwkVote {
        author: u32::from(author),
        authority: [author; 96],
        id: JwkId {
            iss: "https://issuer".to_owned(),
            kid: kid.to_owned(),
        },
        jwk: Jwk {
            kty: "RSA".to_owned(),
            e: "AQAB".to_owned(),
            n: "n".to_owned(),
            alg: "RS256".to_owned(),
        },
    };
    let handler = |output| CommitHandler::new(output, 0, members.clone(), &epoch.config, Some(1));
    let with_votes = |index: u32, votes: Vec<JwkVote>| {
        let mut commit = commit(index, &[]);
        commit.jwks = votes;
        commit
    };
    let dir = tempfile::tempdir().unwrap();
    let (_, output, _) = EpochStore::open(dir.path(), 0);
    let committed = output.committed_checkpoints();
    let mut first = handler(output);
    // "a" becomes active in commit 1; "b" has one vote of two.
    let decided = first
        .handle(
            &epoch,
            with_votes(1, vec![vote(0, "a"), vote(1, "a"), vote(0, "b")]),
            &bump,
        )
        .unwrap();
    assert!(updated_keys(&decided).is_empty());
    let record = validator::consensus::commit::CommitRecord {
        commit_index: 1,
        leader_round: 1,
        height: 1,
        next_seq: 2,
        last_built_timestamp: 0,
        root_version: 1,
        closed: false,
        certs_closed: false,
    };
    first.finish(&record, 1, true);
    committed.committed_through(1);
    drop((first, committed));

    // Restarted: commit 2 adds "a", and a second vote makes "b" active,
    // which commit 3 adds.
    let (_, output, _) = EpochStore::open(dir.path(), 0);
    let mut second = CommitHandler::new(output, 1, members.clone(), &epoch.config, Some(1));
    let decided = second
        .handle(&epoch, with_votes(2, vec![vote(2, "b")]), &bump)
        .unwrap();
    assert_eq!(updated_keys(&decided), ["a"]);
    let decided = second.handle(&epoch, commit(3, &[]), &bump).unwrap();
    assert_eq!(updated_keys(&decided), ["b"]);
    let decided = second.handle(&epoch, commit(4, &[]), &bump).unwrap();
    assert!(updated_keys(&decided).is_empty());
}

#[test]
fn a_shared_gas_coin_conflicts_like_any_owned_input() {
    let bump = containers::Bump::default();
    let epoch = epoch();
    let sender = Sender::new();
    let gas = random_object_ref();
    let first = sender.transaction(gas, &[]);
    let second = sender.transaction(gas, &[random_object_ref()]);
    let (output, _dir) = consensus_output();
    let decided = new_handler(output)
        .handle(&epoch, commit(1, &[(&first, &[]), (&second, &[])]), &bump)
        .unwrap();
    assert_eq!(deferred(&decided), vec![digest(&first)]);
    assert_eq!(
        conflicts(&decided),
        vec![(digest(&second), wire(&gas), digest(&first))]
    );
}

#[test]
fn a_lock_holds_across_commits_for_its_version_only() {
    let bump = containers::Bump::default();
    let epoch = epoch();
    let sender = Sender::new();
    let object = random_object_ref();
    let mut next = object;
    next.1 = SequenceNumber::from_u64(object.1.value() + 1);
    let first = sender.transaction(random_object_ref(), &[object]);
    let again = sender.transaction(random_object_ref(), &[object]);
    let later = sender.transaction(random_object_ref(), &[next]);
    let (output, _dir) = consensus_output();
    let mut handler = new_handler(output);
    let decided = handler
        .handle(&epoch, commit(1, &[(&first, &[])]), &bump)
        .unwrap();
    assert_eq!(scheduled(&decided), vec![digest(&first)]);
    let decided = handler
        .handle(&epoch, commit(2, &[(&again, &[]), (&later, &[])]), &bump)
        .unwrap();
    assert_eq!(scheduled(&decided), vec![digest(&later)]);
    assert_eq!(
        conflicts(&decided),
        vec![(digest(&again), wire(&object), digest(&first))]
    );
}

#[test]
fn inputs_claimed_immutable_are_not_locked() {
    let bump = containers::Bump::default();
    let epoch = epoch();
    let sender = Sender::new();
    let frozen = random_object_ref();
    let claiming: Vec<_> = (0..2)
        .map(|_| sender.transaction(random_object_ref(), &[frozen]))
        .collect();
    let unclaimed: Vec<_> = (0..2)
        .map(|_| sender.transaction(random_object_ref(), &[frozen]))
        .collect();
    let claim = [frozen];
    let (output, _dir) = consensus_output();
    let decided = new_handler(output)
        .handle(
            &epoch,
            commit(
                1,
                &[
                    (&claiming[0], &claim),
                    (&claiming[1], &claim),
                    (&unclaimed[0], &[]),
                    (&unclaimed[1], &[]),
                ],
            ),
            &bump,
        )
        .unwrap();
    // The handler goes by the claim, which voting verified: without it, the
    // object is locked like an owned one.
    assert_eq!(
        scheduled(&decided),
        vec![digest(&claiming[0]), digest(&claiming[1])]
    );
    assert_eq!(deferred(&decided), vec![digest(&unclaimed[0])]);
    assert_eq!(
        conflicts(&decided),
        vec![(digest(&unclaimed[1]), wire(&frozen), digest(&unclaimed[0]))]
    );
}

#[test]
fn a_transaction_with_invalid_inputs_is_dropped_and_locks_nothing() {
    let bump = containers::Bump::default();
    let epoch = epoch();
    let sender = Sender::new();
    let object = random_object_ref();
    let twice = sender.transaction(random_object_ref(), &[object, object]);
    let valid = sender.transaction(random_object_ref(), &[object]);
    let (output, _dir) = consensus_output();
    let decided = new_handler(output)
        .handle(&epoch, commit(1, &[(&twice, &[]), (&valid, &[])]), &bump)
        .unwrap();
    assert_eq!(scheduled(&decided), vec![digest(&valid)]);
    let [(_, _, dropped, DropReason::Inputs(e))] = &decided.dropped[..] else {
        panic!("{:?}", decided.dropped);
    };
    assert_eq!(*dropped, digest(&twice));
    assert_eq!(e.kind, validation::ErrorKind::DuplicateObjectRefInput);
}

/// Everything a handler decided about a commit, as comparable text.
fn decisions(handled: &HandledCommit) -> String {
    let accesses: Vec<_> = (0..handled.schedule.len())
        .map(|i| handled.schedule.accesses(i).to_vec())
        .collect();
    format!(
        "{:?} {accesses:?} {:?} {:?} {:?}",
        scheduled(handled),
        handled.dropped,
        handled.duplicates,
        handled.deferred
    )
}

#[test]
fn handlers_fed_the_same_commits_decide_alike_and_a_replay_changes_nothing() {
    let bump = containers::Bump::default();
    let epoch = epoch();
    let sender = Sender::new();
    let objects: Vec<_> = (0..4).map(|_| random_object_ref()).collect();
    let transactions: Vec<_> = (0..12)
        .map(|i| sender.transaction(random_object_ref(), &[objects[i % 4], objects[(i + 1) % 4]]))
        .collect();
    let commits = || -> Vec<DecodedCommit> {
        transactions
            .chunks(4)
            .enumerate()
            .map(|(i, chunk)| {
                let claimed: Vec<Claimed<'_>> = chunk.iter().map(|t| (t, &[][..])).collect();
                commit(i as u32 + 1, &claimed)
            })
            .collect()
    };
    let decide = |handler: &mut CommitHandler| -> Vec<String> {
        commits()
            .into_iter()
            .map(|c| decisions(&handler.handle(&epoch, c, &bump).unwrap()))
            .collect()
    };
    let (output, _dir) = consensus_output();
    let mut handler = new_handler(output);
    let first = decide(&mut handler);
    let (other, _other_dir) = consensus_output();
    assert_eq!(first, decide(&mut new_handler(other)));
    // Consensus replays commits after a restart; those handled are skipped.
    for c in commits() {
        assert!(handler.handle(&epoch, c, &bump).is_none());
    }
    // The first commit's first transaction took two objects; its second,
    // sharing one, lost it, and the first waits a round.
    let (output, _dir) = consensus_output();
    let decided = new_handler(output)
        .handle(&epoch, commits().remove(0), &bump)
        .unwrap();
    assert_eq!(deferred(&decided)[0], digest(&transactions[0]));
    assert_eq!(decided.dropped[0].2, digest(&transactions[1]));
}

/// A `UserTransactionV2` of `transaction`, claiming no aliases and `immutable`
/// immutable.
fn user_transaction(transaction: &[u8], immutable: &[ObjectRef]) -> Vec<u8> {
    let mut bytes = vec![7; 8];
    bytes.push(12);
    bytes.extend_from_slice(transaction);
    bytes.push(2);
    // `AddressAliasesV2`: the one signature, no alias.
    bytes.extend_from_slice(&[2, 1, 0, 0]);
    // `ImmutableInputObjects`.
    bytes.extend_from_slice(&[1, immutable.len() as u8]);
    for r in immutable {
        bytes.extend_from_slice(&r.0.into_bytes());
    }
    bytes
}

#[test]
fn decoding_carries_the_immutable_claims() {
    let bump = containers::Bump::default();
    let epoch = epoch();
    let sender = Sender::new();
    let frozen = random_object_ref();
    let owned = random_object_ref();
    let gas = random_object_ref();
    let bytes = sender.bytes(gas, &[frozen, owned]);
    let block = Block {
        reference: BlockRef {
            round: 1,
            author: 1,
            digest: [1; 32],
        },
        transactions: vec![user_transaction(&bytes, &[frozen])],
    };
    let mut decoder = CommitDecoder::new(epoch.clone(), Arc::new(ConsensusTxCache::new()));
    let commit = decoder.decode(
        CommittedSubDag {
            commit_index: 1,
            digest: [0; 32],
            leader: block.reference,
            timestamp_ms: 0,
            blocks: vec![block],
            rejected: vec![],
        },
        &bump,
    );
    assert_eq!(commit.cache_misses, 1);
    let locked = commit.transactions[0].owned.as_ref().unwrap();
    assert_eq!(locked, &vec![wire(&owned), wire(&gas)]);
}

#[test]
fn the_stage_leaves_dropped_transactions_out_of_its_chunks() {
    let bump = containers::Bump::default();
    let epoch = epoch();
    let sender = Sender::new();
    let object = random_object_ref();
    let first = sender.transaction(random_object_ref(), &[object]);
    let second = sender.transaction(random_object_ref(), &[object]);
    let (chunks, _chunk_inbox) = workqueue::queue(16);
    let (settlements, _settlement_inbox) = workqueue::queue(16);
    let (checkpoints, _checkpoint_inbox) = workqueue::queue(16);
    let (output, _dir) = consensus_output();
    let mut stage = CommitStage::new(
        epoch.clone(),
        EpochStart {
            previous_checkpoint: 0,
            accumulator_root_initial_shared_version: 1,
            authenticator_initial_shared_version: None,
            accumulator_root_version: 1,
            epoch_start_timestamp_ms: 0,
            epoch_duration_ms: u64::MAX,
        },
        output,
        chunks,
        settlements,
        checkpoints,
        Arc::new(validator::consensus::status::ConsensusTxStatusCache::new()),
        vec![([0; 96], 1)],
    );
    let users = |decided: &validator::consensus::commit::Decided| -> Vec<Hashed> {
        decided
            .chunks
            .iter()
            .flat_map(|c| &c.entries)
            .filter_map(|e| match &e.transaction {
                ExecutableTransaction::User(t) => Some(digest_of(t)),
                ExecutableTransaction::System(_) => None,
            })
            .collect()
    };
    let decided = stage
        .decide(commit(1, &[(&first, &[]), (&second, &[])]), &bump)
        .unwrap();
    // The first, its lock contested, is in the next round's chunks.
    assert!(users(&decided).is_empty());
    let next = stage.decide(commit(2, &[]), &bump).unwrap();
    assert_eq!(users(&next), vec![digest(&first)]);
    assert!(
        decided
            .settlements
            .iter()
            .all(|s| !s.digests.contains(&digest(&second)))
    );
    assert_eq!(decided.dropped.len(), 1);
    assert_eq!(decided.dropped[0].2, digest(&second));
}

/// Once its time is up, the epoch waits for its deferred transactions:
/// the commit at the end time defers one and is not the last; the next
/// schedules it, drops its own user transactions, and is.
#[test]
fn the_epoch_ends_once_its_deferred_transactions_are_scheduled() {
    let bump = containers::Bump::default();
    let epoch = epoch();
    let sender = Sender::new();
    let object = random_object_ref();
    let first = sender.transaction(random_object_ref(), &[object]);
    let second = sender.transaction(random_object_ref(), &[object]);
    let third = sender.transaction(random_object_ref(), &[]);
    let (output, _dir) = consensus_output();
    let mut handler = new_handler(output).with_end(1000, false);
    let decided = handler
        .handle(&epoch, commit(1, &[(&first, &[]), (&second, &[])]), &bump)
        .unwrap();
    assert_eq!(deferred(&decided), vec![digest(&first)]);
    assert!(!decided.final_commit);
    assert!(handler.certs_closed());

    let decided = handler
        .handle(&epoch, commit(2, &[(&third, &[])]), &bump)
        .unwrap();
    assert_eq!(scheduled(&decided), vec![digest(&first)]);
    assert!(matches!(
        &decided.dropped[..],
        [(_, _, dropped, DropReason::EpochClosed)] if *dropped == digest(&third)
    ));
    assert!(decided.final_commit);
}

/// A commit past the close deadline is the epoch's last even with
/// deferred transactions, which are abandoned.
#[test]
fn the_close_deadline_abandons_deferred_transactions() {
    let bump = containers::Bump::default();
    let epoch = epoch();
    let deadline = epoch.config.epoch_close_deadline_ms();
    let sender = Sender::new();
    let object = random_object_ref();
    let first = sender.transaction(random_object_ref(), &[object]);
    let second = sender.transaction(random_object_ref(), &[object]);
    let (output, _dir) = consensus_output();
    let mut handler = new_handler(output).with_end(1000, false);
    // Commit `n` is at `1000 * n` ms.
    let past_deadline = u32::try_from((1000 + deadline).div_ceil(1000)).unwrap();
    let decided = handler
        .handle(
            &epoch,
            commit(past_deadline, &[(&first, &[]), (&second, &[])]),
            &bump,
        )
        .unwrap();
    assert_eq!(deferred(&decided), vec![digest(&first)]);
    assert!(decided.final_commit);
}
