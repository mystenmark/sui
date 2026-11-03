// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `sui_adapter::accumulator_settlement`: settlement transactions executed in Rust, not by the
//! Move VM. Keys and values are encoded with `sui_types`' own types, so their BCS is the
//! reference's.

use containers::{BTreeMap, BTreeSet, Bump};
use exec_types::base::{SUI_ACCUMULATOR_ROOT_OBJECT_ID, SUI_FRAMEWORK_ADDRESS};
use exec_types::execution::DynamicallyLoadedObjectMetadata;
use exec_types::object::Object;
use exec_types::tx_context::TxContext;
use exec_types::type_tags::{
    check_type_input, move_object_type_in, to_move_struct_tag, to_move_type_tag,
};
use exec_types::{assert_invariant, checked_as, invariant_violation, make_invariant_violation};
use fastcrypto::hash::{Blake2b256, HashFunction};
use messages::base::{ObjectId, SequenceNumber, SuiAddress};
use messages::object::{MoveObject, MoveObjectType, Owner};
use messages::transaction::{
    Argument, CallArg, Command, ObjectArg, ProgrammableMoveCall, ProgrammableTransaction,
    SharedObjectMutability,
};
use move_core_types::language_storage::{StructTag as MoveStructTag, TypeTag as MoveTypeTag};
use move_core_types::u256::U256;
use serde::{Serialize, de::DeserializeOwned};
use std::sync::LazyLock;
use sui_protocol_config::ProtocolConfig;
use sui_types::accumulator_metadata::AccumulatorObjectCountKey;
use sui_types::accumulator_root::{AccumulatorKey, EventStreamHead, U128};
use sui_types::dynamic_field::{DynamicFieldInfo, Field, derive_dynamic_field_id};
use sui_types::id::UID;
use sui_types::{MoveTypeTagTrait, MoveTypeTagTraitGeneric};

use crate::error::ExecutionError;
use crate::execution::ExecutionResultsV2;
use crate::execution_value::ExecutionState;

const ACCUMULATOR_SETTLEMENT_MODULE: &str = "accumulator_settlement";
const ACCUMULATOR_METADATA_MODULE: &str = "accumulator_metadata";
const SETTLEMENT_PROLOGUE: &str = "settlement_prologue";
const SETTLE_U128: &str = "settle_u128";
const SETTLE_EVENTS: &str = "settle_events";
const RECORD_ACCUMULATOR_OBJECT_CHANGES: &str = "record_accumulator_object_changes";

const ROOT_ADDRESS: SuiAddress = SuiAddress(SUI_ACCUMULATOR_ROOT_OBJECT_ID.0);

fn root_id() -> sui_types::base_types::ObjectID {
    sui_types::base_types::ObjectID::new(SUI_ACCUMULATOR_ROOT_OBJECT_ID.0)
}

#[derive(Clone, Copy)]
struct ChildObject<'a> {
    original: Option<Object<'a>>,
    current: Option<Object<'a>>,
}

struct Context<'a, 's> {
    bump: &'a Bump,
    state: &'s dyn ExecutionState<'a>,
    protocol_config: &'s ProtocolConfig,
    tx_context: &'s TxContext,
    inputs: &'a [CallArg<'a>],
    root_argument: Argument,
    root: Object<'a>,
    children: BTreeMap<'a, ObjectId, ChildObject<'a>>,
    generated_ids: BTreeSet<'a, ObjectId>,
    input_sui: u64,
    output_sui: u64,
}

