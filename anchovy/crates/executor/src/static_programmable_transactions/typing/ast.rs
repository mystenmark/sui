// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    gas_charger::GasPayment,
    static_programmable_transactions::{
        linkage::resolved_linkage::{ExecutableLinkage, ResolvedLinkage},
        loading::ast::{self as L, PackagePayload},
        spanned::Spanned,
    },
};
use containers::{Box, IndexMap, IndexSet, Vec};
use exec_types::base::ObjectRef;
use messages::base::ObjectId;
use move_core_types::u256::U256;
use move_vm_runtime::execution::values::VectorSpecialization;
use std::cell::OnceCell;

//**************************************************************************************************
// AST Nodes
//**************************************************************************************************

#[derive(Debug)]
pub struct Transaction<'a> {
    pub gas_payment: Option<GasPayment>,
    /// Gathered BCS bytes from Pure inputs, borrowed from the transaction
    pub bytes: IndexSet<'a, &'a [u8]>,
    // All input objects
    pub objects: Vec<'a, ObjectInput<'a>>,
    /// All Withdrawal inputs
    pub withdrawals: Vec<'a, WithdrawalInput<'a>>,
    /// All pure inputs
    pub pure: Vec<'a, PureInput<'a>>,
    /// All receiving inputs
    pub receiving: Vec<'a, ReceivingInput<'a>>,
    pub withdrawal_compatibility_conversions:
        IndexMap<'a, Location, WithdrawalCompatibilityConversion>,
    /// Original number of commands in the transaction. All Spanned indices in the AST should be
    /// < `original_command_len`
    pub original_command_len: usize,
    pub commands: Commands<'a>,
    pub unified_linkage: Option<ExecutableLinkage<'a>>,
}

/// The original index into the `input` vector of the transaction, before the inputs were split
/// into their respective categories (objects, pure, or receiving).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InputIndex(pub u16);

#[derive(Debug)]
pub struct ObjectInput<'a> {
    pub original_input_index: InputIndex,
    pub arg: ObjectArg,
    pub ty: Type<'a>,
}

pub type ByteIndex = usize;

#[derive(Debug)]
pub struct PureInput<'a> {
    pub original_input_index: InputIndex,
    // A index into `byte` table of BCS bytes
    pub byte_index: ByteIndex,
    // the type that the BCS bytes will be deserialized into
    pub ty: Type<'a>,
    // Information about where this constraint came from
    pub constraint: BytesConstraint,
}

#[derive(Debug)]
pub struct ReceivingInput<'a> {
    pub original_input_index: InputIndex,
    pub object_ref: ObjectRef,
    pub ty: Type<'a>,
    // Information about where this constraint came from
    pub constraint: BytesConstraint,
}

#[derive(Debug)]
pub struct WithdrawalInput<'a> {
    pub original_input_index: InputIndex,
    /// The full type.
    /// Either `sui::funds_accumulator::Withdrawal<T>` for a direct source, or
    /// `sui::allowance::AllowanceWithdrawal<T>` for an allowance source
    pub ty: Type<'a>,
    pub source: WithdrawalSource,
    /// This amount is verified to be <= the max for the type described by the `T` in `ty`
    pub amount: U256,
}

#[derive(Debug, Clone, Copy)]
pub struct WithdrawalCompatibilityConversion {
    // The pure input location of the owner address
    pub owner: Location,
    // Result index to conversion call to `sui::coin::redeem_funds`
    pub conversion_result: u16,
}

pub type Commands<'a> = Vec<'a, Command<'a>>;

pub type ObjectArg = L::ObjectArg;

pub type Type<'a> = L::Type<'a>;

pub type WithdrawalSource = L::WithdrawalSource;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Information for a given constraint for input bytes
pub struct BytesConstraint {
    /// The command that first added this constraint
    pub command: u16,
    /// The argument in that command
    pub argument: u16,
}

pub type ResultType<'a> = Vec<'a, Type<'a>>;

pub type Command<'a> = Spanned<Command_<'a>>;

