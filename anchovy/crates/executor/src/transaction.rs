// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The parts of `sui_types::transaction` execution uses for its input objects.

use containers::{BTreeMap, BTreeSet, Bump, Vec};
use exec_types::base::ObjectRef;
use exec_types::object::Object;
use messages::base::{Digest, ObjectId, SequenceNumber, TransactionDigest};
use messages::object::Owner;
pub use messages::transaction::SharedObjectMutability;

/// `(SequenceNumber, ObjectDigest)`.
pub type VersionDigest = (SequenceNumber, Digest);

/// `SequenceNumber::MAX` and the cancellation markers above it.
pub const SEQUENCE_NUMBER_MAX: SequenceNumber = 0x7fff_ffff_ffff_ffff;
pub const CANCELLED_READ: SequenceNumber = SEQUENCE_NUMBER_MAX + 1;
pub const CONGESTED: SequenceNumber = SEQUENCE_NUMBER_MAX + 2;
pub const RANDOMNESS_UNAVAILABLE: SequenceNumber = SEQUENCE_NUMBER_MAX + 3;

/// `SequenceNumber::lamport_increment`: one more than the greatest of `inputs`.
///
/// # Panics
/// If an input is `u64::MAX`, as the reference does.
pub fn lamport_increment(inputs: impl IntoIterator<Item = SequenceNumber>) -> SequenceNumber {
    let max_input = inputs.into_iter().fold(0, std::cmp::max);

    // TODO: Ensure this never overflows.
    // Option 1: Freeze the object when sequence number reaches MAX.
    // Option 2: Reject tx with MAX sequence number.
    // Issue #182.
    assert_ne!(max_input, u64::MAX);

    max_input + 1
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub enum InputObjectKind {
    // A Move package, must be immutable.
    MovePackage(ObjectId),
    // A Move object, either immutable, or owned mutable.
    ImmOrOwnedMoveObject(ObjectRef),
    // A Move object that's shared and mutable.
    SharedMoveObject {
        id: ObjectId,
        initial_shared_version: SequenceNumber,
        mutability: SharedObjectMutability,
    },
}

impl InputObjectKind {
    pub fn object_id(&self) -> ObjectId {
        match self {
            InputObjectKind::MovePackage(id) => *id,
            InputObjectKind::ImmOrOwnedMoveObject((id, _, _)) => *id,
            InputObjectKind::SharedMoveObject { id, .. } => *id,
        }
    }

    pub fn is_shared_object(&self) -> bool {
        matches!(self, Self::SharedMoveObject { .. })
    }
}

/// The result of reading an object for execution. Because shared objects may be deleted, one
/// possible result of reading a shared object is that ObjectReadResultKind::Deleted is returned.
#[derive(Clone, Copy, Debug)]
pub struct ObjectReadResult<'a> {
    pub input_object_kind: InputObjectKind,
    pub object: ObjectReadResultKind<'a>,
}

#[derive(Clone, Copy, Debug)]
pub enum ObjectReadResultKind<'a> {
    Object(Object<'a>),
    // The version of the object that the transaction intended to read, and the digest of the tx
    // that removed it from consensus.
    ObjectConsensusStreamEnded(SequenceNumber, TransactionDigest),
    // A shared object in a cancelled transaction. The sequence number embeds cancellation reason.
    CancelledTransactionSharedObject(SequenceNumber),
}

impl ObjectReadResultKind<'_> {
    pub fn is_cancelled(&self) -> bool {
        matches!(
            self,
            ObjectReadResultKind::CancelledTransactionSharedObject(_)
        )
    }

    pub fn version(&self) -> SequenceNumber {
        match self {
            ObjectReadResultKind::Object(object) => object.version(),
            ObjectReadResultKind::ObjectConsensusStreamEnded(seq, _) => *seq,
            ObjectReadResultKind::CancelledTransactionSharedObject(seq) => *seq,
        }
    }
}

impl<'a> ObjectReadResult<'a> {
    pub fn new(input_object_kind: InputObjectKind, object: ObjectReadResultKind<'a>) -> Self {
        if let (
            InputObjectKind::ImmOrOwnedMoveObject(_),
            ObjectReadResultKind::ObjectConsensusStreamEnded(_, _),
        ) = (&input_object_kind, &object)
        {
            panic!("only consensus objects can be ObjectConsensusStreamEnded");
        }

        if let (
            InputObjectKind::ImmOrOwnedMoveObject(_),
            ObjectReadResultKind::CancelledTransactionSharedObject(_),
        ) = (&input_object_kind, &object)
        {
            panic!("only consensus objects can be CancelledTransactionSharedObject");
        }

        Self {
            input_object_kind,
            object,
        }
    }

