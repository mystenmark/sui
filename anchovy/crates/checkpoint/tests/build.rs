// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Checkpoint building against a mainnet checkpoint and sui's own types:
//! the mainnet checkpoint's transactions and effects go into a store and the
//! chunk table, and the contents, summary and signature built from them must
//! match byte for byte.

use std::collections::BTreeMap;
use std::sync::Arc;

use checkpoint::{BuiltAvailability, ContentsBuilder, ProtocolKey, SummaryBuilder, summary};
use messages::Message;
use messages::base::Digest;
use messages::checkpoint::{CheckpointData, CheckpointSummary};
use messages::fast::Bump;
use protocol_config::{Chain, ProtocolVersion};
use store::{Commit, Executed, Store};
use sui_types::crypto::AuthorityPublicKeyBytes;
use sui_types::effects::TransactionEffects as SuiEffects;
use sui_types::gas::GasCostSummary as SuiGas;
use sui_types::message_envelope::Message as _;
use sui_types::messages_checkpoint::{
    CheckpointArtifacts, CheckpointContents as SuiContents, CheckpointSummary as SuiSummary,
};
use sui_types::messages_consensus::{ConsensusTransaction, ConsensusTransactionKind};
use sui_types::transaction::SenderSignedData as SuiSigned;
use validator::epoch::EpochParams;
use validator::epoch_store::{BuilderOutput, EpochStore};
use validator::pipeline::items::{
    BuiltWaitItem, CheckpointContentsBuilt, ExecutedEffects, PendingCheckpoint, SettledChunk,
};
use validator::pipeline::keys::CheckpointBuilt;
use validator::pipeline::tables::{BuiltCheckpoint, CheckpointStore, ChunkTable};
use waiter::{Command, WaiterProcessor};
use workqueue::{Inbox, Processor, queue};

struct Mainnet {
    checkpoint: Message<CheckpointData<'static>>,
    store: Arc<Store>,
    epoch_store: EpochStore,
    builder: BuilderOutput,
    _dir: tempfile::TempDir,
    _epochs: tempfile::TempDir,
}

impl Mainnet {
    fn load() -> Mainnet {
        let mut bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../messages/tests/data/mainnet-325300367.chk"
        ))
        .unwrap();
        bytes.remove(0);
        let checkpoint = Message::<CheckpointData>::parse(bytes).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        for t in checkpoint.get().transactions {
            store
                .write_through(Commit {
                    executed: Some(Executed {
                        digest: *t.transaction.digest(),
                        transaction: t.transaction.bytes().to_vec(),
                        effects_digest: t.effects.digest,
                        effects: t.effects.bytes.to_vec(),
                        events: None,
                    }),
                    ..Commit::default()
                })
                .unwrap();
        }
        let epochs = tempfile::tempdir().unwrap();
        let (epoch_store, _, builder) = EpochStore::open(epochs.path(), 0);
        Mainnet {
            checkpoint,
            store,
            epoch_store,
            builder,
            _dir: dir,
            _epochs: epochs,
        }
    }

    fn summary(&self) -> &CheckpointSummary<'_> {
        &self.checkpoint.get().checkpoint_summary.data
    }

    fn effects(&self) -> Vec<ExecutedEffects> {
        self.checkpoint
            .get()
            .transactions
            .iter()
            .map(|t| ExecutedEffects {
                digest: t.transaction.digest().bytes,
                effects_digest: t.effects.digest.bytes,
                effects: Arc::from(t.effects.bytes),
            })
            .collect()
    }

    /// The checkpoint's effects in chunks at `heights`, split at `split`.
    fn chunks(&self, heights: [u64; 2], split: usize) -> Arc<ChunkTable> {
        let chunks = Arc::new(ChunkTable::new());
        let mut effects = self.effects();
        let rest = effects.split_off(split);
        let [first, second] = heights;
        for (height, effects) in [(first, effects), (second, rest)] {
            chunks.insert(SettledChunk {
                height,
                checkpoint_seq: self.summary().sequence_number,
                effects,
            });
        }
        chunks
    }
}

fn params(version: u64, epoch: u64) -> Arc<EpochParams> {
    Arc::new(EpochParams::new(
        Chain::Mainnet,
        version,
        epoch,
        Digest::ZERO,
        1000,
        4,
    ))
}

fn pending(seq: u64, heights: Vec<u64>, timestamp_ms: u64) -> PendingCheckpoint {
    PendingCheckpoint {
        seq,
        heights,
        timestamp_ms,
        close: None,
    }
}

fn key() -> Arc<ProtocolKey> {
    let mut private = [0u8; 32];
    private[31] = 7;
    Arc::new(ProtocolKey::from_private_bytes(&private).unwrap())
}

