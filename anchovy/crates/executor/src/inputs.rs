// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A transaction's loaded input objects, as execution takes them. The loader hands over each
//! input object's kind and what it read; the facts the reference derives from `InputObjects`
//! when it builds the temporary store are derived here, once, and read during execution.

use containers::{BTreeMap, BTreeSet, Bump, Vec};
use exec_types::base::ObjectRef;
use exec_types::object::Object;
use messages::base::{ObjectId, SequenceNumber, TransactionDigest};
use messages::object::Owner;
use messages::transaction::SharedObjectMutability;

use crate::effects::{SharedInput, VersionDigest};

/// `SequenceNumber::MAX`.
const SEQUENCE_NUMBER_MAX: SequenceNumber = 0x7fff_ffff_ffff_ffff;
/// `SequenceNumber::CONGESTED`.
pub const CONGESTED: SequenceNumber = SEQUENCE_NUMBER_MAX + 2;
/// `SequenceNumber::RANDOMNESS_UNAVAILABLE`.
pub const RANDOMNESS_UNAVAILABLE: SequenceNumber = SEQUENCE_NUMBER_MAX + 3;

/// `sui_types::transaction::InputObjectKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputObjectKind {
    MovePackage(ObjectId),
    ImmOrOwnedMoveObject(ObjectRef),
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
}

/// `sui_types::transaction::ObjectReadResultKind`: what the loader read for an input.
#[derive(Clone, Copy, Debug)]
pub enum InputState<'a> {
    Object(Object<'a>),
    /// The version the transaction intended to read, and the digest of the transaction that
    /// removed the object from consensus.
    ConsensusStreamEnded(SequenceNumber, TransactionDigest),
    /// A shared object of a cancelled transaction; the version is the cancellation reason.
    Cancelled(SequenceNumber),
}

/// One loaded input object.
#[derive(Clone, Copy, Debug)]
pub struct LoadedInput<'a> {
    pub kind: InputObjectKind,
    pub state: InputState<'a>,
}

impl<'a> LoadedInput<'a> {
    /// # Panics
    /// If an owned or immutable input is not an object, as `ObjectReadResult::new` does.
    pub fn new(kind: InputObjectKind, state: InputState<'a>) -> LoadedInput<'a> {
        if let InputObjectKind::ImmOrOwnedMoveObject(_) = kind {
            assert!(
                matches!(state, InputState::Object(_)),
                "only consensus objects can be stream-ended or cancelled"
            );
        }
        LoadedInput { kind, state }
    }

    pub fn id(&self) -> ObjectId {
        self.kind.object_id()
    }

    pub fn as_object(&self) -> Option<&Object<'a>> {
        match &self.state {
            InputState::Object(object) => Some(object),
            InputState::ConsensusStreamEnded(_, _) | InputState::Cancelled(_) => None,
        }
    }

    fn is_consensus_stream_ended(&self) -> bool {
        matches!(self.state, InputState::ConsensusStreamEnded(_, _))
    }

    /// `ObjectReadResult::to_shared_input`.
    fn shared_input(&self) -> Option<SharedInput> {
        match self.kind {
            InputObjectKind::MovePackage(_) => None,
            InputObjectKind::ImmOrOwnedMoveObject(_) => None,
            InputObjectKind::SharedMoveObject { id, mutability, .. } => Some(match &self.state {
                InputState::Object(obj) => SharedInput::Existing(obj.compute_object_reference()),
                InputState::ConsensusStreamEnded(seq, digest) => {
                    SharedInput::ConsensusStreamEnded((id, *seq, mutability, *digest))
                }
                InputState::Cancelled(seq) => SharedInput::Cancelled((id, *seq)),
            }),
        }
    }

    /// `ObjectReadResult::get_previous_transaction`.
    fn get_previous_transaction(&self) -> Option<TransactionDigest> {
        match &self.state {
            InputState::Object(obj) => Some(obj.previous_transaction()),
            InputState::ConsensusStreamEnded(_, digest) => Some(*digest),
            InputState::Cancelled(_) => None,
        }
    }
}

