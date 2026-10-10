// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! What `TemporaryStore::into_effects` uses from `sui_types::effects`
//! (`object_change`, `effects_v2`, `mod`) and `sui_types::execution`, built
//! with `messages::fast` instead of an owned `TransactionEffects`.

use containers::{BTreeMap, BTreeSet, Bump, VecMap};
use messages::arena::Ref;
use messages::base::{Digest, ObjectDigest, ObjectId, SequenceNumber, TransactionDigest};
use messages::effects::{
    AccumulatorOperation, AccumulatorValue, AccumulatorWriteV1, Event, GasCostSummary, IdOperation,
    ObjectIn, ObjectOut, UnchangedConsensusKind,
};
use messages::execution_status::ExecutionStatus;
use messages::fast::{Built, EffectsBuilder, EventsBuilder};
use messages::object::Owner;
use messages::transaction::SharedObjectMutability;

use exec_types::base::{EpochId, ObjectRef};
use exec_types::object::Object;

/// `sui_types::base_types::VersionDigest`.
pub type VersionDigest = (SequenceNumber, ObjectDigest);

// sui_types::effects (mod.rs)

pub const APPROX_SIZE_OF_OBJECT_REF: usize = 80;
pub const APPROX_SIZE_OF_EXECUTION_STATUS: usize = 120;
pub const APPROX_SIZE_OF_EPOCH_ID: usize = 10;
pub const APPROX_SIZE_OF_GAS_COST_SUMMARY: usize = 40;
pub const APPROX_SIZE_OF_OPT_TX_EVENTS_DIGEST: usize = 40;
pub const APPROX_SIZE_OF_TX_DIGEST: usize = 40;
pub const APPROX_SIZE_OF_OWNER: usize = 48;

/// `TransactionEffects::new_from_execution_v2`: the effects' bytes and digest.
///
/// The reference emits `dependencies` in the order given, which its caller
/// collects from a `BTreeSet`; the builder sorts and deduplicates them, which
/// is the same for that input.
pub fn new_from_execution_v2<'a>(
    bump: &'a Bump,
    status: ExecutionStatus<'a>,
    executed_epoch: EpochId,
    gas_used: GasCostSummary,
    unchanged_consensus_objects: containers::Vec<'a, (ObjectId, UnchangedConsensusKind<'a>)>,
    transaction_digest: TransactionDigest,
    lamport_version: SequenceNumber,
    changed_objects: VecMap<'a, ObjectId, EffectsObjectChange<'a>>,
    gas_object: Option<ObjectId>,
    events_digest: Option<Digest>,
    dependencies: impl IntoIterator<Item = TransactionDigest>,
) -> Built<'a> {
    #[cfg(debug_assertions)]
    check_invariant(
        bump,
        lamport_version,
        &changed_objects,
        gas_object,
        &unchanged_consensus_objects,
    );

    let mut builder = EffectsBuilder::new_in(
        bump,
        status,
        executed_epoch,
        gas_used,
        transaction_digest,
        lamport_version,
    );
    let mut gas_object_found = false;
    // `changed_objects` iterates in increasing id order, as the builder requires.
    for (id, change) in &changed_objects {
        builder.change(
            *id,
            change.input_state,
            change.output_state,
            change.id_operation,
        );
        if gas_object == Some(*id) {
            builder.gas_object_is_last();
            gas_object_found = true;
        }
    }
    // The reference's `position(..).unwrap()`.
    assert!(
        gas_object.is_none() || gas_object_found,
        "gas object {gas_object:?} not among the changed objects"
    );
    for (id, kind) in unchanged_consensus_objects {
        builder.unchanged(id, kind);
    }
    builder.events_digest(events_digest);
    for d in dependencies {
        builder.dependency(d);
    }
    builder.aux_data_digest(None);
    builder.finish()
}