fn drain<T>(inbox: &Inbox<T>) -> Vec<T> {
    std::iter::from_fn(|| inbox.try_pop()).collect()
}

/// The previous checkpoint as the store would hold it: only its epoch,
/// digest and totals matter to the next summary.
fn previous(seq: u64, epoch: u64, digest: [u8; 32], total: u64, gas: [u64; 4]) -> BuiltCheckpoint {
    BuiltCheckpoint {
        seq,
        summary: epoch.to_le_bytes().to_vec(),
        digest,
        contents: Vec::new(),
        network_total_transactions: total,
        epoch_rolling_gas: gas,
        last_height: 0,
        timestamp_ms: 0,
        root_state: None,
    }
}

fn sui_effects(effects: &[ExecutedEffects]) -> Vec<SuiEffects> {
    effects
        .iter()
        .map(|e| bcs::from_bytes(&e.effects).unwrap())
        .collect()
}

fn sui_signatures(
    m: &Mainnet,
) -> Vec<
    Vec<(
        sui_types::signature::GenericSignature,
        Option<sui_types::base_types::SequenceNumber>,
    )>,
> {
    m.checkpoint
        .get()
        .transactions
        .iter()
        .map(|t| {
            let signed: SuiSigned = bcs::from_bytes(t.transaction.bytes()).unwrap();
            signed
                .tx_signatures()
                .iter()
                .map(|s| (s.clone(), None))
                .collect()
        })
        .collect()
}

/// Contents at the latest protocol version: V2 with the artifacts digest,
/// equal to mainnet's and to sui's builder's.
#[test]
fn contents_match_mainnet_and_sui() {
    let m = Mainnet::load();
    let s = m.summary();
    let epoch = params(ProtocolVersion::MAX.as_u64(), s.epoch);
    let (hasher, hasher_inbox) = queue(16);
    let (sequence, _) = queue(16);
    let mut builder = ContentsBuilder::new(
        epoch.clone(),
        m.chunks([3, 4], 5),
        m.store.clone(),
        hasher,
        sequence,
    );
    let (built, effects) = builder.build(
        pending(s.sequence_number, vec![3, 4], s.timestamp_ms),
        &mut containers::Bump::default(),
    );
    drop(hasher_inbox);

    let mainnet = &m.checkpoint.get().checkpoint_contents;
    assert_eq!(&*built.contents, mainnet.bytes);
    assert_eq!(built.contents_digest, s.content_digest.bytes);
    assert_eq!(built.transaction_count, 14);
    assert_eq!(
        built.artifacts_digest,
        Some(s.checkpoint_commitments[0].digest.bytes)
    );
    assert!(built.randomness_rounds.is_empty());

    let sui = sui_effects(&effects);
    let contents = SuiContents::new_v2(&sui, sui_signatures(&m));
    assert_eq!(&*built.contents, bcs::to_bytes(&contents).unwrap());
    assert_eq!(built.contents_digest, contents.digest().into_inner());
    let gas = SuiGas::new_from_txn_effects(sui.iter());
    assert_eq!(
        built.gas,
        [
            gas.computation_cost,
            gas.storage_cost,
            gas.storage_rebate,
            gas.non_refundable_storage_fee
        ]
    );
    let artifacts = CheckpointArtifacts::from(&sui[..]).digest().unwrap();
    assert_eq!(built.artifacts_digest, Some(artifacts.into_inner()));
}

/// Before address aliases the contents are V1, and before the artifacts
/// commitment there is none.
#[test]
fn contents_v1_match_sui() {
    let m = Mainnet::load();
    let s = m.summary();
    let version = 60;
    let epoch = params(version, s.epoch);
    assert!(!epoch.config.address_aliases());
    let (hasher, _hasher_inbox) = queue(16);
    let (sequence, _) = queue(16);
    let mut builder = ContentsBuilder::new(
        epoch.clone(),
        m.chunks([0, 1], 14),
        m.store.clone(),
        hasher,
        sequence,
    );
    let (built, effects) = builder.build(
        pending(s.sequence_number, vec![0, 1], s.timestamp_ms),
        &mut containers::Bump::default(),
    );

    let sui = sui_effects(&effects);
    let contents = SuiContents::new_with_digests_and_signatures(
        sui.iter().map(SuiEffects::execution_digests),
        sui_signatures(&m)
            .into_iter()
            .map(|sigs| sigs.into_iter().map(|(s, _)| s).collect())
            .collect(),
    );
    assert_eq!(&*built.contents, bcs::to_bytes(&contents).unwrap());
    assert_eq!(built.contents_digest, contents.digest().into_inner());
    assert_eq!(built.artifacts_digest, None);
}

