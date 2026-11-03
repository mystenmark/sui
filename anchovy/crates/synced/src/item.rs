// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A certified transaction as the executor runs it, with what its certified
//! effects decide in place of the consensus path's decisions
//! (`docs/state_sync_design.md`, Synced items).

use messages::Message;
use messages::base::{ObjectId, TransactionDigest};
use messages::checkpoint::ExecutionData;
use messages::effects::{ObjectIn, UnchangedConsensusKind, VersionedEffects};
use messages::execution_status::{ExecutionErrorKind, ExecutionStatus};
use messages::object::Owner;
use messages::transaction::{CallArg, ObjectArg, Transaction, TransactionKind};
use messages::tx_index::SUI_CLOCK_OBJECT_ID;
use validator::checks;
use validator::pipeline::items::{
    Executable, ExecutableTransaction, RegistryRead, ScheduledEntry, Synced,
};
use validator::pipeline::keys::{AccumulatorVersion, CausalIndex, InputKey, StreamId};

/// `SUI_FORWARDING_ADDRESS_REGISTRY_OBJECT_ID`.
const FORWARDING_ADDRESS_REGISTRY: ObjectId = ObjectId::from_u16(0xfa);

/// The executable for certified transaction `t`, of a chunk whose
/// transactions read the accumulator root at `root_version`, with its input
/// keys left in `keys` (`packages` is scratch).
///
/// Its consensus objects come at the input versions its effects record
/// (R4.3): those it declares, if a user transaction, else all of them.
/// `deleter` names the transaction among `t`'s dependencies that ended a
/// consensus object's stream at a version.
pub fn executable(
    t: &ExecutionData<'_>,
    root_version: u64,
    causal: Option<CausalIndex>,
    deleter: impl Fn(&ObjectId, u64, &[TransactionDigest]) -> [u8; 32],
    keys: &mut Vec<InputKey>,
    packages: &mut Vec<ObjectId>,
) -> Executable {
    let VersionedEffects::V2(effects) = &t.effects.version else {
        unreachable!("verified effects are V2");
    };
    let data = t.transaction.data();
    // A user transaction accesses the objects it declares as shared inputs;
    // any other in its effects (the accumulator root) it reads implicitly.
    let accessed = |id: &ObjectId| {
        match data.kind() {
        TransactionKind::ProgrammableTransaction(pt) => pt.inputs.iter().any(|input| {
            matches!(input, CallArg::Object(ObjectArg::SharedObject(shared)) if shared.id == *id)
        }),
        _ => true,
    }
    };

    keys.clear();
    // A system transaction's gas payment is a placeholder, not an input.
    if let TransactionKind::ProgrammableTransaction(_) = data.kind() {
        resolver::object_keys(data, keys, packages);
    }
    let mut writes = Vec::new();
    for change in effects.changed_objects {
        if let ObjectIn::Exist {
            version,
            digest,
            owner:
                Owner::Shared {
                    initial_shared_version: start,
                }
                | Owner::ConsensusAddressOwner {
                    start_version: start,
                    ..
                },
        } = change.input_state
            && accessed(change.id)
        {
            keys.push(InputKey::ObjectVersion {
                id: *change.id,
                version,
            });
            let stream = StreamId {
                id: *change.id,
                initial_shared_version: start,
            };
            writes.push((stream, version, *digest));
        }
    }
    let (mut ended, mut cancelled) = (Vec::new(), Vec::new());
    let mut clock_version = None;
    let mut forwarding_registry = None;
    for &(id, kind) in effects.unchanged_consensus_objects {
        // The forwarding address registry, read without being an input,
        // likewise. The effects say it is read, so its version is waited for
        // here: an earlier transaction of the checkpoint may write it.
        if *id == FORWARDING_ADDRESS_REGISTRY
            && !accessed(id)
            && let UnchangedConsensusKind::ReadOnlyRoot(version, _) = kind
        {
            forwarding_registry = Some(RegistryRead::At(version));
            keys.push(InputKey::ObjectVersion { id: *id, version });
        }
        // The Clock, borrowed without being an input, at the version the
        // effects record (the reference's `SystemObjectVersions::from_effects`).
        if *id == SUI_CLOCK_OBJECT_ID
            && let UnchangedConsensusKind::ReadOnlyRoot(version, _) = kind
        {
            clock_version = Some(version);
            if !accessed(id) {
                keys.push(InputKey::ObjectVersion { id: *id, version });
            }
        }
        if !accessed(id) {
            continue;
        }
        match kind {
            UnchangedConsensusKind::ReadOnlyRoot(version, _) => {
                keys.push(InputKey::ObjectVersion { id: *id, version });
            }
            UnchangedConsensusKind::MutateConsensusStreamEnded(version)
            | UnchangedConsensusKind::ReadConsensusStreamEnded(version) => {
                ended.push((*id, version, deleter(id, version, effects.dependencies)));
            }
            UnchangedConsensusKind::Cancelled(marker) => cancelled.push((*id, marker)),
            UnchangedConsensusKind::PerEpochConfig => {}
        }
    }
    let insufficient = matches!(
        effects.status,
        ExecutionStatus::Failure {
            error: ExecutionErrorKind::InsufficientFundsForWithdraw,
            ..
        }
    );

    let transaction = Message::<Transaction<'static, _>>::parse(t.transaction.bytes().to_vec())
        .map_err(|(e, _)| e)
        .expect("a certified transaction parses again");
    Executable {
        // Read only by the consensus path's funds settlement, which a
        // certified transaction skips.
        version: AccumulatorVersion(0),
        root_version,
        reserved: Vec::new(),
        entry: ScheduledEntry {
            transaction: ExecutableTransaction::User(checks::certified(transaction)),
            shared: Vec::new(),
            origin: None,
            aliases: None,
            cancelled,
            clock_version,
            forwarding_registry,
        },
        inputs: Vec::new(),
        synced: Some(Box::new(Synced {
            effects: t.effects.digest,
            ended,
            insufficient,
            writes,
        })),
        causal,
    }
}
