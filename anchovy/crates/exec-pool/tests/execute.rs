// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Executing through the input waiter: transactions on a shared coin handed
//! to the waiter in reverse, executed in the order their ordinals give, each
//! at the version its generation names.

mod common;

use std::sync::Arc;

use common::{Node, digest};
use exec_pool::Executor;
use sui_types::effects::{TransactionEffectsAPI, UnchangedConsensusKind};
use sui_types::execution_status::ExecutionStatus;
use validator::pipeline::items::InputValue;
use validator::pipeline::keys::{EffectsKey, InputKey};
use validator::pipeline::tables::{Generation, GenerationTable};
use workqueue::Processor;

fn version(generation: Option<Generation>) -> u64 {
    match generation.unwrap() {
        Generation::Version { version, .. } => version,
        ended @ Generation::Ended { .. } => panic!("{ended:?}"),
    }
}

/// The version `effects` give the shared object as an input.
fn read_at(
    effects: &sui_types::effects::TransactionEffects,
    id: sui_types::base_types::ObjectID,
) -> u64 {
    if let Some((_, v)) = effects
        .modified_at_versions()
        .into_iter()
        .find(|(o, _)| *o == id)
    {
        return v.value();
    }
    match effects
        .unchanged_consensus_objects()
        .into_iter()
        .find(|(o, _)| *o == id)
    {
        Some((_, UnchangedConsensusKind::ReadOnlyRoot((v, _)))) => v.value(),
        other => panic!("{other:?}"),
    }
}

// The transactions are named as in the scenario.
#[test]
fn a_chain_of_shared_writes_executes_across_generations_and_rounds() {
    let mut node = Node::new(8);
    let shared = node.new_shared(0);
    let stream = shared.stream();
    let initial = shared.initial_shared_version;

    // Round 10: A writes generation 0, R reads generation 1, B writes it.
    // Round 11: C writes the generation B produced. Round 12: D deletes it.
    // Round 13: E writes the ended stream.
    let a = node.increment(node.coin(1), shared);
    let r = node.read(node.coin(2), shared);
    let b = node.increment(node.coin(3), shared);
    let c = node.increment(node.coin(4), shared);
    let d = node.delete(node.coin(5), shared);
    let e = node.increment(node.coin(6), shared);
    let digests = [&a, &r, &b, &c, &d, &e].map(digest);
    let [a, r, b, c, d, e] = [
        (a, true, 10, 0, vec![]),
        (r, false, 10, 1, vec![]),
        (b, true, 10, 1, vec![]),
        (c, true, 11, 0, vec![(stream, 10, 2)]),
        (d, true, 12, 0, vec![(stream, 11, 1)]),
        (e, true, 13, 0, vec![(stream, 12, 1)]),
    ]
    .map(|(t, mutable, round, k, base)| {
        node.executable(t, vec![shared.access(mutable, round, k)], base)
    });

    let mut waiter = common::InputWaiter::new(&node);
    let batches = waiter.run(&mut node, vec![e, d, c, b, r, a]);
    assert_eq!(waiter.waiting(), 0, "everything executed");
    // A; then R and B, ready together on generation 1; then C, D, E.
    assert_eq!(batches, 5);

    let effects = digests.map(|d| node.effects(d).expect("executed"));
    for effects in &effects[..5] {
        assert_eq!(effects.status(), &ExecutionStatus::Success, "{effects:?}");
    }
    let [a, r, b, c, d, e] = &effects;
    let id = shared.id;

    // Each read the generation its ordinal names, and produced the next.
    assert_eq!(version(node.generations.get(stream, 10, 0)), initial);
    assert_eq!(read_at(a, id), initial);
    let first = version(node.generations.get(stream, 10, 1));
    assert_eq!(first, a.lamport_version().value());
    assert_eq!(
        read_at(r, id),
        first,
        "R reads A's version though B moved on"
    );
    assert_eq!(read_at(b, id), first);
    let second = version(node.generations.get(stream, 10, 2));
    assert_eq!(second, b.lamport_version().value());
    assert_eq!(read_at(c, id), second);
    assert_eq!(
        version(node.generations.get(stream, 11, 1)),
        c.lamport_version().value()
    );
    assert_eq!(read_at(d, id), c.lamport_version().value());

    // D ended the stream; E, writing it, keeps it ended.
    let ended = Generation::Ended {
        version: d.lamport_version().value(),
        transaction: digests[4].bytes,
    };
    assert_eq!(node.generations.get(stream, 12, 1), Some(ended));
    assert_eq!(node.generations.get(stream, 13, 1), Some(ended));
    assert!(node.live(id).is_none(), "deleted");
    assert!(
        e.unchanged_consensus_objects().contains(&(
            id,
            UnchangedConsensusKind::MutateConsensusStreamEnded(d.lamport_version())
        )),
        "{e:?}"
    );
}

#[test]
fn deleting_a_shared_object_ends_its_stream_for_its_readers() {
    let mut node = Node::new(3);
    let shared = node.new_shared(0);
    let stream = shared.stream();
    let delete = node.delete(node.coin(1), shared);
    let read = node.read(node.coin(2), shared);
    let (deleter, reader) = (digest(&delete), digest(&read));
    let delete = node.executable(delete, vec![shared.access(true, 5, 0)], vec![]);
    let read = node.executable(read, vec![shared.access(false, 6, 0)], vec![(stream, 5, 1)]);

    common::InputWaiter::new(&node).run(&mut node, vec![read, delete]);

    let deletion = node.effects(deleter).unwrap();
    assert_eq!(deletion.status(), &ExecutionStatus::Success);
    assert!(deletion.deleted().iter().any(|r| r.0 == shared.id));
    let lamport = deletion.lamport_version();
    assert_eq!(
        node.generations.get(stream, 5, 1),
        Some(Generation::Ended {
            version: lamport.value(),
            transaction: deleter.bytes,
        })
    );
    // The reader still executes, against the reference's stream-ended marker.
    let read = node.effects(reader).unwrap();
    assert!(
        read.unchanged_consensus_objects().contains(&(
            shared.id,
            UnchangedConsensusKind::ReadConsensusStreamEnded(lamport)
        )),
        "{read:?}"
    );
}