/// `TransactionEffects::estimate_effects_size_upperbound_v2`.
pub fn estimate_effects_size_upperbound_v2(
    num_writes: usize,
    num_modifies: usize,
    num_deps: usize,
) -> usize {
    let fixed_sizes = APPROX_SIZE_OF_EXECUTION_STATUS
        + APPROX_SIZE_OF_EPOCH_ID
        + APPROX_SIZE_OF_GAS_COST_SUMMARY
        + APPROX_SIZE_OF_OPT_TX_EVENTS_DIGEST;

    // We store object ref and owner for both old objects and new objects.
    let approx_change_entry_size = 1_000
        + (APPROX_SIZE_OF_OWNER + APPROX_SIZE_OF_OBJECT_REF) * num_writes
        + (APPROX_SIZE_OF_OWNER + APPROX_SIZE_OF_OBJECT_REF) * num_modifies;

    let deps_size = 1_000 + APPROX_SIZE_OF_TX_DIGEST * num_deps;

    fixed_sizes + approx_change_entry_size + deps_size
}

/// `TransactionEvents` of `events`: its bytes and its digest
/// (`TransactionEvents::digest`).
pub fn build_events<'a>(bump: &'a Bump, events: &[Event<'a>]) -> Built<'a> {
    let mut builder = EventsBuilder::new_in(bump, events.len());
    for e in events {
        builder.push(*e);
    }
    builder.finish()
}

// sui_types::effects::object_change

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EffectsObjectChange<'a> {
    /// State of the object in the store prior to this transaction.
    pub input_state: ObjectIn<'a>,
    /// State of the object in the store after this transaction.
    pub output_state: ObjectOut<'a>,
    /// Whether this object ID is created or deleted in this transaction.
    pub id_operation: IdOperation,
}

impl<'a> EffectsObjectChange<'a> {
    /// `written` must be sealed (`Object::seal`): its digest is a hash of its
    /// stored bytes, which the caller keeps for writing the object out, so
    /// sealing here would encode it a second time.
    ///
    /// # Panics
    /// If `written` is not sealed.
    pub fn new(
        bump: &'a Bump,
        modified_at: Option<(VersionDigest, Owner<'a>)>,
        written: Option<&Object<'a>>,
        id_created: bool,
        id_deleted: bool,
    ) -> Self {
        debug_assert!(
            !id_created || !id_deleted,
            "Object ID can't be created and deleted at the same time."
        );
        Self {
            input_state: modified_at.map_or(ObjectIn::NotExist, |((version, digest), owner)| {
                ObjectIn::Exist {
                    version,
                    digest: containers::alloc(bump, digest),
                    owner,
                }
            }),
            output_state: written.map_or(ObjectOut::NotExist, |o| {
                if o.is_package() {
                    ObjectOut::PackageWrite(o.version(), containers::alloc(bump, o.digest()))
                } else {
                    ObjectOut::ObjectWrite(containers::alloc(bump, o.digest()), *o.owner())
                }
            }),
            id_operation: if id_created {
                IdOperation::Created
            } else if id_deleted {
                IdOperation::Deleted
            } else {
                IdOperation::None
            },
        }
    }

    pub fn new_from_accumulator_write(bump: &'a Bump, write: AccumulatorWriteV1<'a>) -> Self {
        Self {
            input_state: ObjectIn::NotExist,
            output_state: ObjectOut::AccumulatorWriteV1(Ref::new(containers::alloc(bump, write))),
            id_operation: IdOperation::None,
        }
    }
}

/// `AccumulatorWriteV1::merge`. Merged event digests are a new slice in
/// `bump`.
///
/// # Panics
/// If `writes` is empty, if the writes' value kinds differ, on integer
/// overflow, or on `IntegerTuple` (not implemented by the reference). In
/// debug builds, also if the writes' addresses or types differ.
pub fn merge_accumulator_writes<'a>(
    bump: &'a Bump,
    writes: &[AccumulatorWriteV1<'a>],
) -> AccumulatorWriteV1<'a> {
    if writes.len() == 1 {
        return writes[0];
    }

    let address = writes[0].address;
    let ty = writes[0].ty;