impl<'a, 's> Context<'a, 's> {
    fn new(
        bump: &'a Bump,
        protocol_config: &'s ProtocolConfig,
        state: &'s dyn ExecutionState<'a>,
        tx_context: &'s TxContext,
        inputs: &'a [CallArg<'a>],
    ) -> Result<Self, ExecutionError<'a>> {
        assert_invariant!(
            tx_context.sender() == SuiAddress([0; 32]),
            "Settlement sender must be the system address"
        );
        let mut root_argument = None;
        for (index, input) in inputs.iter().enumerate() {
            match input {
                CallArg::Pure(_) => (),
                CallArg::Object(ObjectArg::SharedObject(arg))
                    if matches!(
                        arg.mutability(),
                        SharedObjectMutability::NonExclusiveWrite | SharedObjectMutability::Mutable
                    ) =>
                {
                    assert_invariant!(
                        arg.id == SUI_ACCUMULATOR_ROOT_OBJECT_ID,
                        "Settlement shared input must be the accumulator root"
                    );
                    assert_invariant!(
                        root_argument.is_none(),
                        "Settlement must have only one accumulator root input"
                    );
                    root_argument = Some(Argument::Input(checked_as!(index, u16)?));
                }
                _ => invariant_violation!("Unexpected settlement input: {input:?}"),
            }
        }
        let Some(root_argument) = root_argument else {
            invariant_violation!("Settlement must have an accumulator root input");
        };
        let Some(root) = state.read_object(&SUI_ACCUMULATOR_ROOT_OBJECT_ID).copied() else {
            invariant_violation!("Settlement accumulator root is missing");
        };
        Ok(Self {
            bump,
            state,
            protocol_config,
            tx_context,
            inputs,
            root_argument,
            root,
            children: BTreeMap::new_in(bump),
            generated_ids: BTreeSet::new_in(bump),
            input_sui: 0,
            output_sui: 0,
        })
    }

    fn read_dynamic_field<K: Serialize + DeserializeOwned, V: DeserializeOwned>(
        &mut self,
        key: &K,
        key_type: &MoveTypeTag,
        value_type: &MoveTypeTag,
    ) -> Result<(ObjectId, Option<V>), ExecutionError<'a>> {
        let bytes = bcs::to_bytes(key)
            .map_err(|e| make_invariant_violation!("Cannot serialize settlement key: {e}"))?;
        let id = derive_dynamic_field_id(root_id(), key_type, &bytes)
            .map_err(|e| make_invariant_violation!("Cannot derive settlement field ID: {e}"))?;
        let id = ObjectId(id.into_bytes());
        let child = match self.children.get(&id) {
            Some(child) => *child,
            None => {
                let original = self
                    .state
                    .read_child_object(&SUI_ACCUMULATOR_ROOT_OBJECT_ID, &id, self.root.version())
                    .map_err(|e| {
                        make_invariant_violation!("Cannot read settlement field {id:?}: {e:?}")
                    })?;
                let child = ChildObject {
                    original,
                    current: original,
                };
                self.children.insert(id, child);
                child
            }
        };
        let Some(object) = child.current else {
            return Ok((id, None));
        };
        assert_invariant!(
            object.id() == id,
            "Invalid settlement field ID: expected {id:?}, got {:?}",
            object.id()
        );
        assert_invariant!(
            *object.owner() == Owner::ObjectOwner(&ROOT_ADDRESS),
            "Invalid settlement field owner for {id:?}: {:?}",
            object.owner()
        );
        let Some(object) = object.try_as_move() else {
            invariant_violation!("Settlement field {id:?} must be a Move object");
        };
        assert_invariant!(
            object.type_ == field_type(self.bump, key_type.clone(), value_type.clone()),
            "Settlement field {id:?} has an unexpected type"
        );
        let field: Field<K, V> = bcs::from_bytes(object.contents).map_err(|e| {
            make_invariant_violation!("Cannot deserialize settlement field {id:?}: {e}")
        })?;
        Ok((id, Some(field.value)))
    }

    fn write_dynamic_field<K: Serialize, V: Serialize>(
        &mut self,
        id: ObjectId,
        key: K,
        key_type: MoveTypeTag,
        value: V,
        value_type: MoveTypeTag,
    ) -> Result<(), ExecutionError<'a>> {
        let Some(child) = self.children.get_mut(&id) else {
            invariant_violation!("Settlement field {id:?} was not loaded");
        };
        if child.current.is_none() {
            self.generated_ids.insert(id);
        }
        let contents = bcs::to_bytes(&Field {
            id: UID::new(sui_types::base_types::ObjectID::new(id.0)),
            name: key,
            value,
        })
        .map_err(|e| make_invariant_violation!("Cannot serialize settlement field {id:?}: {e}"))?;
        let contents = containers::alloc_slice_copy(self.bump, &contents);
        let type_ = field_type(self.bump, key_type, value_type);
        // `MoveObject::new_from_execution` with `system_mutation`: unbounded once allowed.
        if !self.protocol_config.allow_unbounded_system_objects()
            && contents.len() as u64 > self.protocol_config.max_move_object_size()
        {
            return Err(ExecutionError::from_kind(
                crate::error::ExecutionErrorKind::MoveObjectTooBig {
                    object_size: contents.len() as u64,
                    max_object_size: self.protocol_config.max_move_object_size(),
                },
            ));
        }
        // Field has key but not store, regardless of its type arguments.
        let object = MoveObject {
            type_,
            has_public_transfer: false,
            version: child
                .original
                .as_ref()
                .map_or(SequenceNumber::MIN, |object| object.version()),
            contents,
        };
        child.current = Some(Object::new_move(
            object,
            Owner::ObjectOwner(&ROOT_ADDRESS),
            self.tx_context.digest(),
        ));
        Ok(())
    }

    fn finish(self) -> (ExecutionResultsV2<'a>, BTreeSet<'a, ObjectId>) {
        let Self {
            bump,
            root,
            children,
            generated_ids,
            input_sui: settlement_input_sui,
            output_sui: settlement_output_sui,
            state: _,
            protocol_config: _,
            tx_context: _,
            inputs: _,
            root_argument: _,
        } = self;
        let mut results = ExecutionResultsV2::new_in(bump);
        results.settlement_input_sui = settlement_input_sui;
        results.settlement_output_sui = settlement_output_sui;
        let root_id = root.id();
        results.written_objects.insert(root_id, root);
        results.modified_objects.insert(root_id);
        for (id, ChildObject { original, current }) in children {
            match (original, current) {
                (None, None) => (),
                (None, Some(object)) => {
                    results.created_object_ids.insert(id);
                    results.written_objects.insert(id, object);
                }
                (Some(_), None) => {
                    results.modified_objects.insert(id);
                    results.deleted_object_ids.insert(id);
                }
                (Some(original), Some(object)) => {
                    // Match Move's child-object change detection:
                    // unchanged values produce no write.
                    // Transaction metadata and storage rebates are finalized later.
                    if original.data() != object.data() || original.owner() != object.owner() {
                        results.modified_objects.insert(id);
                        results.written_objects.insert(id, object);
                    }
                }
            }
        }
        (results, generated_ids)
    }
}

