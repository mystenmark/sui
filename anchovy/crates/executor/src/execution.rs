// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The parts of `sui_types::execution` only the executor uses.

use containers::{BTreeMap, BTreeSet, Bump, Vec};
use exec_types::assert_invariant;
use exec_types::error::ExecutionError;
use exec_types::object::Object;
use messages::base::{ObjectId, SequenceNumber, TransactionDigest};
use messages::effects::Event;
use messages::object::{Data, MoveObject, Owner};

use crate::accumulator_event::AccumulatorEvent;

/// Used by sui-execution v1 and above, to capture the execution results from Move.
/// The results represent the primitive information that can then be used to construct
/// both transaction effects V1 and V2.
#[derive(Debug)]
pub struct ExecutionResultsV2<'a> {
    /// All objects written regardless of whether they were mutated, created, or unwrapped.
    pub written_objects: BTreeMap<'a, ObjectId, Object<'a>>,
    /// All objects that existed prior to this transaction, and are modified in this transaction.
    /// This includes any type of modification, including mutated, wrapped and deleted objects.
    pub modified_objects: BTreeSet<'a, ObjectId>,
    /// All object IDs created in this transaction.
    pub created_object_ids: BTreeSet<'a, ObjectId>,
    /// All object IDs deleted in this transaction.
    /// No object ID should be in both created_object_ids and deleted_object_ids.
    pub deleted_object_ids: BTreeSet<'a, ObjectId>,
    /// All Move events emitted in this transaction.
    pub user_events: Vec<'a, Event<'a>>,
    /// All accumulator events emitted in this transaction.
    pub accumulator_events: Vec<'a, AccumulatorEvent<'a>>,

    /// Used to track SUI conservation in settlement transactions. Settlement transactions
    /// gather up withdraws and deposits from other transactions, and record them to accumulator
    /// fields. The settlement transaction records the total amount of SUI being disbursed here,
    /// so that we can verify that the amount stored in the fields at the end of the transaction
    /// is correct.
    pub settlement_input_sui: u64,
    pub settlement_output_sui: u64,
}

impl<'a> ExecutionResultsV2<'a> {
    /// The reference's `Default`: empty, in `bump`.
    pub fn new_in(bump: &'a Bump) -> Self {
        Self {
            written_objects: BTreeMap::new_in(bump),
            modified_objects: BTreeSet::new_in(bump),
            created_object_ids: BTreeSet::new_in(bump),
            deleted_object_ids: BTreeSet::new_in(bump),
            user_events: Vec::new_in(bump),
            accumulator_events: Vec::new_in(bump),
            settlement_input_sui: 0,
            settlement_output_sui: 0,
        }
    }

    pub fn drop_writes(&mut self) {
        self.written_objects.clear();
        self.modified_objects.clear();
        self.created_object_ids.clear();
        self.deleted_object_ids.clear();
        self.user_events.clear();
        self.accumulator_events.clear();
    }

    /// If `consistent_merge` is true, the deletes and writes in `new_results` will update the
    /// results any existing writes and deletes in `self` respectively. If false, it is assumed
    /// that deletes and writes are disjoint.
    /// If `invariant_checks` is true, the function will check for disjointness between deleted
    /// and created/written objects.
    pub fn merge_results(
        &mut self,
        new_results: Self,
        consistent_merge: bool,
        invariant_checks: bool,
    ) -> Result<(), ExecutionError<'a>> {
        if consistent_merge {
            // An object written before the merge (e.g., gas coin written by smash_gas) may be
            // deleted by the new results (e.g., send_funds destroying the gas coin during PTB
            // execution). Remove such stale entries.
            for id in &new_results.deleted_object_ids {
                self.written_objects.remove(id);
                // additional hardening
                self.created_object_ids.remove(id);
            }
            // While not possible currently, we should ensure that any object previously marked as
            // deleted is now marked only as written
            for id in new_results.written_objects.keys() {
                self.deleted_object_ids.remove(id);
            }
        }

        self.written_objects.extend(new_results.written_objects);
        self.modified_objects.extend(new_results.modified_objects);
        self.created_object_ids
            .extend(new_results.created_object_ids);
        self.deleted_object_ids
            .extend(new_results.deleted_object_ids);