#[derive(Debug)]
pub struct Command_<'a> {
    /// The command
    pub command: Command__<'a>,
    /// The type of the return values of the command
    pub result_type: ResultType<'a>,
    /// Markers to drop unused results from the command. These are inferred based on any usage
    /// of the given result `Result(i,j)` after this command. This is leveraged by the borrow
    /// checker to remove unused references to allow potentially reuse of parent references.
    /// The value at result `j` is unused and can be dropped if `drop_value[j]` is true.
    pub drop_values: Vec<'a, /* drop value */ bool>,
    /// Marks if the command consumes by value either a legacy shared object, or a party object with
    /// post-execution checks. A party object has post-execution checks if it is used with mutable
    /// usage and is missing one of the mutable permissions.
    pub incurs_post_execution_checks: bool,
}

#[derive(Debug)]
pub enum Command__<'a> {
    MoveCall(Box<'a, MoveCall<'a>>),
    TransferObjects(Vec<'a, Argument<'a>>, Argument<'a>),
    SplitCoins(
        /* Coin<T> */ Type<'a>,
        Argument<'a>,
        Vec<'a, Argument<'a>>,
    ),
    MergeCoins(
        /* Coin<T> */ Type<'a>,
        Argument<'a>,
        Vec<'a, Argument<'a>>,
    ),
    MakeMoveVec(/* T for vector<T> */ Type<'a>, Vec<'a, Argument<'a>>),
    Publish(PackagePayload<'a>, Vec<'a, ObjectId>, ResolvedLinkage<'a>),
    Upgrade(
        PackagePayload<'a>,
        Vec<'a, ObjectId>,
        ObjectId,
        Argument<'a>,
        ResolvedLinkage<'a>,
    ),
}

pub type LoadedFunctionInstantiation<'a> = L::LoadedFunctionInstantiation<'a>;

pub type LoadedFunction<'a> = L::LoadedFunction<'a>;

#[derive(Debug)]
pub struct MoveCall<'a> {
    pub function: LoadedFunction<'a>,
    pub arguments: Vec<'a, Argument<'a>>,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Location {
    TxContext,
    GasCoin,
    ObjectInput(u16),
    WithdrawalInput(u16),
    PureInput(u16),
    ReceivingInput(u16),
    Result(u16, u16),
}

// Non borrowing usage of locations, moving or copying
#[derive(Clone, Debug)]
pub enum Usage {
    Move(Location),
    Copy {
        location: Location,
        /// Was this location borrowed at the time of copying?
        /// Initially empty and populated by `memory_safety`
        borrowed: OnceCell<bool>,
    },
}

pub type Argument<'a> = Spanned<Argument_<'a>>;
pub type Argument_<'a> = (Argument__, Type<'a>);

#[derive(Clone, Debug)]
pub enum Argument__ {
    /// Move or copy a value
    Use(Usage),
    /// Borrow a value, i.e. `&x` or `&mut x`
    Borrow(/* mut */ bool, Location),
    /// Read a value from a reference, i.e. `*&x`
    Read(Usage),
    /// Freeze a mutable reference, making an `&t` from `&mut t`
    Freeze(Usage),
}

//**************************************************************************************************
// impl
//**************************************************************************************************

impl<'a> Transaction<'a> {
    pub fn types(&self) -> impl Iterator<Item = &Type<'a>> {
        let pure_types = self.pure.iter().map(|p| &p.ty);
        let object_types = self.objects.iter().map(|o| &o.ty);
        let receiving_types = self.receiving.iter().map(|r| &r.ty);
        let command_types = self.commands.iter().flat_map(command_types);
        pure_types
            .chain(object_types)
            .chain(receiving_types)
            .chain(command_types)
    }
}

impl Usage {
    pub fn new_move(location: Location) -> Usage {
        Usage::Move(location)
    }

    pub fn new_copy(location: Location) -> Usage {
        Usage::Copy {
            location,
            borrowed: OnceCell::new(),
        }
    }

    pub fn location(&self) -> Location {
        match self {
            Usage::Move(location) => *location,
            Usage::Copy { location, .. } => *location,
        }
    }
}

impl Argument__ {
    pub fn new_move(location: Location) -> Self {
        Self::Use(Usage::new_move(location))
    }

    pub fn new_copy(location: Location) -> Self {
        Self::Use(Usage::new_copy(location))
    }

    pub fn location(&self) -> Location {
        match self {
            Self::Use(usage) | Self::Read(usage) => usage.location(),
            Self::Borrow(_, location) => *location,
            Self::Freeze(usage) => usage.location(),
        }
    }
}