#[test]
fn keys_are_notified_only_once_visible() {
    let mut node = Node::new(2);
    let shared = node.new_shared(0);
    let transfer = node.transfer(node.coin(1));
    let increment = node.increment(node.coin(0), shared);
    let digests = [digest(&transfer), digest(&increment)];
    let batch = vec![
        node.executable(transfer, vec![], vec![]),
        node.executable(increment, vec![shared.access(true, 3, 0)], vec![]),
    ];
    assert!(node.notified_inputs.try_pop().is_none());

    node.executor
        .process(batch, &mut workqueue::Bump::default());
    // `drain_checked` asserts each key is visible as it takes it.
    let (resolved, effects) = node.drain_checked();
    // Each key resolves to what it names: an object at its version, a
    // generation with its object.
    for (key, value) in &resolved {
        match (key, value) {
            (InputKey::ObjectVersion { id, version }, InputValue::Object(object)) => {
                assert_eq!(object.get().id(), Some(id));
                assert_eq!(object.get().version(), *version);
            }
            (
                InputKey::Generation { stream, .. },
                InputValue::Generation(Generation::Version { version, .. }, Some(object)),
            ) => {
                assert_eq!(object.get().id(), Some(&stream.id));
                assert_eq!(object.get().version(), *version);
            }
            other => panic!("{other:?}"),
        }
    }
    let inputs: Vec<InputKey> = resolved.iter().map(|(key, _)| *key).collect();

    assert_eq!(effects, digests.map(|d| EffectsKey(*d)).to_vec());
    let transferred = node.effects(digests[0]).unwrap();
    for (object, _, _) in transferred.all_changed_objects() {
        assert!(inputs.contains(&InputKey::ObjectVersion {
            id: messages::base::ObjectId(object.0.into_bytes()),
            version: object.1.value(),
        }));
    }
    let stream = shared.stream();
    for generation in [0, 1] {
        assert!(inputs.contains(&InputKey::Generation {
            stream,
            round: 3,
            generation,
        }));
    }
}

#[test]
fn an_executed_transaction_republishes_its_generation_from_its_effects() {
    let mut node = Node::new(2);
    let shared = node.new_shared(0);
    let stream = shared.stream();
    let increment = node.increment(node.coin(1), shared);
    let again = messages::Message::parse(increment.wire_bytes().to_vec())
        .map_err(|(e, _)| e)
        .unwrap();
    let again = validator::checks::sequenced_by_consensus(again);
    node.executor.process(
        vec![node.executable(increment, vec![shared.access(true, 4, 0)], vec![])],
        &mut workqueue::Bump::default(),
    );
    node.drain_checked();
    let produced = node.generations.get(stream, 4, 1).unwrap();
    let live = node.live(shared.id).unwrap();

    // A restart: the generation table is empty; the replayed writer is not
    // executed again, but its generation is recorded again.
    node.generations = Arc::new(GenerationTable::new(node.epoch_store.clone()));
    let (inputs, _) = workqueue::queue(16);
    let (effects, notified) = workqueue::queue(16);
    let (funds, _) = workqueue::queue(16);
    let mut executor = Executor::new(
        node.store.clone(),
        node.epoch.clone(),
        node.epoch_store.clone(),
        node.generations.clone(),
        inputs,
        effects,
        funds,
    );
    executor.process(
        vec![node.executable(again, vec![shared.access(true, 9, 0)], vec![])],
        &mut workqueue::Bump::default(),
    );
    assert_eq!(node.generations.get(stream, 9, 1), Some(produced));
    assert_eq!(node.live(shared.id).unwrap(), live, "not executed again");
    assert!(
        notified.try_pop().is_some(),
        "its effects are notified again"
    );
}

#[test]
fn a_reader_of_generation_zero_reads_it_after_the_writer_ran() {
    let mut node = Node::new(3);
    let shared = node.new_shared(0);
    let stream = shared.stream();
    let initial = shared.initial_shared_version;
    // Both at generation 0 of round 7, live: the resolver had no generation.
    let write = node.increment(node.coin(1), shared);
    let read = node.read(node.coin(2), shared);
    let (writer, reader) = (digest(&write), digest(&read));

    node.executor.process(
        vec![node.executable(write, vec![shared.access(true, 7, 0)], vec![])],
        &mut workqueue::Bump::default(),
    );
    node.drain_checked();
    assert!(version(node.generations.get(stream, 7, 1)) > initial);
    node.executor.process(
        vec![node.executable(read, vec![shared.access(false, 7, 0)], vec![])],
        &mut workqueue::Bump::default(),
    );
    node.drain_checked();

    assert_eq!(version(node.generations.get(stream, 7, 0)), initial);
    let written = node.effects(writer).unwrap();
    assert_eq!(read_at(&written, shared.id), initial);
    assert_eq!(read_at(&node.effects(reader).unwrap(), shared.id), initial);
}
