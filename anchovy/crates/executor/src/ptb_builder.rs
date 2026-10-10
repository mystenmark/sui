// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `sui_types::programmable_transaction_builder`, building `messages` views in the arena: the
//! system transactions execution runs as PTBs.

use containers::{Bump, IndexMap, Vec};
use messages::base::{ObjectId, SequenceNumber};
use messages::transaction::{
    Argument, CallArg, Command, ObjectArg, ProgrammableMoveCall, ProgrammableTransaction,
    SharedObjectArg, SharedObjectMutability,
};
use messages::type_tag::TypeInput;
use serde::Serialize;

#[derive(PartialEq, Eq, Hash)]
enum BuilderArg<'a> {
    Object(ObjectId),
    Pure(&'a [u8]),
    ForcedNonUniquePure(usize),
}

/// `ObjectArg::SUI_SYSTEM_MUT`.
pub const SUI_SYSTEM_MUT: SharedObjectArg = SharedObjectArg::new(
    exec_types::base::SUI_SYSTEM_STATE_OBJECT_ID,
    exec_types::object::OBJECT_START_VERSION,
    SharedObjectMutability::Mutable,
);

/// `ObjectArg::CLOCK_MUT`.
pub const CLOCK_MUT: SharedObjectArg = SharedObjectArg::new(
    exec_types::base::SUI_CLOCK_OBJECT_ID,
    exec_types::object::OBJECT_START_VERSION,
    SharedObjectMutability::Mutable,
);

pub struct ProgrammableTransactionBuilder<'a> {
    bump: &'a Bump,
    inputs: IndexMap<'a, BuilderArg<'a>, CallArg<'a>>,
    commands: Vec<'a, Command<'a>>,
}

impl<'a> ProgrammableTransactionBuilder<'a> {
    pub fn new(bump: &'a Bump) -> Self {
        Self {
            bump,
            inputs: IndexMap::new_in(bump),
            commands: Vec::new_in(bump),
        }
    }

    pub fn finish(self) -> ProgrammableTransaction<'a> {
        let Self {
            bump,
            inputs,
            commands,
        } = self;
        let mut input_args = Vec::with_capacity_in(inputs.len(), bump);
        input_args.extend(inputs.into_iter().map(|(_, arg)| arg));
        ProgrammableTransaction {
            inputs: input_args.leak(),
            commands: commands.leak(),
        }
    }

    pub fn pure_bytes(&mut self, bytes: &'a [u8], force_separate: bool) -> Argument {
        let arg = if force_separate {
            BuilderArg::ForcedNonUniquePure(self.inputs.len())
        } else {
            BuilderArg::Pure(bytes)
        };
        let (i, _) = self.inputs.insert_full(arg, CallArg::Pure(bytes));
        Argument::Input(i as u16)
    }

    /// The value's BCS, in the arena, as a pure input.
    pub fn pure<T: Serialize + ?Sized>(&mut self, value: &T) -> Argument {
        let bytes = self.bcs(value);
        self.pure_bytes(bytes, /* force separate */ false)
    }

    /// The value's BCS, in the arena.
    pub fn bcs<T: Serialize + ?Sized>(&self, value: &T) -> &'a [u8] {
        let bytes = bcs::to_bytes(value).expect("a pure argument serializes");
        containers::alloc_slice_copy(self.bump, &bytes)
    }

    /// # Errors
    /// If the object is already an input of an incompatible kind, as the reference does.
    pub fn obj(&mut self, obj_arg: ObjectArg<'a>) -> anyhow::Result<Argument> {
        let id = object_arg_id(&obj_arg);
        let obj_arg = if let Some(old_value) = self.inputs.get(&BuilderArg::Object(id)) {
            let old_obj_arg = match old_value {
                CallArg::Pure(_) => anyhow::bail!("invariant violation! object has pure argument"),
                CallArg::Object(arg) => arg,
                CallArg::FundsWithdrawal(_) => {
                    anyhow::bail!("invariant violation! object has balance withdraw argument")
                }
            };
            match (old_obj_arg, obj_arg) {
                (ObjectArg::SharedObject(old), ObjectArg::SharedObject(new))
                    if old.initial_shared_version == new.initial_shared_version =>
                {
                    anyhow::ensure!(
                        old.id == new.id && id == new.id,
                        "invariant violation! object has id does not match call arg"
                    );
                    let mutability = if old.mutability() == SharedObjectMutability::Mutable
                        || new.mutability() == SharedObjectMutability::Mutable
                    {
                        SharedObjectMutability::Mutable
                    } else {
                        new.mutability()
                    };
                    ObjectArg::SharedObject(containers::alloc(
                        self.bump,
                        SharedObjectArg::new(id, new.initial_shared_version.get(), mutability),
                    ))
                }
                (old_obj_arg, obj_arg) => {
                    anyhow::ensure!(
                        *old_obj_arg == obj_arg,
                        "Mismatched Object argument kind for object {id}. \
                        {old_value:?} is not compatible with {obj_arg:?}"
                    );
                    obj_arg
                }
            }
        } else {
            obj_arg
        };
        let (i, _) = self
            .inputs
            .insert_full(BuilderArg::Object(id), CallArg::Object(obj_arg));
        Ok(Argument::Input(i as u16))
    }

    /// A shared object input.
    pub fn shared_obj(
        &mut self,
        id: ObjectId,
        initial_shared_version: SequenceNumber,
        mutability: SharedObjectMutability,
    ) -> anyhow::Result<Argument> {
        let arg = containers::alloc(
            self.bump,
            SharedObjectArg::new(id, initial_shared_version, mutability),
        );
        self.obj(ObjectArg::SharedObject(arg))
    }

    /// # Errors
    /// As `obj`. A funds withdrawal is not an input the system transactions make.
    pub fn input(&mut self, call_arg: CallArg<'a>) -> anyhow::Result<Argument> {
        match call_arg {
            CallArg::Pure(bytes) => Ok(self.pure_bytes(bytes, /* force separate */ false)),
            CallArg::Object(obj) => self.obj(obj),
            CallArg::FundsWithdrawal(_) => {
                anyhow::bail!("system transactions do not withdraw funds")
            }
        }
    }

    pub fn command(&mut self, command: Command<'a>) -> Argument {
        let i = self.commands.len();
        self.commands.push(command);
        Argument::Result(i as u16)
    }

    /// Will fail to generate if given an empty ObjVec
    pub fn move_call(
        &mut self,
        package: ObjectId,
        module: &'a str,
        function: &'a str,
        type_arguments: &'a [TypeInput<'a>],
        call_args: &[CallArg<'a>],
    ) -> anyhow::Result<()> {
        let mut arguments = Vec::with_capacity_in(call_args.len(), self.bump);
        for a in call_args {
            arguments.push(self.input(*a)?);
        }
        self.programmable_move_call(package, module, function, type_arguments, &arguments);
        Ok(())
    }

    pub fn programmable_move_call(
        &mut self,
        package: ObjectId,
        module: &'a str,
        function: &'a str,
        type_arguments: &'a [TypeInput<'a>],
        arguments: &[Argument],
    ) -> Argument {
        let call = ProgrammableMoveCall {
            package: containers::alloc(self.bump, package),
            module,
            function,
            type_arguments,
            arguments: containers::alloc_slice_copy(self.bump, arguments),
        };
        self.command(Command::MoveCall(call))
    }
}

fn object_arg_id(arg: &ObjectArg<'_>) -> ObjectId {
    match arg {
        ObjectArg::ImmOrOwnedObject(r) | ObjectArg::Receiving(r) => r.id,
        ObjectArg::SharedObject(s) => s.id,
    }
}
