// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Closing the epoch: its last checkpoint, built whole
//! (`docs/reconfiguration_design.md`, The final checkpoint).

use std::sync::Arc;

use execution::epoch_start::SystemObjects;
use fastcrypto::hash::MultisetHash;
use messages::base::Digest;
use messages::build::base::{AuthorityPublicKeyBytes, ChainIdentifier, SequenceNumber};
use messages::build::system_transaction::{
    AuthenticatorStateExpire, Duration, EndOfEpochTransactionKind, StoredExecutionTimeObservations,
    WriteAccumulatorStorageCost,
};
use messages::fast::Bump;
use messages::transaction::TransactionKind;
use protocol_config::{ExecutionTimeEstimateParams, PerObjectCongestionControlMode};
use validator::consensus::commit::end_of_epoch_transaction;
use validator::consensus::execution_time::{Key, KeyRef};
use validator::epoch::EpochParams;
use validator::pipeline::items::{
    CheckpointContentsBuilt, CheckpointEffects, Chunk, EffectsWaitItem, EndOfEpoch, EpochClose,
    EpochCloserInput, ExecutableTransaction, ExecutedEffects, LastOfEpoch, ScheduledEntry,
};
use validator::pipeline::keys::EffectsKey;
use validator::pipeline::tables::CheckpointStore;
use waiter::{Command, WaitBatch};
use workqueue::{Processor, Queue};

use crate::{contents_of, push, summary};

/// Passes each checkpoint on to `SummaryBuilder` once the previous one is
/// built, but the epoch's last, which it closes:
/// 1. Builds the end-of-epoch transaction: the kinds the reference puts
///    before `ChangeEpoch` as the protocol and the system objects at the
///    epoch's start call for them, then `ChangeEpoch`, with the epoch's gas
///    totals through this checkpoint (the previous one's are built), and
///    this checkpoint's timestamp as the next epoch's start.
/// 2. Hands it to the resolver with the shared accesses the commit handler
///    reserved, so it executes after everything else in the epoch, into the
///    writeback cache: built, not committed. It commits with the certified
///    checkpoint like any transaction.
/// 3. Once executed, builds the checkpoint again with it last, and hands
///    the whole checkpoint's effects to the hasher.
/// 4. Once the hasher has folded them, fills in the end-of-epoch data: the
///    next committee and protocol version from the system state the
///    transaction wrote, and the epoch's root.
///
/// Not yet: protocol upgrades (the next epoch keeps the protocol version).
///
/// Inward:
/// - built waiter, ready: `EpochCloserInput::Built`;
/// - effects waiter, ready: `EpochCloserInput::ChangeEpochExecuted`;
/// - `StateHasher`: `EpochCloserInput::Root`.
///
/// Outward:
/// - `VersionResolver`: the change-epoch transaction's `Chunk`;
/// - effects waiter, wait for: `ChangeEpochExecuted`, keyed by its
///   `EffectsKey`;
/// - `StateHasher`: the last checkpoint's `CheckpointEffects`;
/// - `SummaryBuilder`: `Vec<CheckpointContentsBuilt>`.
///
/// Reads: the checkpoint store, the object store.
pub struct EpochCloser {
    epoch: Arc<EpochParams>,
    /// The system objects as the epoch started.
    objects: SystemObjects,
    store: Arc<store::Store>,
    checkpoints: Arc<CheckpointStore>,
    resolver: Queue<Chunk>,
    effects: Queue<Command<EffectsKey, EffectsWaitItem>>,
    hasher: Queue<CheckpointEffects>,
    summary: Queue<Vec<CheckpointContentsBuilt>>,
    closing: Option<Closing>,
    bump: Bump,
}

/// The last checkpoint, between the steps above.
struct Closing {
    built: CheckpointContentsBuilt,
    change_epoch: [u8; 32],
}

impl EpochCloser {
    pub fn new(
        epoch: Arc<EpochParams>,
        objects: SystemObjects,
        store: Arc<store::Store>,
        checkpoints: Arc<CheckpointStore>,
        resolver: Queue<Chunk>,
        effects: Queue<Command<EffectsKey, EffectsWaitItem>>,
        hasher: Queue<CheckpointEffects>,
        summary: Queue<Vec<CheckpointContentsBuilt>>,
    ) -> EpochCloser {
        EpochCloser {
            epoch,
            objects,
            store,
            checkpoints,
            resolver,
            effects,
            hasher,
            summary,
            closing: None,
            bump: Bump::with_capacity(crate::ARENA),
        }
    }

