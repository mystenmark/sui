// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Submission: `SubmitChecker` readies a transaction for consensus as voters
//! accept it, or answers it at once if executed already or failing the
//! input checks; a wait for its effects is woken once it executes.

mod common;

use std::sync::Arc;

use common::Node;
use consensus::{Block, BlockRef};
use exec_pool::{EffectsAvailability, EffectsRouter};
use messages::Message;
use messages::transaction::DigestPending;
use validator::consensus::cache::ConsensusTxCache;
use validator::consensus::vote::BlockVoter;
use validator::pipeline::items::{EffectsWaitItem, SettlementInput};
use validator::pipeline::keys::EffectsKey;
use validator::processors::{Outcome, Rejected, Request, SignatureVerifier, TransactionValidator};
use validator::submit::SubmitChecker;
use waiter::{Command, WaitBatch, WaiterProcessor};
use workqueue::{Processor, Sink};

type Answer = tokio::sync::oneshot::Receiver<Result<Vec<Outcome>, Rejected>>;

struct Submission {
    checker: SubmitChecker,
    validator: TransactionValidator,
    verifier: SignatureVerifier,
}

impl Submission {
    fn new(node: &Node) -> Submission {
        Submission {
            checker: SubmitChecker::new(node.store.clone()),
            validator: TransactionValidator::new(),
            verifier: SignatureVerifier::new(),
        }
    }

    /// Submits `transaction` (`Transaction` BCS) through validation and
    /// signature verification.
    fn submit(&mut self, node: &Node, transaction: &[u8]) -> Answer {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let parsed: Message<messages::transaction::Transaction<'static, DigestPending>> =
            Message::parse(transaction.to_vec())
                .map_err(|(e, _)| e)
                .unwrap();
        let request = Request::new(node.epoch.clone(), vec![parsed], reply);
        let verified = self
            .validator
            .process(request)
            .and_then(|valid| self.verifier.process(valid))
            .expect("valid and verified");
        self.checker.process(verified);
        answer
    }
}

fn executed_digest(outcome: &Outcome) -> [u8; 32] {
    match outcome {
        Outcome::Executed(executed) => executed.effects_digest,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_transaction_is_readied_for_consensus_and_its_wait_woken_once_executed() {
    let mut node = Node::new(1);
    let transfer = node.transfer(node.coin(0));
    let bytes = transfer.wire_bytes().to_vec();
    let digest = common::digest(&transfer);
    let mut submission = Submission::new(&node);

    // Readied as voters accept it.
    let outcomes = submission
        .submit(&node, &bytes)
        .try_recv()
        .unwrap()
        .unwrap();
    let [Outcome::Submit(consensus_bytes)] = &outcomes[..] else {
        panic!("{outcomes:?}");
    };
    let mut voter = BlockVoter::new(node.store.clone(), Arc::new(ConsensusTxCache::new()));
    let block = Block {
        reference: BlockRef {
            round: 1,
            author: 0,
            digest: [1; 32],
        },
        transactions: vec![consensus_bytes.clone()],
    };
    assert_eq!(voter.vote(&node.epoch, block), Ok(vec![]));

    // A wait for its effects is woken once it executes.
    let (settlement, settlement_inbox) = workqueue::queue::<SettlementInput>(16);
    let mut router = EffectsRouter {
        settlement,
        epoch_closer: workqueue::queue(1).0,
    };
    let mut waiter = WaiterProcessor::new(EffectsAvailability {
        store: node.store.clone(),
    });
    let (reply, mut woken) = tokio::sync::oneshot::channel();
    let mut batch = WaitBatch::new();
    batch.push(EffectsWaitItem::Executed(reply), [EffectsKey(digest)]);
    assert!(waiter.process(Command::Wait(batch)).is_none());
    let executable = node.executable(transfer, vec![], vec![]);
    node.executor.process(vec![executable]);
    let notify = node.notified_effects.try_pop().expect("effects notified");
    router.send(waiter.process(notify).expect("the wait is ready"));
    assert_eq!(woken.try_recv(), Ok(()));
    assert!(settlement_inbox.try_pop().is_none());

    // Submitted again, it is answered from the store.
    let effects_digest = node
        .store
        .executed_effects(&messages::base::Digest::new(digest))
        .unwrap()
        .unwrap();
    let outcomes = submission
        .submit(&node, &bytes)
        .try_recv()
        .unwrap()
        .unwrap();
    assert_eq!(executed_digest(&outcomes[0]), effects_digest.bytes);
}

#[test]
fn a_transaction_failing_the_input_checks_is_answered_at_once() {
    let mut node = Node::new(1);
    let stale = node.coin(0);
    let spent = node.transfer(stale);
    node.execute_owned(spent);
    // Paid with a version the first transfer consumed.
    let late = node.transfer(stale);
    let mut submission = Submission::new(&node);
    let mut answer = submission.submit(&node, late.wire_bytes());

    let outcomes = answer.try_recv().unwrap().unwrap();
    assert!(
        matches!(outcomes[..], [Outcome::Rejected(_)]),
        "{outcomes:?}"
    );
}

#[test]
fn settlement_work_is_routed_to_the_settlement_scheduler() {
    let (settlement, inbox) = workqueue::queue(16);
    let mut router = EffectsRouter {
        settlement,
        epoch_closer: workqueue::queue(1).0,
    };
    router.send(vec![EffectsWaitItem::SettlementExecuted {
        height: 3,
        digests: vec![[9; 32]],
    }]);
    let Some(SettlementInput::SettlementExecuted { height, digests }) = inbox.try_pop() else {
        panic!("routed to settlement");
    };
    assert_eq!((height, digests), (3, vec![[9; 32]]));
}