    pub fn id(&self) -> ObjectId {
        self.input_object_kind.object_id()
    }

    pub fn as_object(&self) -> Option<&Object<'a>> {
        match &self.object {
            ObjectReadResultKind::Object(object) => Some(object),
            ObjectReadResultKind::ObjectConsensusStreamEnded(_, _) => None,
            ObjectReadResultKind::CancelledTransactionSharedObject(_) => None,
        }
    }

    pub fn is_mutable(&self) -> bool {
        match (&self.input_object_kind, &self.object) {
            (InputObjectKind::MovePackage(_), _) => false,
            (InputObjectKind::ImmOrOwnedMoveObject(_), ObjectReadResultKind::Object(object)) => {
                !object.is_immutable()
            }
            (
                InputObjectKind::ImmOrOwnedMoveObject(_),
                ObjectReadResultKind::ObjectConsensusStreamEnded(_, _),
            ) => unreachable!(),
            (
                InputObjectKind::ImmOrOwnedMoveObject(_),
                ObjectReadResultKind::CancelledTransactionSharedObject(_),
            ) => unreachable!(),
            (InputObjectKind::SharedMoveObject { mutability, .. }, _) => match mutability {
                SharedObjectMutability::Mutable => true,
                SharedObjectMutability::Immutable => false,
                SharedObjectMutability::NonExclusiveWrite => false,
            },
        }
    }

    pub fn is_shared_object(&self) -> bool {
        self.input_object_kind.is_shared_object()
    }

    pub fn is_consensus_stream_ended(&self) -> bool {
        self.consensus_stream_end_info().is_some()
    }

    pub fn consensus_stream_end_info(&self) -> Option<(SequenceNumber, TransactionDigest)> {
        match &self.object {
            ObjectReadResultKind::ObjectConsensusStreamEnded(v, tx) => Some((*v, *tx)),
            _ => None,
        }
    }

    /// Return the object ref iff the object is an address-owned object (i.e. not shared, not immutable).
    pub fn get_address_owned_objref(&self) -> Option<ObjectRef> {
        match (&self.input_object_kind, &self.object) {
            (InputObjectKind::MovePackage(_), _) => None,
            (
                InputObjectKind::ImmOrOwnedMoveObject(objref),
                ObjectReadResultKind::Object(object),
            ) => {
                if object.is_immutable() {
                    None
                } else {
                    Some(*objref)
                }
            }
            (
                InputObjectKind::ImmOrOwnedMoveObject(_),
                ObjectReadResultKind::ObjectConsensusStreamEnded(_, _),
            ) => unreachable!(),
            (
                InputObjectKind::ImmOrOwnedMoveObject(_),
                ObjectReadResultKind::CancelledTransactionSharedObject(_),
            ) => unreachable!(),
            (InputObjectKind::SharedMoveObject { .. }, _) => None,
        }
    }

    pub fn is_address_owned(&self) -> bool {
        self.get_address_owned_objref().is_some()
    }

    pub fn get_previous_transaction(&self) -> Option<TransactionDigest> {
        match &self.object {
            ObjectReadResultKind::Object(obj) => Some(obj.previous_transaction()),
            ObjectReadResultKind::ObjectConsensusStreamEnded(_, digest) => Some(*digest),
            ObjectReadResultKind::CancelledTransactionSharedObject(_) => None,
        }
    }
}

/// The reference's `InputObjects`, which `CheckedInputObjects` wraps once
/// `sui-transaction-checks` has passed them; anchovy's input checks mint `InputsChecked`
/// transactions instead, so the execution entry point takes these directly.
#[derive(Debug)]
pub struct InputObjects<'a> {
    objects: Vec<'a, ObjectReadResult<'a>>,
}

impl<'a> InputObjects<'a> {
    pub fn new(objects: Vec<'a, ObjectReadResult<'a>>) -> Self {
        Self { objects }
    }

    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    pub fn contains_consensus_stream_ended_objects(&self) -> bool {
        self.objects
            .iter()
            .any(|obj| obj.is_consensus_stream_ended())
    }