/// A transaction's inputs, loaded. Nothing here changes during execution.
pub struct ExecutionInputs<'a> {
    /// In the transaction's order (`TransactionData::input_objects`).
    loaded: Vec<'a, LoadedInput<'a>>,
    /// `InputObjects::into_object_map`: the inputs that are objects. The reference's temporary
    /// store holds a copy; here it is borrowed.
    objects: BTreeMap<'a, ObjectId, Object<'a>>,
    /// `InputObjects::exclusive_mutable_inputs`.
    exclusive_mutable_inputs: BTreeMap<'a, ObjectId, (VersionDigest, Owner<'a>)>,
    /// `InputObjects::non_exclusive_input_objects`, as ids: the original objects are in
    /// `objects`, which nothing changes.
    non_exclusive_inputs: BTreeSet<'a, ObjectId>,
    /// `InputObjects::consensus_stream_ended_objects`: id to initial shared version.
    consensus_stream_ended: BTreeMap<'a, ObjectId, SequenceNumber>,
    receiving: &'a [ObjectRef],
    lamport_timestamp: SequenceNumber,
}

impl<'a> ExecutionInputs<'a> {
    /// `receiving` are the objects the transaction may receive (`TransactionKind::
    /// receiving_objects`), which are not inputs.
    pub fn new(
        bump: &'a Bump,
        loaded: Vec<'a, LoadedInput<'a>>,
        receiving: &'a [ObjectRef],
    ) -> ExecutionInputs<'a> {
        let mut objects = BTreeMap::new_in(bump);
        let mut exclusive_mutable_inputs = BTreeMap::new_in(bump);
        let mut non_exclusive_inputs = BTreeSet::new_in(bump);
        let mut consensus_stream_ended = BTreeMap::new_in(bump);
        for input in &loaded {
            if let Some(object) = input.as_object() {
                objects.insert(input.id(), *object);
            }
            if let Some((id, entry, kind)) = mutable_with_input_kind(input) {
                // `exclusive_mutable_inputs` leaves out non-exclusive writes.
                let exclusive = match kind {
                    InputObjectKind::SharedMoveObject { mutability, .. } => match mutability {
                        SharedObjectMutability::Mutable => true,
                        SharedObjectMutability::Immutable => false,
                        SharedObjectMutability::NonExclusiveWrite => false,
                    },
                    _ => true,
                };
                if exclusive {
                    exclusive_mutable_inputs.insert(id, entry);
                }
            }
            if let (
                Some(_),
                InputObjectKind::SharedMoveObject {
                    mutability: SharedObjectMutability::NonExclusiveWrite,
                    ..
                },
            ) = (input.as_object(), input.kind)
            {
                non_exclusive_inputs.insert(input.id());
            }
            if let InputObjectKind::SharedMoveObject {
                id,
                initial_shared_version,
                ..
            } = input.kind
                && input.is_consensus_stream_ended()
            {
                consensus_stream_ended.insert(id, initial_shared_version);
            }
        }
        let lamport_timestamp = lamport_timestamp(&loaded, receiving);
        ExecutionInputs {
            loaded,
            objects,
            exclusive_mutable_inputs,
            non_exclusive_inputs,
            consensus_stream_ended,
            receiving,
            lamport_timestamp,
        }
    }

    pub fn loaded(&self) -> &[LoadedInput<'a>] {
        &self.loaded
    }

    pub fn objects(&self) -> &BTreeMap<'a, ObjectId, Object<'a>> {
        &self.objects
    }

    pub fn exclusive_mutable_inputs(&self) -> &BTreeMap<'a, ObjectId, (VersionDigest, Owner<'a>)> {
        &self.exclusive_mutable_inputs
    }

    /// The non-exclusive write inputs, as they were before execution.
    pub fn non_exclusive_input_objects(&self) -> impl Iterator<Item = (&ObjectId, &Object<'a>)> {
        self.non_exclusive_inputs.iter().map(|id| {
            (
                id,
                self.objects
                    .get(id)
                    .expect("a non-exclusive input is an input object"),
            )
        })
    }

    pub fn consensus_stream_ended_objects(&self) -> &BTreeMap<'a, ObjectId, SequenceNumber> {
        &self.consensus_stream_ended
    }

    pub fn receiving_objects(&self) -> &'a [ObjectRef] {
        self.receiving
    }

    /// The version to set on objects created by the computation that `self` is input to.
    /// Guaranteed to be strictly greater than the versions of all input objects and objects
    /// received in the transaction.
    pub fn lamport_timestamp(&self) -> SequenceNumber {
        self.lamport_timestamp
    }

    /// `InputObjects::filter_shared_objects`.
    pub fn filter_shared_objects(&self, bump: &'a Bump) -> Vec<'a, SharedInput> {
        let mut shared = Vec::new_in(bump);
        shared.extend(self.loaded.iter().filter_map(LoadedInput::shared_input));
        shared
    }

    /// `InputObjects::transaction_dependencies`.
    pub fn transaction_dependencies(&self, bump: &'a Bump) -> BTreeSet<'a, TransactionDigest> {
        let mut dependencies = BTreeSet::new_in(bump);
        dependencies.extend(
            self.loaded
                .iter()
                .filter_map(LoadedInput::get_previous_transaction),
        );
        dependencies
    }

    /// `InputObjects::get_cancelled_objects`: the objects responsible for the transaction's
    /// cancellation, and the reason.
    ///
    /// # Panics
    /// On more than one reason, or a cancelled object without one, as the reference does.
    pub fn get_cancelled_objects(
        &self,
        bump: &'a Bump,
    ) -> Option<(Vec<'a, ObjectId>, SequenceNumber)> {
        let mut contains_cancelled = false;
        let mut cancel_reason = None;
        let mut cancelled_objects = Vec::new_in(bump);
        for obj in &self.loaded {
            if let InputState::Cancelled(version) = obj.state {
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
}

/// `InputObjects::mutables_with_input_kinds`, for one input.
fn mutable_with_input_kind<'a>(
    input: &LoadedInput<'a>,
) -> Option<(ObjectId, (VersionDigest, Owner<'a>), InputObjectKind)> {
    match (&input.kind, &input.state) {
        (InputObjectKind::MovePackage(_), _) => None,
        (InputObjectKind::ImmOrOwnedMoveObject(object_ref), InputState::Object(object)) => {
            if object.is_immutable() {
                None
            } else {
                Some((
                    object_ref.0,
                    ((object_ref.1, object_ref.2), *object.owner()),
                    input.kind,
                ))
            }
        }
        (InputObjectKind::ImmOrOwnedMoveObject(_), _) => unreachable!(),
        (InputObjectKind::SharedMoveObject { .. }, InputState::ConsensusStreamEnded(_, _)) => None,
        (InputObjectKind::SharedMoveObject { mutability, .. }, InputState::Object(object)) => {
            match *mutability {
                SharedObjectMutability::Mutable | SharedObjectMutability::NonExclusiveWrite => {
                    let oref = object.compute_object_reference();
                    Some((oref.0, ((oref.1, oref.2), *object.owner()), input.kind))
                }
                SharedObjectMutability::Immutable => None,
            }
        }
        (InputObjectKind::SharedMoveObject { .. }, InputState::Cancelled(_)) => None,
    }
}

/// `InputObjects::lamport_timestamp`.
fn lamport_timestamp(loaded: &[LoadedInput<'_>], receiving: &[ObjectRef]) -> SequenceNumber {
    let input_versions = loaded
        .iter()
        .filter_map(|input| match &input.state {
            InputState::Object(object) => object.try_as_move().map(|m| m.version),
            InputState::ConsensusStreamEnded(v, _) => Some(*v),
            InputState::Cancelled(_) => None,
        })
        .chain(receiving.iter().map(|object_ref| object_ref.1));

    // `SequenceNumber::lamport_increment`
    let max_input = input_versions.fold(0, std::cmp::max);
    // TODO: Ensure this never overflows.
    assert_ne!(max_input, u64::MAX);
    max_input + 1
}