/// The summary after the mainnet checkpoint's predecessor is mainnet's, and
/// sui's `CheckpointSummary::new`.
#[test]
fn summary_matches_mainnet_and_sui() {
    let m = Mainnet::load();
    let s = m.summary();
    let epoch = params(ProtocolVersion::MAX.as_u64(), s.epoch);
    let (hasher, _hasher_inbox) = queue(16);
    let (sequence, _) = queue(16);
    let mut builder = ContentsBuilder::new(
        epoch.clone(),
        m.chunks([0, 1], 1),
        m.store.clone(),
        hasher,
        sequence,
    );
    let (built, effects) = builder.build(
        pending(s.sequence_number, vec![0, 1], s.timestamp_ms),
        &mut containers::Bump::default(),
    );

    let rolling = s.epoch_rolling_gas_cost_summary;
    let before = [
        rolling.computation_cost - built.gas[0],
        rolling.storage_cost - built.gas[1],
        rolling.storage_rebate - built.gas[2],
        rolling.non_refundable_storage_fee - built.gas[3],
    ];
    let prev = previous(
        s.sequence_number - 1,
        s.epoch,
        s.previous_digest.unwrap().bytes,
        s.network_total_transactions - 14,
        before,
    );
    let summary = summary::build(&built, Some(&prev), &epoch, &Bump::default());
    assert_eq!(summary.bytes, s.bytes);
    assert_eq!(summary.digest, s.digest.bytes);
    assert_eq!(
        summary.network_total_transactions,
        s.network_total_transactions
    );

    let sui_config = sui_protocol_config::ProtocolConfig::get_for_version(
        sui_protocol_config::ProtocolVersion::MAX,
        sui_protocol_config::Chain::Mainnet,
    );
    let sui = sui_effects(&effects);
    let contents = SuiContents::new_v2(&sui, sui_signatures(&m));
    let artifacts = CheckpointArtifacts::from(&sui[..]).digest().unwrap();
    let expected = SuiSummary::new(
        &sui_config,
        s.epoch,
        s.sequence_number,
        s.network_total_transactions,
        &contents,
        Some(s.previous_digest.unwrap().bytes.into()),
        SuiGas::new(
            rolling.computation_cost,
            rolling.storage_cost,
            rolling.storage_rebate,
            rolling.non_refundable_storage_fee,
        ),
        None,
        s.timestamp_ms,
        vec![],
        vec![artifacts.into()],
    );
    assert_eq!(summary.bytes, bcs::to_bytes(&expected).unwrap());
    assert_eq!(summary.digest, expected.digest().into_inner());

    // The first checkpoint of an epoch restarts the rolling gas.
    let prev = previous(
        prev.seq,
        s.epoch - 1,
        prev.digest,
        prev.network_total_transactions,
        before,
    );
    let first = summary::build(&built, Some(&prev), &epoch, &Bump::default());
    assert_eq!(first.epoch_rolling_gas, built.gas);

    // Checkpoint 0 has no previous digest, and its total is its own.
    let mut genesis = built.clone();
    genesis.pending.seq = 0;
    let zero = summary::build(&genesis, None, &epoch, &Bump::default());
    let parsed: SuiSummary = bcs::from_bytes(&zero.bytes).unwrap();
    assert_eq!(parsed.previous_digest, None);
    assert_eq!(parsed.network_total_transactions, 14);
}

/// The signature transaction is a `CheckpointSignatureV2` sui decodes and
/// whose signature verifies under the committee of this validator alone.
#[test]
fn signature_transaction_verifies_in_sui() {
    let m = Mainnet::load();
    let s = m.summary();
    let key = key();
    let bytes = summary::signature_transaction(&key, s.bytes, s.epoch);
    let transaction: ConsensusTransaction = bcs::from_bytes(&bytes).unwrap();
    let ConsensusTransactionKind::CheckpointSignatureV2(message) = transaction.kind else {
        panic!("a CheckpointSignatureV2");
    };
    assert_eq!(bcs::to_bytes(message.summary.data()).unwrap(), s.bytes);
    let name = AuthorityPublicKeyBytes::new(*key.public());
    assert_eq!(message.summary.auth_sig().authority, name);
    assert_eq!(message.summary.auth_sig().epoch, s.epoch);
    let committee = sui_types::committee::Committee::new(s.epoch, BTreeMap::from([(name, 10_000)]));
    message
        .summary
        .verify_authority_signatures(&committee)
        .unwrap();
}