    // The reference checks under `in_test_configuration` with `debug_fatal!`.
    if cfg!(debug_assertions) {
        for write in &writes[1..] {
            assert!(
                write.address == address,
                "All writes must have the same accumulator address: {} != {}",
                write.address,
                address
            );
            assert!(
                write.ty == ty,
                "All writes must have the same accumulator type: {:?} != {:?}",
                write.ty,
                ty
            );
        }
    }
    let (merged_value, net_operation) = match &writes[0].value {
        AccumulatorValue::Integer(_) => {
            let (merge_amount, split_amount) =
                writes.iter().fold((0u64, 0u64), |(merge, split), w| {
                    if let AccumulatorValue::Integer(v) = w.value {
                        match w.operation {
                            AccumulatorOperation::Merge => (
                                merge.checked_add(v).expect("validated in object runtime"),
                                split,
                            ),
                            AccumulatorOperation::Split => (
                                merge,
                                split.checked_add(v).expect("validated in object runtime"),
                            ),
                        }
                    } else {
                        panic!("mismatched accumulator value types for same object");
                    }
                });
            let (amount, operation) = if merge_amount >= split_amount {
                (merge_amount - split_amount, AccumulatorOperation::Merge)
            } else {
                (split_amount - merge_amount, AccumulatorOperation::Split)
            };
            (AccumulatorValue::Integer(amount), operation)
        }
        AccumulatorValue::IntegerTuple(_, _) => {
            todo!("IntegerTuple netting-out logic not yet implemented")
        }
        AccumulatorValue::EventDigest(first_digests) => {
            let total = writes
                .iter()
                .map(|w| match w.value {
                    AccumulatorValue::EventDigest(d) => d.len(),
                    _ => 0,
                })
                .sum();
            let mut event_digests = containers::Vec::with_capacity_in(total, bump);
            event_digests.extend_from_slice(first_digests);
            for write in &writes[1..] {
                if let AccumulatorValue::EventDigest(digests) = write.value {
                    event_digests.extend_from_slice(digests);
                } else {
                    panic!("mismatched accumulator value types for same object");
                }
            }
            (
                AccumulatorValue::EventDigest(event_digests.leak()),
                AccumulatorOperation::Merge,
            )
        }
    };
    AccumulatorWriteV1 {
        address,
        ty,
        operation: net_operation,
        value: merged_value,
    }
}

// sui_types::execution