    /// Step 1 and 2.
    fn close(&mut self, built: CheckpointContentsBuilt) {
        let seq = built.pending.seq;
        let close = built
            .pending
            .close
            .clone()
            .expect("only the epoch's last checkpoint is closed");
        let previous = self
            .checkpoints
            .built(seq - 1)
            .expect("a checkpoint is released once the previous one is built");
        let kinds = self.end_of_epoch_kinds(&built, &close);
        let epoch = &self.epoch;
        let transaction = end_of_epoch_transaction(
            epoch.epoch,
            kinds,
            epoch.config.version.as_u64(),
            summary::rolling_gas(&built, Some(&previous), epoch.epoch),
            built.pending.timestamp_ms,
        );
        let digest = transaction.digest;
        push(
            &self.resolver,
            Chunk {
                version: close.version,
                root_version: close.root_version,
                round: close.round,
                height: *built
                    .pending
                    .heights
                    .last()
                    .expect("a checkpoint has chunks"),
                checkpoint_seq: seq,
                entries: vec![ScheduledEntry {
                    transaction: ExecutableTransaction::System(transaction),
                    shared: close.shared,
                    origin: None,
                }],
            },
            "resolver",
        );
        let mut wait = WaitBatch::with_capacity(1, 1);
        wait.push(EffectsWaitItem::ChangeEpochExecuted, [EffectsKey(digest)]);
        push(&self.effects, Command::Wait(wait), "effects waiter");
        self.closing = Some(Closing {
            built,
            change_epoch: digest,
        });
    }

    /// What the reference's `create_and_execute_advance_epoch_tx` puts
    /// before `ChangeEpoch`, in its order: each kind as its protocol flag
    /// and the system objects as the epoch started call for it.
    fn end_of_epoch_kinds(
        &mut self,
        built: &CheckpointContentsBuilt,
        close: &EpochClose,
    ) -> Vec<EndOfEpochTransactionKind> {
        let config = &self.epoch.config;
        let objects = self.objects;
        let mut kinds = Vec::new();
        if config.enable_jwk_consensus_updates() {
            kinds.push(match objects.authenticator_state {
                Some(version) => {
                    EndOfEpochTransactionKind::AuthenticatorStateExpire(AuthenticatorStateExpire {
                        min_epoch: (self.epoch.epoch + 1)
                            .saturating_sub(config.max_age_of_jwk_in_epochs()),
                        authenticator_obj_initial_shared_version: SequenceNumber(version),
                    })
                }
                None => EndOfEpochTransactionKind::AuthenticatorStateCreate,
            });
        }
        if config.random_beacon() && objects.randomness_state.is_none() {
            kinds.push(EndOfEpochTransactionKind::RandomnessStateCreate);
        }
        if config.bridge() {
            match objects.bridge {
                None => kinds.push(EndOfEpochTransactionKind::BridgeStateCreate(
                    ChainIdentifier::from(&self.epoch.chain_identifier),
                )),
                Some(version)
                    if config.should_try_to_finalize_bridge_committee()
                        && !objects.bridge_committee_initiated =>
                {
                    kinds.push(EndOfEpochTransactionKind::BridgeCommitteeInit(
                        SequenceNumber(version),
                    ));
                }
                Some(_) => {}
            }
        }
        if config.enable_coin_deny_list() && objects.deny_list.is_none() {
            kinds.push(EndOfEpochTransactionKind::DenyListStateCreate);
        }
        if let PerObjectCongestionControlMode::ExecutionTimeEstimate(params) =
            config.per_object_congestion_control_mode()
        {
            let observations = self.stored_observations(built, close, &params);
            kinds.push(EndOfEpochTransactionKind::StoreExecutionTimeObservations(
                observations,
            ));
        }
        let config = &self.epoch.config;
        for (enabled, exists, kind) in [
            (
                config.create_root_accumulator_object(),
                objects.accumulator_root,
                EndOfEpochTransactionKind::AccumulatorRootCreate,
            ),
            (
                config.enable_coin_registry(),
                objects.coin_registry,
                EndOfEpochTransactionKind::CoinRegistryCreate,
            ),
            (
                config.enable_display_registry(),
                objects.display_registry,
                EndOfEpochTransactionKind::DisplayRegistryCreate,
            ),
            (
                config.address_aliases(),
                objects.address_alias_state,
                EndOfEpochTransactionKind::AddressAliasStateCreate,
            ),
            (
                config.create_forwarding_address_registry(),
                objects.forwarding_address_registry,
                EndOfEpochTransactionKind::ForwardingAddressRegistryCreate,
            ),
        ] {
            if enabled && exists.is_none() {
                kinds.push(kind);
            }
        }
        // As the store holds them now, after the epoch's other transactions.
        if objects.accumulator_root.is_some()
            && config.enable_accumulators()
            && let Some(count) = execution::epoch_start::accumulator_object_count(&self.store)
                .expect("the store reads")
        {
            kinds.push(EndOfEpochTransactionKind::WriteAccumulatorStorageCost(
                WriteAccumulatorStorageCost {
                    storage_cost: count.saturating_mul(config.accumulator_object_storage_cost()),
                },
            ));
        }
        kinds
    }

