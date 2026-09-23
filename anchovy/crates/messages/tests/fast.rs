// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Rebuilds every message in the mainnet corpus from its parsed view with
//! the fast builders and expects the same bytes and digest.

use std::path::Path;

use messages::Message;
use messages::checkpoint::{CheckpointData, VersionedCheckpointContents};
use messages::effects::{TransactionEffects, VersionedEffects};
use messages::fast::checkpoint::{CommitteeMember, EndOfEpoch};
use messages::fast::{Built, Bump, ContentsBuilder, EffectsBuilder, EventsBuilder, SummaryBuilder};

fn rebuild_effects<'a>(bump: &'a Bump, effects: &TransactionEffects<'a>) -> Built<'a> {
    let VersionedEffects::V2(v2) = &effects.version else {
        panic!("mainnet effects are V2")
    };
    let mut b = EffectsBuilder::new_in(
        bump,
        v2.status,
        v2.executed_epoch,
        v2.gas_used,
        *v2.transaction_digest,
        v2.lamport_version,
    );
    b.events_digest(v2.events_digest.copied())
        .aux_data_digest(v2.aux_data_digest.copied());
    // Reverse order, to show that dependencies are sorted at the end.
    for d in v2.dependencies.iter().rev() {
        b.dependency(*d);
    }
    for (i, c) in v2.changed_objects.iter().enumerate() {
        b.change(*c.id, c.input_state, c.output_state, c.id_operation);
        if v2.gas_object_index == Some(i as u32) {
            b.gas_object_is_last();
        }
    }
    for (id, kind) in v2.unchanged_consensus_objects {
        b.unchanged(**id, *kind);
    }
    b.finish()
}

fn check(path: &Path, counts: &mut (usize, usize, usize)) {
    let mut bytes = std::fs::read(path).unwrap();
    bytes.remove(0);
    let checkpoint = Message::<CheckpointData>::parse(bytes).unwrap();
    let view = checkpoint.get();

    // Inputs the builders borrow, gathered before the arena so they outlive it.
    let VersionedCheckpointContents::V2(entries) = &view.checkpoint_contents.version else {
        panic!("mainnet contents are V2")
    };
    let signatures: Vec<Vec<(&[u8], Option<u64>)>> = entries
        .iter()
        .map(|e| e.user_signatures.iter().map(|(s, v)| (s.0, *v)).collect())
        .collect();
    let summary = &view.checkpoint_summary.data;
    let commitments: Vec<_> = summary
        .checkpoint_commitments
        .iter()
        .map(messages::checkpoint::CheckpointCommitment::parts)
        .collect();
    let end_of_epoch = summary.end_of_epoch_data.as_ref().map(|e| {
        let committee: Vec<CommitteeMember> = e
            .next_epoch_committee
            .iter()
            .map(|m| (m.authority, m.stake.get()))
            .collect();
        let epoch_commitments: Vec<_> = e
            .epoch_commitments
            .iter()
            .map(messages::checkpoint::CheckpointCommitment::parts)
            .collect();
        (committee, e.next_epoch_protocol_version, epoch_commitments)
    });

    let bump = Bump::with_capacity(4 << 20);

    for tx in view.transactions {
        let built = rebuild_effects(&bump, &tx.effects);
        assert_eq!(built.bytes, tx.effects.bytes, "{}", path.display());
        assert_eq!(built.digest, tx.effects.digest);
        counts.0 += 1;

        if let Some(events) = &tx.events {
            let mut b = EventsBuilder::new_in(&bump, events.data.len());
            for e in events.data {
                b.push(*e);
            }
            let built = b.finish();
            assert_eq!(built.bytes, events.bytes, "{}", path.display());
            assert_eq!(built.digest, events.digest());
            counts.1 += 1;
        }
    }

    let mut b = ContentsBuilder::new_in(&bump, entries.len());
    for (e, signatures) in entries.iter().enumerate().map(|(i, e)| (e, &signatures[i])) {
        b.push(e.digest.transaction, e.digest.effects, signatures);
    }
    let built = b.finish();
    assert_eq!(built.bytes, view.checkpoint_contents.bytes);
    assert_eq!(built.digest, view.checkpoint_contents.digest());

    let mut b = SummaryBuilder::new_in(
        &bump,
        summary.epoch,
        summary.sequence_number,
        summary.network_total_transactions,
        *summary.content_digest,
        summary.epoch_rolling_gas_cost_summary,
        summary.timestamp_ms,
    );
    b.previous_digest = summary.previous_digest.copied();
    b.checkpoint_commitments = &commitments;
    b.version_specific_data = summary.version_specific_data;
    if let Some((committee, protocol_version, epoch_commitments)) = &end_of_epoch {
        b.end_of_epoch_data = Some(EndOfEpoch {
            next_epoch_committee: committee,
            next_epoch_protocol_version: *protocol_version,
            epoch_commitments,
        });
    }
    let built = b.finish();
    assert_eq!(built.bytes, summary.bytes);
    assert_eq!(built.digest, summary.digest);
    counts.2 += 1;

    assert_eq!(bump.chunks(), 1, "{}", path.display());
}

#[test]
fn rebuilds_the_corpus_byte_for_byte() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut counts = (0, 0, 0);
    check(
        &manifest.join("tests/data/mainnet-325300367.chk"),
        &mut counts,
    );
    if let Ok(entries) = std::fs::read_dir(manifest.join("../../corpus/mainnet")) {
        for entry in entries {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "chk") {
                check(&path, &mut counts);
            }
        }
    }
    eprintln!(
        "{} effects, {} event sets, {} summaries rebuilt",
        counts.0, counts.1, counts.2
    );
    assert!(counts.0 > 0);
}