/// `sui_types::execution::ConsensusStreamEndedInfo`: the id, the version,
/// how the transaction took the object, and the transaction that last used
/// it mutably or by value.
pub type ConsensusStreamEndedInfo = (
    ObjectId,
    SequenceNumber,
    SharedObjectMutability,
    TransactionDigest,
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SharedInput {
    Existing(ObjectRef),
    ConsensusStreamEnded(ConsensusStreamEndedInfo),
    Cancelled((ObjectId, SequenceNumber)),
}

// sui_types::effects::effects_v2

/// `TransactionEffectsV2::compute_unchanged_consensus_objects`: the
/// unchanged consensus objects of a transaction from its shared inputs, the
/// per-epoch config objects it read, and the system objects it read during
/// execution, given the set of objects it changed.
///
/// Takes its inputs by reference where the reference takes (clones of) them
/// by value.
pub fn compute_unchanged_consensus_objects<'a>(
    bump: &'a Bump,
    shared_objects: &[SharedInput],
    loaded_per_epoch_config_objects: &BTreeSet<'_, ObjectId>,
    changed_objects: &VecMap<'a, ObjectId, EffectsObjectChange<'a>>,
    loaded_system_objects: &BTreeMap<'_, ObjectId, VersionDigest>,
) -> containers::Vec<'a, (ObjectId, UnchangedConsensusKind<'a>)> {
    let mut unchanged_consensus_objects = containers::Vec::with_capacity_in(
        shared_objects.len() + loaded_per_epoch_config_objects.len() + loaded_system_objects.len(),
        bump,
    );
    unchanged_consensus_objects.extend(
        shared_objects
            .iter()
            .filter_map(|shared_input| match *shared_input {
                SharedInput::Existing((id, version, digest)) => {
                    if changed_objects.contains_key(&id) {
                        None
                    } else {
                        Some((
                            id,
                            UnchangedConsensusKind::ReadOnlyRoot(
                                version,
                                containers::alloc(bump, digest),
                            ),
                        ))
                    }
                }
                SharedInput::ConsensusStreamEnded((id, version, mutability, _)) => {
                    debug_assert!(!changed_objects.contains_key(&id));
                    match mutability {
                        SharedObjectMutability::Mutable => Some((
                            id,
                            UnchangedConsensusKind::MutateConsensusStreamEnded(version),
                        )),
                        SharedObjectMutability::Immutable => Some((
                            id,
                            UnchangedConsensusKind::ReadConsensusStreamEnded(version),
                        )),
                        // This is current unreachable, because non exclusive writes are not exposed to
                        // user transactions yet, and so there is no way for their inputs to be deleted.
                        SharedObjectMutability::NonExclusiveWrite => Some((
                            id,
                            UnchangedConsensusKind::MutateConsensusStreamEnded(version),
                        )),
                    }
                }
                SharedInput::Cancelled((id, version)) => {
                    debug_assert!(!changed_objects.contains_key(&id));
                    Some((id, UnchangedConsensusKind::Cancelled(version)))
                }
            })
            .chain(
                loaded_per_epoch_config_objects
                    .iter()
                    .map(|id| (*id, UnchangedConsensusKind::PerEpochConfig)),
            ),
    );

    // Record system objects read during execution (e.g. the accumulator root) as read-only
    // consensus objects, so nodes executing from these effects (checkpoint execution, crash
    // recovery) can reproduce the read. Skip any that already appear as a changed object or as
    // an unchanged consensus object, keeping the version (and digest) each such entry records
    // to check it matches what the in-execution read observed.
    // With no system objects read there is nothing to record or check.
    if loaded_system_objects.is_empty() {
        return unchanged_consensus_objects;
    }
    let mut already_recorded: BTreeMap<'_, ObjectId, Option<VersionDigest>> =
        BTreeMap::new_in(bump);
    for (id, change) in changed_objects {
        let recorded = match &change.input_state {
            ObjectIn::Exist {
                version, digest, ..
            } => Some((*version, **digest)),
            _ => None,
        };
        already_recorded.insert(*id, recorded);
    }
    for (id, kind) in &unchanged_consensus_objects {
        let recorded = match kind {
            UnchangedConsensusKind::ReadOnlyRoot(version, digest) => Some((*version, **digest)),
            _ => None,
        };
        already_recorded.insert(*id, recorded);
    }
    for (id, version_digest) in loaded_system_objects {
        match already_recorded.get(id) {
            None => {
                unchanged_consensus_objects.push((
                    *id,
                    UnchangedConsensusKind::ReadOnlyRoot(
                        version_digest.0,
                        containers::alloc(bump, version_digest.1),
                    ),
                ));
            }
            Some(recorded) => {
                // The existing entry must record the same version the in-execution read
                // observed. `None` means the entry's kind carries no version to compare, which
                // no implicitly readable system object should ever coincide with. The
                // reference's `debug_fatal!` panics in debug builds and only logs in release.
                debug_assert!(
                    *recorded == Some(*version_digest),
                    "system object {id} read at version {version_digest:?} but its \
                     effects entry records {recorded:?}"
                );
            }
        }
    }
    unchanged_consensus_objects
}