    /// The reference's `create_execution_time_observations_tx`: the
    /// observations as of the epoch's last commit, of the keys the commands
    /// of the last `stored_observations_num_included_checkpoints`
    /// checkpoints before this one, and of this one, use; at most
    /// `stored_observations_limit`, by key.
    fn stored_observations(
        &mut self,
        built: &CheckpointContentsBuilt,
        close: &EpochClose,
        params: &ExecutionTimeEstimateParams,
    ) -> StoredExecutionTimeObservations {
        let last_before = built.pending.seq - 1;
        let first = last_before
            .saturating_sub(params.stored_observations_num_included_checkpoints - 1)
            .max(close.first_checkpoint);
        let observations = &close.observations;
        let flags = Bump::with_capacity(observations.len());
        let mut used = containers::Vec::with_capacity_in(observations.len(), &flags);
        used.resize(observations.len(), false);
        for seq in first..=last_before {
            let contents = self
                .checkpoints
                .built(seq)
                .expect("the epoch's checkpoints are built")
                .contents
                .clone();
            self.mark_used(&contents, observations, &mut used);
        }
        self.mark_used(&built.contents, observations, &mut used);
        let limit = usize::try_from(params.stored_observations_limit).unwrap_or(usize::MAX);
        StoredExecutionTimeObservations::V1(
            observations
                .iter()
                .zip(used.iter())
                .filter(|(_, used)| **used)
                .take(limit)
                .map(|((key, by_authority), _)| {
                    let by_authority = by_authority
                        .iter()
                        .map(|(name, duration)| {
                            let duration = Duration {
                                secs: duration.as_secs(),
                                nanos: duration.subsec_nanos(),
                            };
                            (AuthorityPublicKeyBytes(*name), duration)
                        })
                        .collect();
                    (key.to_stored(), by_authority)
                })
                .collect(),
        )
    }

    /// Marks in `used` each of `observations` (sorted by key) whose key a
    /// command of a transaction `contents` (`CheckpointContents` BCS) lists
    /// uses.
    fn mark_used(
        &mut self,
        contents: &[u8],
        observations: &[(Key, Vec<([u8; 96], std::time::Duration)>)],
        used: &mut [bool],
    ) {
        for digest in crate::transactions(contents, &mut self.bump) {
            let transaction = self
                .store
                .transaction(&digest)
                .expect("the store reads")
                .expect("a checkpoint's transactions executed");
            let TransactionKind::ProgrammableTransaction(ptb) = transaction.get().data().kind()
            else {
                continue;
            };
            for command in ptb.commands {
                let key = KeyRef::of_command(command);
                if let Ok(i) = observations.binary_search_by(|(k, _)| k.as_ref().cmp(&key)) {
                    used[i] = true;
                }
            }
        }
    }

    /// Step 3.
    fn executed(&mut self) {
        let change_epoch = self
            .closing
            .as_ref()
            .expect("closing the epoch")
            .change_epoch;
        let executed = self.change_epoch_effects(change_epoch);
        let closing = self.closing.as_mut().expect("closing the epoch");
        let Some(LastOfEpoch::Effects(mut effects)) = closing.built.last.take() else {
            unreachable!("the last checkpoint's effects are kept until it is built again")
        };
        effects.push(executed);
        let seq = closing.built.pending.seq;
        closing.built = contents_of(
            &self.store,
            &self.epoch,
            closing.built.pending.clone(),
            &effects,
            &mut self.bump,
        );
        push(
            &self.hasher,
            CheckpointEffects {
                seq,
                effects,
                last_of_epoch: true,
            },
            "hasher",
        );
    }

    /// The change-epoch transaction's executed effects.
    fn change_epoch_effects(&self, digest: [u8; 32]) -> ExecutedEffects {
        let effects_digest = self
            .store
            .executed_effects(&Digest::new(digest))
            .expect("the store reads")
            .expect("the change-epoch transaction executed");
        let effects = self
            .store
            .effects(&effects_digest)
            .expect("the store reads")
            .expect("executed effects are stored with their transaction");
        ExecutedEffects {
            digest,
            effects_digest: effects_digest.bytes,
            effects: effects.wire_bytes().into(),
        }
    }

    /// Step 4.
    fn rooted(&mut self, root_state: Vec<u8>) {
        let Closing { mut built, .. } = self.closing.take().expect("closing the epoch");
        let next = execution::system_state::read(&self.store)
            .expect("the change-epoch transaction wrote the system state");
        assert_eq!(
            next.epoch,
            self.epoch.epoch + 1,
            "the change-epoch transaction advanced the epoch"
        );
        let root_state_digest = self
            .epoch
            .config
            .commit_root_state_digest()
            .then(|| state_hasher::decode(&root_state).digest().digest);
        built.last = Some(LastOfEpoch::Closed(EndOfEpoch {
            next_committee: next.stakes(),
            next_protocol_version: next.protocol_version,
            root_state,
            root_state_digest,
        }));
        push(&self.summary, vec![built], "summary builder");
    }
}

impl Processor for EpochCloser {
    type Input = EpochCloserInput;
    type Output = ();

    fn process(&mut self, input: EpochCloserInput) -> Option<()> {
        match input {
            EpochCloserInput::Built(ready) => {
                let mut passed = Vec::with_capacity(ready.len());
                for built in ready {
                    if built.pending.close.is_some() {
                        self.close(built);
                    } else {
                        passed.push(built);
                    }
                }
                if !passed.is_empty() {
                    push(&self.summary, passed, "summary builder");
                }
            }
            EpochCloserInput::ChangeEpochExecuted => self.executed(),
            EpochCloserInput::Root(root) => self.rooted(root),
        }
        None
    }
}
