// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Voting on hand-built blocks of signed transactions against a funded
//! genesis.

use std::sync::Arc;

use consensus::{Block, BlockRef};
use messages::base::Digest;
use protocol_config::{Chain, ProtocolVersion};
use sui_types::base_types::{ObjectID, ObjectRef, SuiAddress};
use sui_types::crypto::{AccountKeyPair, AuthorityPublicKeyBytes, get_key_pair};
use sui_types::digests::TransactionDigest;
use sui_types::messages_consensus::ConsensusTransaction;
use sui_types::object::Object;
use sui_types::transaction::{PlainTransactionWithClaims, Transaction, TransactionData};
use validator::consensus::cache::ConsensusTxCache;
use validator::consensus::vote::BlockVoter;
use validator::epoch::EpochState;

const RGP: u64 = 1000;
const BUDGET: u64 = 50_000_000;
const SUI: u64 = 1_000_000_000;

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
        assert!(execution::genesis::init(&epoch.execution, &store, &funded).unwrap());
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
        BlockVoter::new(self.store.clone(), self.cache.clone())
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
        let data = TransactionData::new_transfer_sui(
            SuiAddress::random_for_testing_only(),
            self.sender,
            Some(SUI),
            gas,
            BUDGET,
            RGP,
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
    assert_eq!(node.voter().vote(&node.epoch, block), Ok(vec![]));
    let entries = node.cache.take(&reference).unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].is_some());
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
    assert_eq!(node.voter().vote(&node.epoch, block), Ok(vec![0, 1]));
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
    assert_eq!(node.voter().vote(&node.epoch, changed), Ok(vec![0]));
    // No claim, or one naming a signature the transaction lacks: the block is
    // invalid.
    for claims in [vec![], vec![aliases(&[(1, None)])]] {
        let invalid = block(4, vec![user_transaction(&transaction, &claims)]);
        let verdict = node.voter().vote(&node.epoch, invalid).unwrap_err();
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
    node.store.commit(outcome.commit).unwrap();
    let block = block(5, vec![user_transaction(&transaction, &[no_alias()])]);
    assert_eq!(node.voter().vote(&node.epoch, block), Ok(vec![]));
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
        let verdict = node.voter().vote(&node.epoch, block).unwrap_err();
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
    assert_eq!(node.voter().vote(&node.epoch, block), Ok(vec![]));
    assert_eq!(node.cache.take(&reference).unwrap().len(), 1);
}