/// `Field<key_type, value_type>`, as an object's type.
fn field_type<'a>(
    bump: &'a Bump,
    key_type: MoveTypeTag,
    value_type: MoveTypeTag,
) -> MoveObjectType<'a> {
    move_object_type_in(
        bump,
        &DynamicFieldInfo::dynamic_field_type(key_type, value_type),
    )
}

/// Executes accumulator settlement calls directly in Rust.
/// Type arguments come from accumulator events and already contain defining IDs. Settlement
/// only uses those types as phantom dynamic-field keys, so it needs neither code nor layouts.
pub fn execute<'a>(
    bump: &'a Bump,
    protocol_config: &ProtocolConfig,
    state: &mut dyn ExecutionState<'a>,
    tx_context: &TxContext,
    transaction: ProgrammableTransaction<'a>,
) -> Result<(), ExecutionError<'a>> {
    let ProgrammableTransaction { inputs, commands } = transaction;
    let Some(last_command_index) = commands.len().checked_sub(1) else {
        invariant_violation!("Settlement must contain a prologue");
    };
    let (execution_results, generated_ids) = {
        let mut context = Context::new(bump, protocol_config, state, tx_context, inputs)?;
        let command_result = commands.iter().enumerate().try_for_each(|(index, cmd)| {
            command(&mut context, index, last_command_index, cmd)
                .map_err(|e| e.with_command_index(index))
        });
        let mut loaded = BTreeMap::new_in(bump);
        for (id, child) in &context.children {
            if let Some(object) = &child.original {
                loaded.insert(
                    *id,
                    DynamicallyLoadedObjectMetadata {
                        version: object.version(),
                        digest: object.digest(),
                        owner: *object.owner(),
                        storage_rebate: object.storage_rebate(),
                        previous_transaction: object.previous_transaction(),
                    },
                );
            }
        }
        let final_result = context.finish();
        // Settlement must succeed. If an invariant or storage error is returned, retain
        // the reads for replay and leave the writes unapplied.
        state.save_loaded_runtime_objects(loaded);
        command_result?;
        final_result
    };
    state.record_execution_results(execution_results)?;
    state.record_generated_object_ids(generated_ids);
    Ok(())
}