/// `TransactionEffectsV2::check_invariant`: what's the invariant of the
/// effects, and the semantics of the combinations in object changes.
#[cfg(debug_assertions)]
fn check_invariant(
    bump: &Bump,
    lamport_version: SequenceNumber,
    changed_objects: &VecMap<'_, ObjectId, EffectsObjectChange<'_>>,
    gas_object: Option<ObjectId>,
    unchanged_consensus_objects: &[(ObjectId, UnchangedConsensusKind<'_>)],
) {
    use crate::execution::is_system_package;

    let is_shared = |o: &Owner<'_>| matches!(o, Owner::Shared { .. });
    let is_immutable = |o: &Owner<'_>| matches!(o, Owner::Immutable);

    let mut unique_ids = containers::hash_set(
        bump,
        changed_objects.len() + unchanged_consensus_objects.len(),
    );
    for (id, change) in changed_objects {
        assert!(unique_ids.insert(*id));
        match (
            &change.input_state,
            &change.output_state,
            &change.id_operation,
        ) {
            (ObjectIn::NotExist, ObjectOut::NotExist, IdOperation::Created) => {
                // created and then wrapped Move object.
            }
            (ObjectIn::NotExist, ObjectOut::NotExist, IdOperation::Deleted) => {
                // unwrapped and then deleted Move object.
            }
            (ObjectIn::NotExist, ObjectOut::ObjectWrite(_, owner), IdOperation::None) => {
                // unwrapped Move object.
                // It's not allowed to make an object shared after unwrapping.
                assert!(!is_shared(owner));
            }
            (ObjectIn::NotExist, ObjectOut::ObjectWrite(..), IdOperation::Created) => {
                // created Move object.
            }
            (ObjectIn::NotExist, ObjectOut::PackageWrite(..), IdOperation::Created) => {
                // created Move package or user Move package upgrade.
            }
            (
                ObjectIn::Exist {
                    version: old_version,
                    owner: old_owner,
                    ..
                },
                ObjectOut::NotExist,
                IdOperation::None,
            ) => {
                // wrapped.
                assert!(*old_version < lamport_version);
                assert!(
                    !is_shared(old_owner) && !is_immutable(old_owner),
                    "Cannot wrap shared or immutable object"
                );
            }
            (
                ObjectIn::Exist {
                    version: old_version,
                    owner: old_owner,
                    ..
                },
                ObjectOut::NotExist,
                IdOperation::Deleted,
            ) => {
                // deleted.
                assert!(*old_version < lamport_version);
                assert!(!is_immutable(old_owner), "Cannot delete immutable object");
            }
            (
                ObjectIn::Exist {
                    version: old_version,
                    digest: old_digest,
                    owner: old_owner,
                },
                ObjectOut::ObjectWrite(new_digest, new_owner),
                IdOperation::None,
            ) => {
                // mutated.
                assert!(*old_version < lamport_version);
                assert_ne!(old_digest, new_digest);
                assert!(!is_immutable(old_owner), "Cannot mutate immutable object");
                if is_shared(old_owner) {
                    assert!(is_shared(new_owner), "Cannot un-share an object");
                } else {
                    assert!(!is_shared(new_owner), "Cannot share an existing object");
                }
            }
            (
                ObjectIn::Exist {
                    version: old_version,
                    digest: old_digest,
                    owner: old_owner,
                },
                ObjectOut::PackageWrite(new_version, new_digest),
                IdOperation::None,
            ) => {
                // system package upgrade.
                assert!(
                    is_immutable(old_owner) && is_system_package(id),
                    "Must be a system package"
                );
                assert_eq!(*old_version + 1, *new_version);
                assert_ne!(old_digest, new_digest);
            }
            (ObjectIn::NotExist, ObjectOut::AccumulatorWriteV1(_), IdOperation::None) => {
                // This is an accumulator write.
            }
            _ => {
                panic!("Impossible object change: {:?}, {:?}", id, change);
            }
        }
    }
    // Make sure that gas object, if present, has an address owner.
    if let Some(gas_id) = gas_object
        && let Some(change) = changed_objects.get(&gas_id)
    {
        match &change.output_state {
            ObjectOut::ObjectWrite(_, owner) => {
                assert!(matches!(owner, Owner::AddressOwner(_)));
            }
            // A deleted gas coin reports a default address owner.
            ObjectOut::NotExist => {}
            _ => panic!("Gas object must be an ObjectWrite or Deleted in changed_objects"),
        }
    }

    for (id, _) in unchanged_consensus_objects {
        assert!(
            unique_ids.insert(*id),
            "Duplicate object id: {:?}\n{:#?}",
            id,
            unchanged_consensus_objects
        );
    }
}