impl<'a> Command__<'a> {
    pub fn arguments(&self) -> std::boxed::Box<dyn Iterator<Item = &Argument<'a>> + '_> {
        match self {
            Command__::MoveCall(mc) => std::boxed::Box::new(mc.arguments.iter()),
            Command__::TransferObjects(objs, addr) => {
                std::boxed::Box::new(objs.iter().chain(std::iter::once(addr)))
            }
            Command__::SplitCoins(_, coin, amounts) => {
                std::boxed::Box::new(std::iter::once(coin).chain(amounts))
            }
            Command__::MergeCoins(_, target, sources) => {
                std::boxed::Box::new(std::iter::once(target).chain(sources))
            }
            Command__::MakeMoveVec(_, elems) => std::boxed::Box::new(elems.iter()),
            Command__::Publish(_, _, _) => std::boxed::Box::new(std::iter::empty()),
            Command__::Upgrade(_, _, _, arg, _) => std::boxed::Box::new(std::iter::once(arg)),
        }
    }

    pub fn types(&self) -> std::boxed::Box<dyn Iterator<Item = &Type<'a>> + '_> {
        match self {
            Command__::TransferObjects(args, arg) => {
                std::boxed::Box::new(std::iter::once(arg).chain(args.iter()).map(argument_type))
            }
            Command__::SplitCoins(ty, arg, args) | Command__::MergeCoins(ty, arg, args) => {
                std::boxed::Box::new(
                    std::iter::once(arg)
                        .chain(args.iter())
                        .map(argument_type)
                        .chain(std::iter::once(ty)),
                )
            }
            Command__::MakeMoveVec(ty, args) => {
                std::boxed::Box::new(args.iter().map(argument_type).chain(std::iter::once(ty)))
            }
            Command__::MoveCall(call) => std::boxed::Box::new(
                call.arguments
                    .iter()
                    .map(argument_type)
                    .chain(call.function.type_arguments.iter())
                    .chain(call.function.signature.parameters.iter())
                    .chain(call.function.signature.return_.iter()),
            ),
            Command__::Upgrade(_, _, _, arg, _) => {
                std::boxed::Box::new(std::iter::once(arg).map(argument_type))
            }
            Command__::Publish(_, _, _) => std::boxed::Box::new(std::iter::empty()),
        }
    }

    pub fn arguments_len(&self) -> usize {
        let n = match self {
            Command__::MoveCall(mc) => mc.arguments.len(),
            Command__::TransferObjects(objs, _) => objs.len().saturating_add(1),
            Command__::SplitCoins(_, _, amounts) => amounts.len().saturating_add(1),
            Command__::MergeCoins(_, _, sources) => sources.len().saturating_add(1),
            Command__::MakeMoveVec(_, elems) => elems.len(),
            Command__::Publish(_, _, _) => 0,
            Command__::Upgrade(_, _, _, _, _) => 1,
        };
        debug_assert_eq!(self.arguments().count(), n);
        n
    }
}

//**************************************************************************************************
// Standalone functions
//**************************************************************************************************

pub fn command_types<'b, 'a>(cmd: &'b Command<'a>) -> impl Iterator<Item = &'b Type<'a>> {
    let result_types = cmd.value.result_type.iter();
    let command_types = cmd.value.command.types();
    result_types.chain(command_types)
}

pub fn argument_type<'b, 'a>(arg: &'b Argument<'a>) -> &'b Type<'a> {
    &arg.value.1
}

//**************************************************************************************************
// traits
//**************************************************************************************************

impl TryFrom<Type<'_>> for VectorSpecialization {
    type Error = &'static str;

    fn try_from(value: Type<'_>) -> Result<Self, Self::Error> {
        Ok(match value {
            Type::U8 => VectorSpecialization::U8,
            Type::U16 => VectorSpecialization::U16,
            Type::U32 => VectorSpecialization::U32,
            Type::U64 => VectorSpecialization::U64,
            Type::U128 => VectorSpecialization::U128,
            Type::U256 => VectorSpecialization::U256,
            Type::Address => VectorSpecialization::Address,
            Type::Bool => VectorSpecialization::Bool,
            Type::Signer | Type::Vector(_) | Type::Datatype(_) => VectorSpecialization::Container,
            Type::Reference(_, _) => return Err("unexpected reference in vector specialization"),
        })
    }
}