fn command<'a>(
    context: &mut Context<'a, '_>,
    index: usize,
    last_command_index: usize,
    command: &Command<'a>,
) -> Result<(), ExecutionError<'a>> {
    let Command::MoveCall(call) = command else {
        invariant_violation!("Settlement commands must be Move calls");
    };
    let ProgrammableMoveCall {
        package,
        module,
        function,
        type_arguments,
        arguments,
    } = *call;
    assert_invariant!(
        *package == ObjectId(SUI_FRAMEWORK_ADDRESS.0),
        "Unexpected settlement package: {package:?}"
    );
    match (module, function) {
        (ACCUMULATOR_SETTLEMENT_MODULE, SETTLEMENT_PROLOGUE) => {
            assert_invariant!(index == 0, "Settlement prologue must be the first command");
            let [] = type_arguments else {
                invariant_violation!("Settlement prologue expects no type arguments");
            };
            let [root, epoch, height, offset, input, output] = arguments else {
                invariant_violation!("Settlement prologue expects six arguments");
            };
            assert_invariant!(
                *root == context.root_argument,
                "Settlement call has an incorrect root argument"
            );
            // These values distinguish transaction digests. Move also ignores their values,
            // but execution still validates their encoding as u64 arguments.
            let _: u64 = pure(context.inputs, epoch)?;
            let _: u64 = pure(context.inputs, height)?;
            let _: u64 = pure(context.inputs, offset)?;
            context.input_sui = pure::<u64>(context.inputs, input)?;
            context.output_sui = pure::<u64>(context.inputs, output)?;
            Ok(())
        }
        (ACCUMULATOR_SETTLEMENT_MODULE, SETTLE_U128) => {
            assert_invariant!(index > 0, "Balance settlement must follow the prologue");
            let [ty] = type_arguments else {
                invariant_violation!("Balance settlement expects one type argument");
            };
            let [root, owner, merge, split] = arguments else {
                invariant_violation!("Balance settlement expects four arguments");
            };
            if let Err(e) = check_type_input(ty) {
                invariant_violation!("Invalid settlement type: {e}");
            }
            let ty = to_move_type_tag(ty);
            assert_invariant!(
                *root == context.root_argument,
                "Settlement call has an incorrect root argument"
            );
            let owner: sui_types::base_types::SuiAddress = pure(context.inputs, owner)?;
            let merge: u128 = pure(context.inputs, merge)?;
            let split: u128 = pure(context.inputs, split)?;
            settle_u128(context, ty, owner, merge, split)
        }
        (ACCUMULATOR_SETTLEMENT_MODULE, SETTLE_EVENTS) => {
            assert_invariant!(index > 0, "Event settlement must follow the prologue");
            let [] = type_arguments else {
                invariant_violation!("Event settlement expects no type arguments");
            };
            let [root, stream, new_root, count, checkpoint] = arguments else {
                invariant_violation!("Event settlement expects five arguments");
            };
            assert_invariant!(
                *root == context.root_argument,
                "Settlement call has an incorrect root argument"
            );
            let stream: sui_types::base_types::SuiAddress = pure(context.inputs, stream)?;
            let new_root: U256 = pure(context.inputs, new_root)?;
            let count: u64 = pure(context.inputs, count)?;
            let checkpoint: u64 = pure(context.inputs, checkpoint)?;
            settle_events(context, stream, new_root, count, checkpoint)
        }
        (ACCUMULATOR_METADATA_MODULE, RECORD_ACCUMULATOR_OBJECT_CHANGES) => {
            assert_invariant!(index > 0, "Object-count update must follow the prologue");
            assert_invariant!(
                index == last_command_index,
                "Object-count update must be the last command"
            );
            let [] = type_arguments else {
                invariant_violation!("Object-count update expects no type arguments");
            };
            let [root, created, destroyed] = arguments else {
                invariant_violation!("Object-count update expects three arguments");
            };
            assert_invariant!(
                *root == context.root_argument,
                "Settlement call has an incorrect root argument"
            );
            let created: u64 = pure(context.inputs, created)?;
            let destroyed: u64 = pure(context.inputs, destroyed)?;
            record_object_changes(context, created, destroyed)
        }
        _ => invariant_violation!("Unexpected settlement call: {module}::{function}"),
    }
}

fn pure<'a, T: DeserializeOwned>(
    inputs: &[CallArg<'_>],
    argument: &Argument,
) -> Result<T, ExecutionError<'a>> {
    let Argument::Input(index) = argument else {
        invariant_violation!("Settlement arguments must refer to inputs: {argument:?}");
    };
    let Some(input) = inputs.get(usize::from(*index)) else {
        invariant_violation!("Settlement input index out of bounds: {index}")
    };
    let CallArg::Pure(bytes) = input else {
        invariant_violation!("Settlement expected a pure argument: {argument:?}");
    };
    bcs::from_bytes(bytes)
        .map_err(|e| make_invariant_violation!("Invalid settlement argument: {e}"))
}