    // Returns IDs of objects responsible for a transaction being cancelled, and the corresponding
    // reason for cancellation.
    pub fn get_cancelled_objects(
        &self,
        bump: &'a Bump,
    ) -> Option<(Vec<'a, ObjectId>, SequenceNumber)> {
        let mut contains_cancelled = false;
        let mut cancel_reason = None;
        let mut cancelled_objects = Vec::new_in(bump);
        for obj in &self.objects {
            if let ObjectReadResultKind::CancelledTransactionSharedObject(version) = obj.object {
                contains_cancelled = true;
                if version == CONGESTED || version == RANDOMNESS_UNAVAILABLE {
                    // Verify we don't have multiple cancellation reasons.
                    assert!(cancel_reason.is_none() || cancel_reason == Some(version));
                    cancel_reason = Some(version);
                    cancelled_objects.push(obj.id());
                }
            }
        }

        if !cancelled_objects.is_empty() {
            Some((
                cancelled_objects,
                cancel_reason
                    .expect("there should be a cancel reason if there are cancelled objects"),
            ))
        } else {
            assert!(!contains_cancelled);
            None
        }
    }

    pub fn filter_owned_objects(&self, bump: &'a Bump) -> Vec<'a, ObjectRef> {
        let mut owned_objects = Vec::new_in(bump);
        owned_objects.extend(
            self.objects
                .iter()
                .filter_map(|obj| obj.get_address_owned_objref()),
        );
        owned_objects
    }

    pub fn transaction_dependencies(&self, bump: &'a Bump) -> BTreeSet<'a, TransactionDigest> {
        let mut dependencies = BTreeSet::new_in(bump);
        dependencies.extend(
            self.objects
                .iter()
                .filter_map(|obj| obj.get_previous_transaction()),
        );
        dependencies
    }

    /// All inputs that will be directly mutated by the transaction. This does
    /// not include SharedObjectMutability::NonExclusiveWrite inputs.
    pub fn exclusive_mutable_inputs(
        &self,
        bump: &'a Bump,
    ) -> BTreeMap<'a, ObjectId, (VersionDigest, Owner<'a>)> {
        let mut inputs = BTreeMap::new_in(bump);
        inputs.extend(self.mutables_with_input_kinds().filter_map(
            |(id, (version, owner, kind))| match kind {
                InputObjectKind::SharedMoveObject { mutability, .. } => match mutability {
                    SharedObjectMutability::Mutable => Some((id, (version, owner))),
                    SharedObjectMutability::Immutable => None,
                    SharedObjectMutability::NonExclusiveWrite => None,
                },
                _ => Some((id, (version, owner))),
            },
        ));
        inputs
    }

    pub fn non_exclusive_input_objects(
        &self,
        bump: &'a Bump,
    ) -> BTreeMap<'a, ObjectId, Object<'a>> {
        let mut objects = BTreeMap::new_in(bump);
        objects.extend(self.objects.iter().filter_map(|read_result| {
            match (read_result.as_object(), read_result.input_object_kind) {
                (
                    Some(object),
                    InputObjectKind::SharedMoveObject {
                        mutability: SharedObjectMutability::NonExclusiveWrite,
                        ..
                    },
                ) => Some((read_result.id(), *object)),
                _ => None,
            }
        }));
        objects
    }

    /// All inputs that can be taken as &mut T, which includes both
    /// SharedObjectMutability::Mutable and SharedObjectMutability::NonExclusiveWrite inputs.
    pub fn all_mutable_inputs(
        &self,
        bump: &'a Bump,
    ) -> BTreeMap<'a, ObjectId, (VersionDigest, Owner<'a>)> {
        let mut inputs = BTreeMap::new_in(bump);
        inputs.extend(self.mutables_with_input_kinds().filter_map(
            |(id, (version, owner, kind))| match kind {
                InputObjectKind::SharedMoveObject { mutability, .. } => match mutability {
                    SharedObjectMutability::Mutable => Some((id, (version, owner))),
                    SharedObjectMutability::Immutable => None,
                    SharedObjectMutability::NonExclusiveWrite => Some((id, (version, owner))),
                },
                _ => Some((id, (version, owner))),
            },
        ));
        inputs
    }

