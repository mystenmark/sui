// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Certified transactions through the input waiter: executed on a node from
//! the same genesis at the versions their effects give, whatever order they
//! arrive in, to the same effects.

mod common;

use common::{Node, digest};
use messages::Message;
use messages::base::Hashed;
use messages::checkpoint::ExecutionData;
use messages::effects::TransactionEffects;
use messages::transaction::SenderSignedData;
use validator::pipeline::items::Executable;
use validator::pipeline::keys::InputKey;

/// What `node` executed: each transaction's and its effects' BCS.
fn executed(node: &Node, digests: &[Hashed]) -> Vec<(Vec<u8>, Vec<u8>)> {
    digests
        .iter()
        .map(|d| {
            let effects_digest = node.store.executed_effects(d).unwrap().unwrap();
            let transaction = node.store.transaction(d.digest()).unwrap().unwrap();
            let effects = node.store.effects(&effects_digest).unwrap().unwrap();
            (
                transaction.wire_bytes().to_vec(),
                effects.wire_bytes().to_vec(),
            )
        })
        .collect()
}

/// The synced executables for `executed`, with their keys.
fn certified(executed: &[(Vec<u8>, Vec<u8>)]) -> Vec<(Executable, Vec<InputKey>)> {
    executed
        .iter()
        .map(|(transaction, effects)| {
            let transaction = Message::<SenderSignedData>::parse(transaction.clone()).unwrap();
            let effects = Message::<TransactionEffects>::parse(effects.clone()).unwrap();
            let t = ExecutionData {
                transaction: *transaction.get(),
                effects: *effects.get(),
            };
            let (mut keys, mut packages) = (Vec::new(), Vec::new());
            let executable = synced::item::executable(
                &t,
                1,
                1,
                |_, _, _| unreachable!("no stream ended"),
                &mut keys,
                &mut packages,
            );
            (executable, keys)
        })
        .collect()
}

#[test]
fn certified_transactions_execute_at_their_certified_versions() {
    let mut node = Node::new(4);
    let mut twin = node.twin(4);
    let (shared, share) = node.new_shared_by(0);
    let first = node.increment(node.coin(1), shared);
    let read = node.read(node.coin(2), shared);
    let second = node.increment(node.coin(3), shared);
    let digests = [share, digest(&first), digest(&read), digest(&second)];
    let executables = vec![
        node.executable(first, vec![shared.access(true, 5, 0)], vec![]),
        node.executable(read, vec![shared.access(false, 5, 1)], vec![]),
        node.executable(second, vec![shared.access(true, 5, 1)], vec![]),
    ];
    common::InputWaiter::new(&node).run(&mut node, executables);
    let executed = executed(&node, &digests);

    // In reverse: each waits for the versions the one before writes.
    let mut keyed = certified(&executed);
    keyed.reverse();
    let mut waiter = common::InputWaiter::new(&twin);
    waiter.run_keyed(&mut twin, keyed);
    assert_eq!(waiter.waiting(), 0);
    assert_eq!(self::executed(&twin, &digests), executed);

    // Recorded by the first write, which read it there: the live version has
    // moved on since.
    let start = twin
        .epoch_store
        .start_version(
            &twin.store,
            &shared.stream().id,
            shared.initial_shared_version,
        )
        .unwrap();
    assert_eq!(start.version, shared.initial_shared_version);
}

#[test]
#[should_panic(expected = "fork: transaction")]
fn a_certified_transaction_with_other_effects_is_a_fork() {
    let mut node = Node::new(1);
    let mut twin = node.twin(1);
    let transfer = node.transfer(node.coin(0));
    let digests = [digest(&transfer)];
    node.execute_owned(transfer);
    let mut keyed = certified(&executed(&node, &digests));
    keyed[0].0.synced.as_mut().unwrap().effects = Hashed::of("TransactionEffects", &[]);
    common::InputWaiter::new(&twin).run_keyed(&mut twin, keyed);
}