/// Pending checkpoints arriving out of order are built at once, and their
/// summaries come out in order, each chained to the one before, through
/// the built waiter.
#[test]
fn out_of_order_checkpoints_summarize_in_order() {
    let m = Mainnet::load();
    let s = m.summary();
    let epoch = params(ProtocolVersion::MAX.as_u64(), s.epoch);
    // Checkpoints 11, 12 and 13 of one or two chunks each.
    let chunks = Arc::new(ChunkTable::new());
    let effects = m.effects();
    let groups = [
        &effects[0..5],
        &effects[5..10],
        &effects[10..12],
        &effects[12..14],
    ];
    for (height, group) in groups.iter().enumerate() {
        chunks.insert(SettledChunk {
            height: height as u64,
            checkpoint_seq: 0,
            effects: group.to_vec(),
        });
    }
    let checkpoints = Arc::new(CheckpointStore::new(
        m.store.clone(),
        0,
        m.epoch_store.clone(),
        m.builder.clone(),
    ));
    checkpoints.insert_built(previous(10, s.epoch, [9; 32], 100, [1, 2, 3, 4]));

    let (to_hasher, hasher_inbox) = queue(16);
    let (built, built_inbox) = queue(16);
    let (consensus, _normal, mut signatures) = consensus::TransactionClient::new(16);
    let mut contents = ContentsBuilder::new(
        epoch.clone(),
        chunks,
        m.store.clone(),
        to_hasher,
        built.clone(),
    );
    let mut waiter = WaiterProcessor::new(BuiltAvailability {
        checkpoints: checkpoints.clone(),
    });
    let mut summary =
        SummaryBuilder::new(epoch.clone(), checkpoints.clone(), key(), built, consensus);

    // Runs the built waiter and the summary builder until quiet; returns the
    // sequence numbers summarized, in order. Records the checkpoints the
    // summary builder resolved.
    let mut resolved = Vec::new();
    let mut settle = |waiter: &mut WaiterProcessor<_, _, _, _>| -> Vec<u64> {
        let mut summarized = Vec::new();
        while let Some(command) = built_inbox.try_pop() {
            if let Command::Resolve(keys) = &command {
                resolved.extend(keys.iter().map(|(CheckpointBuilt(seq), _)| *seq));
            }
            if let Some(ready) = waiter.process(command, &mut workqueue::Bump::default()) {
                let ready: Vec<CheckpointContentsBuilt> = ready
                    .into_iter()
                    .map(|item| match item {
                        BuiltWaitItem::Next(built) => built,
                        other => panic!("only contents wait here: {other:?}"),
                    })
                    .collect();
                summarized.extend(ready.iter().map(|b| b.pending.seq));
                summary.process(ready, &mut workqueue::Bump::default());
            }
        }
        summarized
    };

    contents.process(
        vec![pending(13, vec![3], 30)],
        &mut workqueue::Bump::default(),
    );
    contents.process(
        vec![pending(12, vec![2], 20)],
        &mut workqueue::Bump::default(),
    );
    assert!(settle(&mut waiter).is_empty(), "12 and 13 wait for 11");
    assert!(checkpoints.built(12).is_none());
    contents.process(
        vec![pending(11, vec![0, 1], 10)],
        &mut workqueue::Bump::default(),
    );
    assert_eq!(settle(&mut waiter), vec![11, 12, 13]);

    let hashed: Vec<u64> = drain(&hasher_inbox).iter().map(|e| e.seq).collect();
    assert_eq!(
        hashed,
        vec![13, 12, 11],
        "contents go to the hasher as they are built"
    );

    let mut previous = checkpoints.built(10).unwrap();
    for seq in 11..=13 {
        let built = checkpoints.built(seq).unwrap();
        let parsed: SuiSummary = bcs::from_bytes(&built.summary).unwrap();
        assert_eq!(parsed.sequence_number, seq);
        assert_eq!(
            parsed.previous_digest.unwrap().into_inner(),
            previous.digest
        );
        assert_eq!(
            parsed.network_total_transactions,
            previous.network_total_transactions + [10, 2, 2][(seq - 11) as usize]
        );
        assert_eq!(parsed.digest().into_inner(), built.digest);
        previous = built;
    }

    assert_eq!(resolved, vec![11, 12, 13]);

    // Each on the priority lane, alone.
    let signed: Vec<u64> = std::iter::from_fn(|| signatures.try_recv().ok())
        .map(|submission| {
            let [bytes] = &submission.transactions[..] else {
                panic!("one signature per submission");
            };
            let transaction: ConsensusTransaction = bcs::from_bytes(bytes).unwrap();
            let ConsensusTransactionKind::CheckpointSignatureV2(message) = transaction.kind else {
                panic!("a CheckpointSignatureV2");
            };
            message.summary.data().sequence_number
        })
        .collect();
    assert_eq!(signed, vec![11, 12, 13]);
}