    fn mutables_with_input_kinds(
        &self,
    ) -> impl Iterator<Item = (ObjectId, (VersionDigest, Owner<'a>, InputObjectKind))> + '_ {
        self.objects.iter().filter_map(
            |ObjectReadResult {
                 input_object_kind,
                 object,
             }| match (input_object_kind, object) {
                (InputObjectKind::MovePackage(_), _) => None,
                (
                    InputObjectKind::ImmOrOwnedMoveObject(object_ref),
                    ObjectReadResultKind::Object(object),
                ) => {
                    if object.is_immutable() {
                        None
                    } else {
                        Some((
                            object_ref.0,
                            (
                                (object_ref.1, object_ref.2),
                                *object.owner(),
                                *input_object_kind,
                            ),
                        ))
                    }
                }
                (
                    InputObjectKind::ImmOrOwnedMoveObject(_),
                    ObjectReadResultKind::ObjectConsensusStreamEnded(_, _),
                ) => {
                    unreachable!()
                }
                (
                    InputObjectKind::SharedMoveObject { .. },
                    ObjectReadResultKind::ObjectConsensusStreamEnded(_, _),
                ) => None,
                (
                    InputObjectKind::SharedMoveObject { mutability, .. },
                    ObjectReadResultKind::Object(object),
                ) => match *mutability {
                    SharedObjectMutability::Mutable => {
                        let oref = object.compute_object_reference();
                        Some((
                            oref.0,
                            ((oref.1, oref.2), *object.owner(), *input_object_kind),
                        ))
                    }
                    SharedObjectMutability::Immutable => None,
                    SharedObjectMutability::NonExclusiveWrite => {
                        let oref = object.compute_object_reference();
                        Some((
                            oref.0,
                            ((oref.1, oref.2), *object.owner(), *input_object_kind),
                        ))
                    }
                },
                (
                    InputObjectKind::ImmOrOwnedMoveObject(_),
                    ObjectReadResultKind::CancelledTransactionSharedObject(_),
                ) => {
                    unreachable!()
                }
                (
                    InputObjectKind::SharedMoveObject { .. },
                    ObjectReadResultKind::CancelledTransactionSharedObject(_),
                ) => None,
            },
        )
    }

    /// The version to set on objects created by the computation that `self` is input to.
    /// Guaranteed to be strictly greater than the versions of all input objects and objects
    /// received in the transaction.
    pub fn lamport_timestamp(&self, receiving_objects: &[ObjectRef]) -> SequenceNumber {
        let input_versions = self
            .objects
            .iter()
            .filter_map(|object| match &object.object {
                ObjectReadResultKind::Object(object) => object.try_as_move().map(|m| m.version),
                ObjectReadResultKind::ObjectConsensusStreamEnded(v, _) => Some(*v),
                ObjectReadResultKind::CancelledTransactionSharedObject(_) => None,
            })
            .chain(receiving_objects.iter().map(|object_ref| object_ref.1));

        lamport_increment(input_versions)
    }

    pub fn object_kinds(&self) -> impl Iterator<Item = &InputObjectKind> {
        self.objects.iter().map(
            |ObjectReadResult {
                 input_object_kind, ..
             }| input_object_kind,
        )
    }

    pub fn consensus_stream_ended_objects(
        &self,
        bump: &'a Bump,
    ) -> BTreeMap<'a, ObjectId, SequenceNumber> {
        let mut objects = BTreeMap::new_in(bump);
        objects.extend(self.objects.iter().filter_map(|obj| {
            if let InputObjectKind::SharedMoveObject {
                id,
                initial_shared_version,
                ..
            } = obj.input_object_kind
            {
                obj.is_consensus_stream_ended()
                    .then_some((id, initial_shared_version))
            } else {
                None
            }
        }));
        objects
    }

    /// The reference clones each object; these are `Copy` views.
    pub fn into_object_map(self, bump: &'a Bump) -> BTreeMap<'a, ObjectId, Object<'a>> {
        let mut objects = BTreeMap::new_in(bump);
        objects.extend(
            self.objects
                .iter()
                .filter_map(|o| o.as_object().map(|object| (o.id(), *object))),
        );
        objects
    }

    pub fn push(&mut self, object: ObjectReadResult<'a>) {
        self.objects.push(object);
    }

    pub fn iter(&self) -> impl Iterator<Item = &ObjectReadResult<'a>> {
        self.objects.iter()
    }

    pub fn iter_objects(&self) -> impl Iterator<Item = &Object<'a>> {
        self.objects.iter().filter_map(|o| o.as_object())
    }
}