fn settle_u128<'a>(
    context: &mut Context<'a, '_>,
    ty: MoveTypeTag,
    owner: sui_types::base_types::SuiAddress,
    merge: u128,
    split: u128,
) -> Result<(), ExecutionError<'a>> {
    let key = AccumulatorKey { owner };
    let key_type = AccumulatorKey::get_type_tag(&[ty]);
    let (id, previous) =
        context.read_dynamic_field::<_, U128>(&key, &key_type, &U128::get_type_tag())?;
    assert_invariant!(
        previous.is_some() || split == 0,
        "Cannot withdraw from a missing accumulator"
    );
    let previous = previous.map_or(0, |v| v.value);
    let value = match (merge, split) {
        (0, 0) => invariant_violation!("Settlement must merge or split a nonzero amount"),
        (merge, 0) => previous.checked_add(merge),
        (0, split) => previous.checked_sub(split),
        (_, _) => invariant_violation!("Settlement cannot both merge and split"),
    };
    let Some(value) = value else {
        invariant_violation!("Settlement balance overflow or underflow");
    };
    if value == 0 {
        let Some(child) = context.children.get_mut(&id) else {
            invariant_violation!("Settlement field was not loaded");
        };
        child.current = None;
        Ok(())
    } else {
        context.write_dynamic_field(id, key, key_type, U128 { value }, U128::get_type_tag())
    }
}

fn settle_events<'a>(
    context: &mut Context<'a, '_>,
    stream: sui_types::base_types::SuiAddress,
    new_root: U256,
    count: u64,
    checkpoint: u64,
) -> Result<(), ExecutionError<'a>> {
    let key = AccumulatorKey { owner: stream };
    let value_type = EventStreamHead::get_type_tag();
    let key_type = AccumulatorKey::get_type_tag(std::slice::from_ref(&value_type));
    let (id, previous) =
        context.read_dynamic_field::<_, EventStreamHead>(&key, &key_type, &value_type)?;
    let mut head = previous.unwrap_or_default();
    add_to_mmr(new_root, &mut head.mmr);
    head.num_events = head
        .num_events
        .checked_add(count)
        .ok_or_else(|| make_invariant_violation!("Settlement event count overflow"))?;
    head.checkpoint_seq = checkpoint;
    context.write_dynamic_field(id, key, key_type, head, value_type)
}

fn add_to_mmr(new_root: U256, mmr: &mut std::vec::Vec<U256>) {
    let mut current = new_root;
    for peak in mmr.iter_mut() {
        if *peak == U256::zero() {
            *peak = current;
            return;
        }
        current = hash_two_to_one_u256(*peak, current);
        *peak = U256::zero();
    }
    mmr.push(current);
}

fn hash_two_to_one_u256(left: U256, right: U256) -> U256 {
    let mut hash = Blake2b256::default();
    hash.update(left.to_le_bytes());
    hash.update(right.to_le_bytes());
    U256::from_le_bytes(&hash.finalize().digest)
}

fn record_object_changes<'a>(
    context: &mut Context<'a, '_>,
    created: u64,
    destroyed: u64,
) -> Result<(), ExecutionError<'a>> {
    let key = AccumulatorObjectCountKey::default();
    let key_type = AccumulatorObjectCountKey::get_type_tag();
    let (id, previous) =
        context.read_dynamic_field::<_, u64>(&key, &key_type, &MoveTypeTag::U64)?;
    let count = previous
        .unwrap_or(0)
        .checked_add(created)
        .and_then(|v| v.checked_sub(destroyed))
        .ok_or_else(|| {
            make_invariant_violation!("Settlement object count overflow or underflow")
        })?;
    context.write_dynamic_field(id, key, key_type, count, MoveTypeTag::U64)
}

/// These framework layouts contain no SUI. Their type arguments are phantom, so conservation
/// checks do not need to resolve user packages just to inspect settlement outputs.
pub(crate) fn has_no_sui(object: &Object<'_>) -> bool {
    static EVENT_STREAM_FIELD_TYPE: LazyLock<MoveStructTag> = LazyLock::new(|| {
        let event = EventStreamHead::get_type_tag();
        DynamicFieldInfo::dynamic_field_type(
            AccumulatorKey::get_type_tag(std::slice::from_ref(&event)),
            event,
        )
    });
    static OBJECT_COUNT_FIELD_TYPE: LazyLock<MoveStructTag> = LazyLock::new(|| {
        DynamicFieldInfo::dynamic_field_type(
            AccumulatorObjectCountKey::get_type_tag(),
            MoveTypeTag::U64,
        )
    });

    let Some(move_object) = object.try_as_move() else {
        return false;
    };
    let tag = match &move_object.type_ {
        MoveObjectType::BalanceAccumulatorField(_) => return true,
        MoveObjectType::SuiBalanceAccumulatorField => return false,
        MoveObjectType::Other(tag) => tag,
        _ => return false,
    };
    if *tag.address == SUI_FRAMEWORK_ADDRESS
        && tag.module == "accumulator"
        && tag.name == "AccumulatorRoot"
        && tag.type_params.is_empty()
    {
        return true;
    }
    let tag = to_move_struct_tag(tag);
    tag == *EVENT_STREAM_FIELD_TYPE || tag == *OBJECT_COUNT_FIELD_TYPE
}
