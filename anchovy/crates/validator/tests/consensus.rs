// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Voting on hand-built blocks of signed transactions against a funded
//! genesis, and handling hand-built commits of them.

use std::sync::Arc;

use consensus::{Block, BlockRef, CommittedSubDag, TransactionIndex};
use messages::base::Digest;
use protocol_config::{Chain, ProtocolVersion};
use sui_types::base_types::{ObjectID, ObjectRef, SuiAddress};
use sui_types::crypto::{AccountKeyPair, AuthorityPublicKeyBytes, get_key_pair};
use sui_types::digests::TransactionDigest;
use sui_types::messages_consensus::ConsensusTransaction;
use sui_types::object::Object;
use sui_types::transaction::{PlainTransactionWithClaims, Transaction, TransactionData};
use validator::consensus::cache::ConsensusTxCache;
use validator::consensus::commit::{CommitHandler, DropReason, HandledCommit};
use validator::consensus::decode::CommitDecoder;
use validator::consensus::vote::BlockVoter;
use validator::epoch::EpochState;
use validator::epoch_store::{ConsensusOutput, EpochStore};

const RGP: u64 = 1000;
const BUDGET: u64 = 50_000_000;
const SUI: u64 = 1_000_000_000;

/// The one writer of a fresh epoch store's consensus output, and the
/// directory the store is in.
fn consensus_output() -> (ConsensusOutput, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let (_, output) = EpochStore::open(dir.path(), 0);
    (output, dir)
}

struct Node {
    _dir: tempfile::TempDir,
    epoch: Arc<EpochState>,
    store: Arc<store::Store>,
    cache: Arc<ConsensusTxCache>,
    sender: SuiAddress,
    key: AccountKeyPair,
}

impl Node {
    fn new() -> Node {
        let (sender, key) = get_key_pair::<AccountKeyPair>();
        let epoch = Arc::new(EpochState::new(
            Chain::Unknown,
            ProtocolVersion::MAX.as_u64(),
            0,
            Digest::new([0; 32]),
            RGP,
            4,
            [],
        ));
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(store::Store::open(dir.path()).unwrap());
        let funded = [(sender.to_inner(), 10 * SUI)];
        assert!(execution::genesis::init(epoch.execution.reference(), &store, &funded).unwrap());
        Node {
            _dir: dir,
            epoch,
            store,
            cache: Arc::new(ConsensusTxCache::new()),
            sender,
            key,
        }
    }

    fn voter(&self) -> BlockVoter {
        BlockVoter::new(self.epoch.clone(), self.store.clone(), self.cache.clone())
    }

    /// The sender's gas coin, as of now.
    fn gas(&self) -> ObjectRef {
        let id = ObjectID::derive_id(TransactionDigest::genesis_marker(), 0);
        let object = self
            .store
            .live_object(&messages::base::ObjectId(id.into_bytes()))
            .unwrap()
            .unwrap();
        bcs::from_bytes::<Object>(object.wire_bytes())
            .unwrap()
            .compute_object_reference()
    }

    /// A signed transfer paid with `gas`, as `Transaction` BCS.
    fn transfer(&self, gas: ObjectRef) -> Vec<u8> {
        self.signed_transfer(gas, &self.key)
    }

    fn signed_transfer(&self, gas: ObjectRef, key: &AccountKeyPair) -> Vec<u8> {
        self.transfer_at(gas, key, RGP)
    }

    /// A transfer paid with `gas` at gas price `price`, signed with `key`.
    fn transfer_at(&self, gas: ObjectRef, key: &AccountKeyPair, price: u64) -> Vec<u8> {
        let data = TransactionData::new_transfer_sui(
            SuiAddress::random_for_testing_only(),
            self.sender,
            Some(SUI),
            gas,
            BUDGET,
            price,
        );
        bcs::to_bytes(&Transaction::from_data_and_signer(data, vec![key])).unwrap()
    }
}

/// A `UserTransactionV2` of `transaction` with `claims`, each claim's BCS.
fn user_transaction(transaction: &[u8], claims: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = vec![7; 8];
    bytes.push(12);
    bytes.extend_from_slice(transaction);
    bytes.push(claims.len() as u8);
    for claim in claims {
        bytes.extend_from_slice(claim);
    }
    bytes
}