        if invariant_checks {
            // debug assert that deleted is disjoint with created and written
            assert_invariant!(
                self.deleted_object_ids
                    .is_disjoint(&self.created_object_ids),
                "Deleted object IDs should be disjoint with created object IDs"
            );
            assert_invariant!(
                self.written_objects
                    .keys()
                    .all(|id| !self.deleted_object_ids.contains(id)),
                "Deleted object IDs should be disjoint with written object IDs"
            );
        }
        self.user_events.extend(new_results.user_events);
        self.accumulator_events
            .extend(new_results.accumulator_events);
        self.settlement_input_sui += new_results.settlement_input_sui;
        self.settlement_output_sui += new_results.settlement_output_sui;
        Ok(())
    }

    pub fn update_version_and_previous_tx(
        &mut self,
        lamport_version: SequenceNumber,
        prev_tx: TransactionDigest,
        input_objects: &BTreeMap<'a, ObjectId, Object<'a>>,
        reshare_at_initial_version: bool,
    ) {
        for (id, obj) in self.written_objects.iter_mut() {
            // TODO: We can now get rid of the following logic by passing in lamport version
            // into the execution layer, and create new objects using the lamport version directly.

            // Update the version for the written object.
            match *obj.data() {
                Data::Move(o) => {
                    // Move objects all get the transaction's lamport timestamp
                    debug_assert!(
                        o.version < lamport_version,
                        "Not an increment: {} to {}",
                        o.version,
                        lamport_version
                    );
                    *obj = obj.with_data(Data::Move(MoveObject {
                        version: lamport_version,
                        ..o
                    }));
                }

                Data::Package(pkg) => {
                    // Modified packages get their version incremented (this is a special case that
                    // only applies to system packages).  All other packages can only be created,
                    // and they are left alone.
                    if self.modified_objects.contains(id) {
                        debug_assert!(is_system_package(id));
                        let mut pkg = pkg;
                        pkg.version += 1;
                        *obj = obj.with_data(Data::Package(pkg));
                    }
                }
            }

            // Record the version that the shared object was created at in its owner field.  Note,
            // this only works because shared objects must be created as shared (not created as
            // owned in one transaction and later converted to shared in another).
            if let Owner::Shared {
                initial_shared_version,
            } = *obj.owner()
            {
                let mut initial_shared_version = initial_shared_version;
                if self.created_object_ids.contains(id) {
                    assert_eq!(
                        initial_shared_version, 0,
                        "Initial version should be blank before this point for {id:?}",
                    );
                    initial_shared_version = lamport_version;
                }

                // Update initial_shared_version for reshared objects
                if reshare_at_initial_version
                    && let Some(Owner::Shared {
                        initial_shared_version: previous_initial_shared_version,
                    }) = input_objects.get(id).map(|obj| *obj.owner())
                {
                    debug_assert!(!self.created_object_ids.contains(id));
                    debug_assert!(!self.deleted_object_ids.contains(id));
                    debug_assert!(
                        initial_shared_version == 0
                            || initial_shared_version == previous_initial_shared_version
                    );

                    initial_shared_version = previous_initial_shared_version;
                }
                *obj = obj.with_owner(Owner::Shared {
                    initial_shared_version,
                });
            }

            // Record start version for ConsensusAddressOwner objects.
            if let Owner::ConsensusAddressOwner { owner, .. } = *obj.owner() {
                debug_assert!(!self.deleted_object_ids.contains(id));

                let start_version = if let Some(Owner::ConsensusAddressOwner {
                    start_version: previous_start_version,
                    owner: previous_owner,
                }) = input_objects.get(id).map(|obj| *obj.owner())
                {
                    if owner == previous_owner {
                        // Assign existing start_version in case a ConsensusAddressOwner object was
                        // transferred to the same owner.
                        previous_start_version
                    } else {
                        // If owner changes, we need to begin a new stream.
                        lamport_version
                    }
                } else {
                    // ConsensusAddressOwner object was created, transferred from another Owner
                    // type, or unwrapped, so we begin a new stream.
                    lamport_version
                };
                *obj = obj.with_owner(Owner::ConsensusAddressOwner {
                    start_version,
                    owner,
                });
            }

            *obj = obj.with_previous_transaction(prev_tx);
        }
    }
}

/// `sui_types::is_system_package`: the framework packages' ids.
pub fn is_system_package(id: &ObjectId) -> bool {
    const SYSTEM_PACKAGES: [u16; 5] = [0x1, 0x2, 0x3, 0xb, 0xdee9];
    SYSTEM_PACKAGES
        .iter()
        .any(|n| *id == ObjectId::from_u16(*n))
}

#[derive(Clone, Copy, Debug)]
pub enum ExecutionTiming {
    Success(std::time::Duration),
    Abort(std::time::Duration),
}

impl ExecutionTiming {
    pub fn is_abort(&self) -> bool {
        matches!(self, ExecutionTiming::Abort(_))
    }

    pub fn duration(&self) -> std::time::Duration {
        match self {
            ExecutionTiming::Success(duration) => *duration,
            ExecutionTiming::Abort(duration) => *duration,
        }
    }
}

pub type ResultWithTimings<'a, R, E> =
    Result<(R, Vec<'a, ExecutionTiming>), (E, Vec<'a, ExecutionTiming>)>;