/// `TransactionClaim::AddressAliasesV2`.
fn aliases(entries: &[(u8, Option<u64>)]) -> Vec<u8> {
    let mut bytes = vec![2, entries.len() as u8];
    for (index, version) in entries {
        bytes.push(*index);
        match version {
            None => bytes.push(0),
            Some(v) => {
                bytes.push(1);
                bytes.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    bytes
}

/// A single-signer transaction's claim that it signs with its one signature
/// and no alias.
fn no_alias() -> Vec<u8> {
    aliases(&[(0, None)])
}

fn block(round: u32, transactions: Vec<Vec<u8>>) -> Block {
    Block {
        reference: BlockRef {
            round,
            author: 1,
            digest: [round as u8; 32],
        },
        transactions,
    }
}

#[test]
fn the_handwritten_encoding_is_sui_s() {
    let node = Node::new();
    let transaction = node.transfer(node.gas());
    let sui = ConsensusTransaction::new_user_transaction_v2_message(
        &AuthorityPublicKeyBytes::ZERO,
        PlainTransactionWithClaims::no_aliases(bcs::from_bytes(&transaction).unwrap()),
    );
    let mut ours = user_transaction(&transaction, &[]);
    ours[..8].copy_from_slice(&sui.tracking_id);
    assert_eq!(bcs::to_bytes(&sui).unwrap(), ours);
}

#[test]
fn a_valid_transfer_is_accepted_and_cached() {
    let node = Node::new();
    let block = block(
        1,
        vec![user_transaction(&node.transfer(node.gas()), &[no_alias()])],
    );
    let reference = block.reference;
    assert_eq!(node.voter().vote(block), Ok(vec![]));
    let entries = node.cache.take(&reference).unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].is_some());
}

#[test]
fn a_signature_that_does_not_verify_fails_alone() {
    let node = Node::new();
    let gas = node.gas();
    let mut corrupt = node.transfer(gas);
    // The signature's `R`: it still parses, but does not verify, so the
    // block's batch fails and each transaction is verified on its own.
    let at = corrupt.len() - 96;
    corrupt[at] ^= 1;
    let block = block(
        8,
        vec![
            user_transaction(&node.transfer(gas), &[no_alias()]),
            user_transaction(&corrupt, &[no_alias()]),
            user_transaction(&node.transfer(gas), &[no_alias()]),
        ],
    );
    let reference = block.reference;
    let mut voter = node.voter();
    assert_eq!(voter.vote(block.clone()), Ok(vec![1]));
    let entries = node.cache.take(&reference).unwrap();
    assert!(entries[0].is_some() && entries[1].is_none() && entries[2].is_some());
    // Again, the valid two from the signature cache.
    assert_eq!(voter.vote(block), Ok(vec![1]));
}

#[test]
fn bad_signatures_and_spent_inputs_are_rejected() {
    let node = Node::new();
    let gas = node.gas();
    let (_, other_key) = get_key_pair::<AccountKeyPair>();
    let mut stale = gas;
    stale.1 = stale.1.next();
    let block = block(
        2,
        vec![
            user_transaction(&node.signed_transfer(gas, &other_key), &[no_alias()]),
            user_transaction(&node.transfer(stale), &[no_alias()]),
            user_transaction(&node.transfer(gas), &[no_alias()]),
        ],
    );
    let reference = block.reference;
    assert_eq!(node.voter().vote(block), Ok(vec![0, 1]));
    // A bad signature is not kept; a transaction whose inputs failed is, as a
    // quorum may still accept it.
    let entries = node.cache.take(&reference).unwrap();
    assert!(entries[0].is_none());
    assert!(entries[1].is_some());
    assert!(entries[2].is_some());
}

#[test]
fn alias_claims_must_match_the_signatures() {
    let node = Node::new();
    let transaction = node.transfer(node.gas());
    // A claimed alias version where there is none: rejected.
    let changed = block(
        3,
        vec![user_transaction(&transaction, &[aliases(&[(0, Some(5))])])],
    );
    assert_eq!(node.voter().vote(changed), Ok(vec![0]));
    // No claim, or one naming a signature the transaction lacks: the block is
    // invalid.
    for claims in [vec![], vec![aliases(&[(1, None)])]] {
        let invalid = block(4, vec![user_transaction(&transaction, &claims)]);
        let verdict = node.voter().vote(invalid).unwrap_err();
        assert_eq!(verdict.index, 0);
    }
}

#[test]
fn an_executed_transaction_is_accepted_though_its_inputs_are_spent() {
    let node = Node::new();
    let transaction = node.transfer(node.gas());
    let outcome = node
        .epoch
        .execution
        .execute(&node.store, &transaction)
        .unwrap();
    node.store.write_through(outcome.commit).unwrap();
    let block = block(5, vec![user_transaction(&transaction, &[no_alias()])]);
    assert_eq!(node.voter().vote(block), Ok(vec![]));
}

/// `TransactionClaim::ImmutableInputObjects`.
fn immutable(ids: &[ObjectID]) -> Vec<u8> {
    let mut bytes = vec![1, ids.len() as u8];
    for id in ids {
        bytes.extend_from_slice(&id.into_bytes());
    }
    bytes
}

#[test]
fn a_false_immutable_claim_is_rejected_and_cached_as_claimed() {
    let node = Node::new();
    let gas = node.gas();
    let block = block(
        9,
        vec![user_transaction(
            &node.transfer(gas),
            &[no_alias(), immutable(&[gas.0])],
        )],
    );
    let reference = block.reference;
    assert_eq!(node.voter().vote(block), Ok(vec![0]));
    // The handler locks by the claim, whatever this validator voted: if a
    // quorum accepts it, the coin it claims immutable is not locked.
    let entries = node.cache.take(&reference).unwrap();
    let owned = entries[0].as_ref().unwrap().owned.as_ref().unwrap();
    assert!(owned.is_empty());
}

/// As the reference's: an executed transaction's inputs are not checked,
/// but its immutable claim is, against the live objects.
#[test]
fn an_executed_transaction_s_immutable_claim_is_still_verified() {
    let node = Node::new();
    let gas = node.gas();
    let transaction = node.transfer(gas);
    let outcome = node
        .epoch
        .execution
        .execute(&node.store, &transaction)
        .unwrap();
    node.store.write_through(outcome.commit).unwrap();
    let block = block(
        10,
        vec![user_transaction(
            &transaction,
            &[no_alias(), immutable(&[gas.0])],
        )],
    );
    assert_eq!(node.voter().vote(block), Ok(vec![0]));
}

#[test]
fn undecodable_and_deprecated_transactions_invalidate_the_block() {
    let node = Node::new();
    let transaction = node.transfer(node.gas());
    let mut truncated = user_transaction(&transaction, &[no_alias()]);
    truncated.pop();
    let mut v1 = vec![7; 8];
    v1.push(9);
    v1.extend_from_slice(&transaction);
    for bad in [truncated, v1] {
        let block = block(6, vec![user_transaction(&transaction, &[no_alias()]), bad]);
        let reference = block.reference;
        let verdict = node.voter().vote(block).unwrap_err();
        assert_eq!(verdict.index, 1, "{}", verdict.reason);
        assert!(node.cache.take(&reference).is_none());
    }
}

#[test]
fn other_kinds_are_accepted_and_not_cached() {
    let node = Node::new();
    let end_of_publish = bcs::to_bytes(&ConsensusTransaction::new_end_of_publish(
        AuthorityPublicKeyBytes::ZERO,
    ))
    .unwrap();
    let block = block(7, vec![end_of_publish]);
    let reference = block.reference;
    assert_eq!(node.voter().vote(block), Ok(vec![]));
    assert_eq!(node.cache.take(&reference).unwrap().len(), 1);
}

fn commit(
    index: u32,
    leader_round: u32,
    blocks: Vec<Block>,
    rejected: Vec<(BlockRef, Vec<TransactionIndex>)>,
) -> CommittedSubDag {
    CommittedSubDag {
        commit_index: index,
        digest: [index as u8; 32],
        leader: blocks.last().map_or(
            BlockRef {
                round: leader_round,
                author: 0,
                digest: [0; 32],
            },
            |b| BlockRef {
                round: leader_round,
                ..b.reference
            },
        ),
        timestamp_ms: 0,
        blocks,
        rejected,
    }
}

/// Commit decoding then handling, as `CommitStage` runs them.
struct Committer {
    decoder: CommitDecoder,
    handler: CommitHandler,
    _dir: tempfile::TempDir,
}

impl Committer {
    fn handle(&mut self, epoch: &EpochState, commit: CommittedSubDag) -> Option<HandledCommit> {
        let decoded = self.decoder.decode(commit);
        self.handler.handle(&epoch.params, decoded)
    }
}

fn committer(node: &Node) -> Committer {
    let (output, dir) = consensus_output();
    Committer {
        decoder: CommitDecoder::new(node.epoch.params.clone(), node.cache.clone()),
        handler: CommitHandler::new(output),
        _dir: dir,
    }
}

/// The positions of the user transactions `handled` scheduled, in schedule
/// order.
fn scheduled(handled: &HandledCommit) -> Vec<TransactionIndex> {
    handled
        .schedule
        .transactions
        .iter()
        .filter_map(|t| match t {
            validator::consensus::sequence::Scheduled::User(t) => {
                Some(handled.origins[&t.get().0.digest().bytes].1)
            }
            validator::consensus::sequence::Scheduled::Prologue(_) => None,
        })
        .collect()
}

/// The positions of the transactions `handled` dropped for a lock conflict.
fn conflicted(handled: &HandledCommit) -> Vec<TransactionIndex> {
    handled
        .dropped
        .iter()
        .filter(|(.., reason)| matches!(reason, DropReason::LockConflict { .. }))
        .map(|(_, index, ..)| *index)
        .collect()
}

#[test]
fn a_voted_block_s_transactions_come_from_the_cache() {
    let node = Node::new();
    let block = block(
        1,
        vec![user_transaction(&node.transfer(node.gas()), &[no_alias()])],
    );
    assert_eq!(node.voter().vote(block.clone()), Ok(vec![]));
    let handled = committer(&node)
        .handle(&node.epoch, commit(1, 1, vec![block], vec![]))
        .unwrap();
    assert_eq!((handled.cache_hits, handled.cache_misses), (1, 0));
    assert_eq!(scheduled(&handled), [0]);
    assert!(node.cache.is_empty());
}

#[test]
fn an_unvoted_block_s_transactions_are_decoded() {
    let node = Node::new();
    let end_of_publish = bcs::to_bytes(&ConsensusTransaction::new_end_of_publish(
        AuthorityPublicKeyBytes::ZERO,
    ))
    .unwrap();
    let block = block(
        1,
        vec![
            end_of_publish,
            user_transaction(&node.transfer(node.gas()), &[no_alias()]),
        ],
    );
    let handled = committer(&node)
        .handle(&node.epoch, commit(1, 1, vec![block], vec![]))
        .unwrap();
    assert_eq!((handled.cache_hits, handled.cache_misses), (0, 1));
    assert_eq!(scheduled(&handled), [1]);
}

#[test]
fn rejected_transactions_are_skipped() {
    let node = Node::new();
    let block = block(
        1,
        vec![user_transaction(&node.transfer(node.gas()), &[no_alias()])],
    );
    let rejected = vec![(block.reference, vec![0])];
    let handled = committer(&node)
        .handle(&node.epoch, commit(1, 1, vec![block], rejected))
        .unwrap();
    assert!(scheduled(&handled).is_empty());
}

#[test]
fn transactions_and_commits_are_handled_once() {
    let node = Node::new();
    let transaction = user_transaction(&node.transfer(node.gas()), &[no_alias()]);
    let first = block(1, vec![transaction.clone()]);
    let second = Block {
        reference: BlockRef {
            author: 2,
            ..first.reference
        },
        ..first.clone()
    };
    let mut committer = committer(&node);
    let handled = committer
        .handle(
            &node.epoch,
            commit(1, 1, vec![first.clone(), second.clone()], vec![]),
        )
        .unwrap();
    assert_eq!(scheduled(&handled), [0]);
    assert_eq!(handled.duplicates.len(), 1);
    assert_eq!(handled.duplicates[0].0, second.reference);
    // Replayed: nothing to do.
    assert!(
        committer
            .handle(&node.epoch, commit(1, 1, vec![first], vec![]))
            .is_none()
    );
    // In a later commit: processed already.
    let later = block(2, vec![transaction]);
    let handled = committer
        .handle(&node.epoch, commit(2, 2, vec![later], vec![]))
        .unwrap();
    assert!(scheduled(&handled).is_empty());
    assert_eq!(handled.duplicates.len(), 1);
}

#[test]
fn the_first_of_two_transactions_spending_one_coin_wins() {
    let node = Node::new();
    let gas = node.gas();
    let first = user_transaction(&node.transfer(gas), &[no_alias()]);
    let second = user_transaction(&node.transfer(gas), &[no_alias()]);
    let block = block(1, vec![first, second]);
    // Both pass voting: neither has executed when the block is voted on.
    assert_eq!(node.voter().vote(block.clone()), Ok(vec![]));
    let handled = committer(&node)
        .handle(&node.epoch, commit(1, 1, vec![block], vec![]))
        .unwrap();
    assert_eq!(scheduled(&handled), [0]);
    assert_eq!(conflicted(&handled), [1]);
}

#[test]
fn blocks_below_the_gc_round_are_evicted() {
    let node = Node::new();
    let depth = node.epoch.params.config.gc_depth();
    assert!(depth > 0);
    let old = block(1, vec![]);
    let recent = block(depth + 2, vec![]);
    let leader = block(depth + 3, vec![]);
    for b in [&old, &recent] {
        assert_eq!(node.voter().vote(b.clone()), Ok(vec![]));
    }
    committer(&node)
        .handle(
            &node.epoch,
            commit(1, leader.reference.round, vec![leader], vec![]),
        )
        .unwrap();
    assert!(node.cache.take(&old.reference).is_none());
    assert!(node.cache.take(&recent.reference).is_some());
}

#[test]
fn owned_inputs_are_locked_in_consensus_order_before_gas_price_order() {
    let node = Node::new();
    let gas = node.gas();
    let cheap = node.transfer_at(gas, &node.key, RGP);
    let dear = node.transfer_at(gas, &node.key, 2 * RGP);
    let block = block(
        1,
        vec![
            user_transaction(&cheap, &[no_alias()]),
            user_transaction(&dear, &[no_alias()]),
        ],
    );
    let handled = committer(&node)
        .handle(&node.epoch, commit(1, 1, vec![block], vec![]))
        .unwrap();
    // Owned inputs are locked in consensus order, before the schedule's
    // gas price order: the cheaper transaction, first in the block, takes
    // the coin, and the dearer one is dropped unscheduled.
    assert_eq!(scheduled(&handled), [0]);
    assert_eq!(conflicted(&handled), [1]);
}

/// The handler is a replicated state machine: two handlers fed the same
/// commits decide the same schedule, ordinals included, whatever has
/// executed. Here one validator executed the commit's transaction already
/// (before a restart, say); its handler still schedules it.
#[test]
fn handlers_decide_alike_whatever_has_executed() {
    let node = Node::new();
    let transaction = node.transfer(node.gas());
    let outcome = node
        .epoch
        .execution
        .execute(&node.store, &transaction)
        .unwrap();
    node.store.write_through(outcome.commit).unwrap();
    let commits = [
        commit(
            1,
            1,
            vec![block(
                1,
                vec![user_transaction(&transaction, &[no_alias()])],
            )],
            vec![],
        ),
        commit(
            2,
            2,
            vec![block(
                2,
                vec![user_transaction(&node.transfer(node.gas()), &[no_alias()])],
            )],
            vec![],
        ),
    ];
    let decide = || {
        let mut decoder = CommitDecoder::new(node.epoch.params.clone(), node.cache.clone());
        let (output, _dir) = consensus_output();
        let mut handler = CommitHandler::new(output);
        commits
            .iter()
            .map(|c| {
                let decided = handler
                    .handle(&node.epoch.params, decoder.decode(c.clone()))
                    .unwrap();
                let order: Vec<_> = decided
                    .schedule
                    .transactions
                    .iter()
                    .map(|t| match t {
                        validator::consensus::sequence::Scheduled::User(t) => {
                            Some(t.get().0.digest().bytes)
                        }
                        validator::consensus::sequence::Scheduled::Prologue(_) => None,
                    })
                    .collect();
                let accesses: Vec<_> = (0..decided.schedule.len())
                    .map(|i| decided.schedule.accesses(i).to_vec())
                    .collect();
                (order, accesses)
            })
            .collect::<Vec<_>>()
    };
    let first = decide();
    assert_eq!(first, decide());
    // The executed transaction is scheduled, not dropped as a duplicate:
    // the executor finds it executed.
    assert_eq!(first[0].0.len(), 2);
}
