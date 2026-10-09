// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The reference's loading AST in the transaction's arena. `Type` is `Copy` (arena references
//! where the reference has `Rc`s), names are `&'a str`, and pure inputs borrow the
//! transaction's bytes rather than move them.

use crate::{
    gas_charger::GasPayment,
    static_programmable_transactions::linkage::resolved_linkage::{
        ExecutableLinkage, ResolvedLinkage,
    },
};
use containers::{BTreeSet, Box, Bump, IndexSet, Vec};
use exec_types::base::ObjectRef;
use messages::base::{ObjectId, SequenceNumber};
use move_binary_format::{
    CompiledModule,
    file_format::{AbilitySet, CodeOffset, FunctionDefinitionIndex, Visibility},
};
use move_core_types::{
    account_address::AccountAddress,
    identifier::{IdentStr, Identifier},
    language_storage::{self as move_tags, StructTag, TypeTag},
    u256::U256,
};
use sui_types::{
    base_types::{RESOLVED_TX_CONTEXT, TxContextKind},
    object::ObjectPermissions,
};
use sui_verifier::INIT_FN_NAME;

//**************************************************************************************************
// AST Nodes
//**************************************************************************************************

#[derive(Debug)]
pub struct Transaction<'a> {
    pub gas_payment: Option<GasPayment>,
    pub inputs: Inputs<'a>,
    /// Original number of commands in the transaction. After typing, Spanned indices in the AST
    /// should be < `original_command_len`
    pub original_command_len: usize,
    pub commands: Commands<'a>,
    pub unified_linkage: Option<ExecutableLinkage<'a>>,
}

pub type Inputs<'a> = Vec<'a, (InputArg<'a>, InputType<'a>)>;

pub type Commands<'a> = Vec<'a, Command<'a>>;

#[derive(Debug, Clone, Copy)]
pub enum InputArg<'a> {
    Pure(&'a [u8]),
    Receiving(ObjectRef),
    Object(ObjectArg),
    FundsWithdrawal(FundsWithdrawalArg<'a>),
}

#[derive(Debug, Clone, Copy)]
pub enum ObjectArgKind {
    ImmObject(ObjectRef),
    OwnedObject(ObjectRef),
    ConsensusObject {
        id: ObjectId,
        initial_shared_version: SequenceNumber,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct ObjectArg {
    pub kind: ObjectArgKind,
    /// Permissions, potentially refined/limited based on the input argument. For example if a
    /// shared object is used but marked as read-only, the permissions would be refined to being
    /// _only_ immutable usage.
    pub refined_permissions: ObjectPermissions,
}

#[derive(Debug, Clone, Copy)]
pub struct FundsWithdrawalArg<'a> {
    // if true, it was from a compatibility object input, not a intentional withdrawal argument
    pub from_compatibility_object: bool,
    /// The full type.
    /// Either `sui::funds_accumulator::Withdrawal<T>` for a direct source, or
    /// `sui::allowance::AllowanceWithdrawal<T>` for an allowance source
    pub ty: Type<'a>,
    pub source: WithdrawalSource,
    /// This amount is verified to be <= the max for the type described by the `T` in `ty`
    pub amount: U256,
}

#[derive(Debug, Clone, Copy)]
pub enum WithdrawalSource {
    /// A `sui::funds_accumulator::Withdrawal` from the sender/sponsor
    Direct { owner: AccountAddress },
    /// An `sui::allowance::AllowanceWithdrawal` permissioned by the `Allowance` object `id`
    Allowance {
        funder: AccountAddress,
        id: ObjectId,
    },
}

impl WithdrawalSource {
    /// The account the withdrawal debits.
    pub fn source_account(&self) -> AccountAddress {
        match self {
            Self::Direct { owner } => *owner,
            Self::Allowance { funder, .. } => *funder,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Type<'a> {
    Bool,
    U8,
    U16,
    U32,
    U64,
    U128,
    U256,
    Address,
    Signer,
    Vector(&'a Vector<'a>),
    Datatype(&'a Datatype<'a>),
    Reference(/* is mut */ bool, &'a Type<'a>),
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Vector<'a> {
    pub abilities: AbilitySet,
    pub element_type: Type<'a>,
}

/// A module's address and name, as the reference's `ModuleId` with the name in the arena.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleId<'a> {
    pub address: AccountAddress,
    pub name: &'a str,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Datatype<'a> {
    pub abilities: AbilitySet,
    pub module: ModuleId<'a>,
    pub name: &'a str,
    pub type_arguments: &'a [Type<'a>],
}

#[derive(Debug, Clone, Copy)]
pub enum InputType<'a> {
    Bytes,
    Fixed(Type<'a>),
}

#[derive(Debug)]
pub enum Command<'a> {
    MoveCall(Box<'a, MoveCall<'a>>),
    TransferObjects(Vec<'a, Argument>, Argument),
    SplitCoins(Argument, Vec<'a, Argument>),
    MergeCoins(Argument, Vec<'a, Argument>),
    MakeMoveVec(
        /* T for vector<T> */ Option<Type<'a>>,
        Vec<'a, Argument>,
    ),
    Publish(PackagePayload<'a>, Vec<'a, ObjectId>, ResolvedLinkage<'a>),
    Upgrade(
        PackagePayload<'a>,
        Vec<'a, ObjectId>,
        ObjectId,
        Argument,
        ResolvedLinkage<'a>,
    ),
}

#[derive(Debug)]
pub enum PackagePayload<'a> {
    Serialized(&'a [&'a [u8]]),
    Deserialized(DeserializedPackage<'a>),
}

// A Deserialized but not yet verified package created as part of loading.
#[derive(Debug)]
pub struct DeserializedPackage<'a> {
    // NB: Modules are deserialized but not yet verified. They _are_ bounds checked though.
    pub deserialized_modules: Vec<'a, CompiledModule>,
    // Sum of the sizes of all modules in (serialized) bytes, used for metering
    pub total_bytes: usize,
    // The computed digest of the package --
    // `MovePackage::compute_digest_for_modules_and_deps` with `hash_modules` set to `true`.
    pub computed_digest: [u8; 32],
    // Names of the modules in this package that define a function named `init`.
    pub modules_with_init: BTreeSet<'a, &'a str>,
}

impl<'a> DeserializedPackage<'a> {
    pub fn new(
        bump: &'a Bump,
        deserialized_modules: Vec<'a, CompiledModule>,
        total_bytes: usize,
        computed_digest: [u8; 32],
    ) -> Self {
        let mut modules_with_init = BTreeSet::new_in(bump);
        modules_with_init.extend(
            deserialized_modules
                .iter()
                .filter(|module| module_has_init(module))
                .map(|module| {
                    containers::alloc_str(
                        bump,
                        module.identifier_at(module.self_handle().name).as_str(),
                    )
                }),
        );
        Self {
            deserialized_modules,
            total_bytes,
            computed_digest,
            modules_with_init,
        }
    }

    /// Returns true if this package defines any modules with function that could be a possible
    /// `init` function.
    pub fn has_potential_init(&self) -> bool {
        !self.modules_with_init.is_empty()
    }
}

/// Whether `module` defines a function named `init`.
///
/// NB: we presuppose that a function named `init` is the module's initializer. If it does not
/// conform to the `init` signature requirements the entry points verifier rejects the publish
/// later, failing the transaction as a whole.
pub(crate) fn module_has_init(module: &CompiledModule) -> bool {
    module.function_defs().iter().any(|func_def| {
        let handle = module.function_handle_at(func_def.function);
        module.identifier_at(handle.name) == INIT_FN_NAME
    })
}

#[derive(Debug)]
pub struct LoadedFunctionInstantiation<'a> {
    pub parameters: Vec<'a, Type<'a>>,
    pub return_: Vec<'a, Type<'a>>,
}

#[derive(Debug)]
pub struct LoadedFunction<'a> {
    pub version_mid: ModuleId<'a>,
    pub original_mid: ModuleId<'a>,
    pub name: &'a str,
    pub type_arguments: Vec<'a, Type<'a>>,
    pub signature: LoadedFunctionInstantiation<'a>,
    pub linkage: ExecutableLinkage<'a>,
    pub instruction_length: CodeOffset,
    pub definition_index: FunctionDefinitionIndex,
    pub visibility: Visibility,
    pub is_entry: bool,
    pub is_native: bool,
}

#[derive(Debug)]
pub struct MoveCall<'a> {
    pub function: LoadedFunction<'a>,
    pub arguments: Vec<'a, Argument>,
}

pub use messages::transaction::Argument;

//**************************************************************************************************
// impl
//**************************************************************************************************

impl ObjectArg {
    pub fn id(&self) -> ObjectId {
        self.kind.id()
    }
}

impl ObjectArgKind {
    pub fn id(&self) -> ObjectId {
        match self {
            Self::ImmObject(oref) | Self::OwnedObject(oref) => oref.0,
            Self::ConsensusObject { id, .. } => *id,
        }
    }
}

impl<'a> ModuleId<'a> {
    pub fn new_in(bump: &'a Bump, id: &move_tags::ModuleId) -> ModuleId<'a> {
        ModuleId {
            address: *id.address(),
            name: containers::alloc_str(bump, id.name().as_str()),
        }
    }

    pub fn address(&self) -> &AccountAddress {
        &self.address
    }

    pub fn name(&self) -> &'a str {
        self.name
    }

    /// The VM's `ModuleId`, owned, for a call into the VM.
    ///
    /// # Panics
    /// If the name is not an identifier: every module id here names a loaded module.
    pub fn to_move(&self) -> move_tags::ModuleId {
        move_tags::ModuleId::new(
            self.address,
            Identifier::new(self.name).expect("a loaded module's name"),
        )
    }
}

impl<'a> Type<'a> {
    pub fn abilities(&self) -> AbilitySet {
        match self {
            Type::Bool
            | Type::U8
            | Type::U16
            | Type::U32
            | Type::U64
            | Type::U128
            | Type::U256
            | Type::Address => AbilitySet::PRIMITIVES,
            Type::Signer => AbilitySet::SIGNER,
            Type::Reference(_, _) => AbilitySet::REFERENCES,
            Type::Vector(v) => v.abilities,
            Type::Datatype(dt) => dt.abilities,
        }
    }

    pub fn is_tx_context(&self) -> TxContextKind {
        let (is_mut, inner) = match self {
            Type::Reference(is_mut, inner) => (*is_mut, inner),
            _ => return TxContextKind::None,
        };
        let Type::Datatype(dt) = &**inner else {
            return TxContextKind::None;
        };
        if dt.is_resolved(RESOLVED_TX_CONTEXT) {
            if is_mut {
                TxContextKind::Mutable
            } else {
                TxContextKind::Immutable
            }
        } else {
            TxContextKind::None
        }
    }

    /// Is this the `TxContext` datatype itself, not behind a reference?
    pub fn is_tx_context_by_value(&self) -> bool {
        matches!(self, Type::Datatype(dt) if dt.is_resolved(RESOLVED_TX_CONTEXT))
    }

    pub fn all_addresses(&self, bump: &'a Bump) -> IndexSet<'a, AccountAddress> {
        match self {
            Type::Bool
            | Type::U8
            | Type::U16
            | Type::U32
            | Type::U64
            | Type::U128
            | Type::U256
            | Type::Address
            | Type::Signer => IndexSet::new_in(bump),
            Type::Vector(v) => v.element_type.all_addresses(bump),
            Type::Reference(_, inner) => inner.all_addresses(bump),
            Type::Datatype(dt) => dt.all_addresses(bump),
        }
    }

    pub fn node_count(&self) -> u64 {
        use Type::*;
        let mut total = 0u64;
        let mut stack = std::vec![*self];

        while let Some(ty) = stack.pop() {
            total = total.saturating_add(1);
            match ty {
                Bool | U8 | U16 | U32 | U64 | U128 | U256 | Address | Signer => {}
                Vector(v) => stack.push(v.element_type),
                Reference(_, inner) => stack.push(*inner),
                Datatype(dt) => {
                    stack.extend(dt.type_arguments);
                }
            }
        }

        total
    }

    pub fn is_reference(&self) -> bool {
        match self {
            Type::Bool
            | Type::U8
            | Type::U16
            | Type::U32
            | Type::U64
            | Type::U128
            | Type::U256
            | Type::Address
            | Type::Signer
            | Type::Vector(_)
            | Type::Datatype(_) => false,
            Type::Reference(_, _) => true,
        }
    }
}

impl<'a> Datatype<'a> {
    pub fn qualified_ident(&self) -> (&AccountAddress, &'a str, &'a str) {
        (&self.module.address, self.module.name, self.name)
    }

    /// Whether this is the datatype a `RESOLVED_*` constant names.
    pub fn is_resolved(
        &self,
        (address, module, name): (&AccountAddress, &IdentStr, &IdentStr),
    ) -> bool {
        self.module.address == *address
            && self.module.name == module.as_str()
            && self.name == name.as_str()
    }

    pub fn all_addresses(&self, bump: &'a Bump) -> IndexSet<'a, AccountAddress> {
        let mut addresses = IndexSet::new_in(bump);
        addresses.insert(self.module.address);
        for arg in self.type_arguments {
            addresses.extend(arg.all_addresses(bump));
        }
        addresses
    }
}

impl<'a> Command<'a> {
    pub fn arguments_mut(&mut self) -> std::boxed::Box<dyn Iterator<Item = &mut Argument> + '_> {
        match self {
            Command::MoveCall(mc) => std::boxed::Box::new(mc.arguments.iter_mut()),
            Command::TransferObjects(objs, recipient) => {
                std::boxed::Box::new(objs.iter_mut().chain(std::iter::once(recipient)))
            }
            Command::SplitCoins(coin, amounts) => {
                std::boxed::Box::new(std::iter::once(coin).chain(amounts.iter_mut()))
            }
            Command::MergeCoins(coin, coins) => {
                std::boxed::Box::new(std::iter::once(coin).chain(coins.iter_mut()))
            }
            Command::MakeMoveVec(_, elements) => std::boxed::Box::new(elements.iter_mut()),
            Command::Publish(_, _, _) => std::boxed::Box::new(std::iter::empty()),
            Command::Upgrade(_, _, _, obj, _) => std::boxed::Box::new(std::iter::once(obj)),
        }
    }

    pub fn arguments(&self) -> std::boxed::Box<dyn Iterator<Item = &Argument> + '_> {
        match self {
            Command::MoveCall(mc) => std::boxed::Box::new(mc.arguments.iter()),
            Command::TransferObjects(objs, recipient) => {
                std::boxed::Box::new(objs.iter().chain(std::iter::once(recipient)))
            }
            Command::SplitCoins(coin, amounts) => {
                std::boxed::Box::new(std::iter::once(coin).chain(amounts.iter()))
            }
            Command::MergeCoins(coin, coins) => {
                std::boxed::Box::new(std::iter::once(coin).chain(coins.iter()))
            }
            Command::MakeMoveVec(_, elements) => std::boxed::Box::new(elements.iter()),
            Command::Publish(_, _, _) => std::boxed::Box::new(std::iter::empty()),
            Command::Upgrade(_, _, _, obj, _) => std::boxed::Box::new(std::iter::once(obj)),
        }
    }
}

//**************************************************************************************************
// Traits
//**************************************************************************************************

/// The VM's `TypeTag` of a type, for a call into the VM.
impl TryFrom<Type<'_>> for TypeTag {
    type Error = &'static str;
    fn try_from(ty: Type<'_>) -> Result<Self, Self::Error> {
        Ok(match ty {
            Type::Bool => TypeTag::Bool,
            Type::U8 => TypeTag::U8,
            Type::U16 => TypeTag::U16,
            Type::U32 => TypeTag::U32,
            Type::U64 => TypeTag::U64,
            Type::U128 => TypeTag::U128,
            Type::U256 => TypeTag::U256,
            Type::Address => TypeTag::Address,
            Type::Signer => TypeTag::Signer,
            Type::Vector(inner) => {
                let Vector { element_type, .. } = inner;
                TypeTag::Vector(std::boxed::Box::new((*element_type).try_into()?))
            }
            Type::Datatype(dt) => TypeTag::Struct(std::boxed::Box::new(dt.try_into()?)),
            Type::Reference(_, _) => return Err("unexpected reference type"),
        })
    }
}

impl<'a> Type<'a> {
    /// The type's `TypeTag` as a view in `bump`, for what execution records (transfers, written
    /// objects) rather than hands to the VM. The reference converts to an owned `TypeTag`.
    pub fn type_tag_in(
        &self,
        bump: &'a Bump,
    ) -> Result<messages::type_tag::TypeTag<'a>, &'static str> {
        use messages::type_tag::TypeTag as V;
        Ok(match self {
            Type::Bool => V::Bool,
            Type::U8 => V::U8,
            Type::U16 => V::U16,
            Type::U32 => V::U32,
            Type::U64 => V::U64,
            Type::U128 => V::U128,
            Type::U256 => V::U256,
            Type::Address => V::Address,
            Type::Signer => V::Signer,
            Type::Vector(inner) => V::Vector(messages::arena::Ref::new(containers::alloc(
                bump,
                inner.element_type.type_tag_in(bump)?,
            ))),
            Type::Datatype(dt) => V::Struct(messages::arena::Ref::new(containers::alloc(
                bump,
                dt.struct_tag_in(bump)?,
            ))),
            Type::Reference(_, _) => return Err("unexpected reference type"),
        })
    }
}

impl<'a> Datatype<'a> {
    /// The datatype's `StructTag` as a view in `bump` (see `Type::type_tag_in`).
    pub fn struct_tag_in(
        &self,
        bump: &'a Bump,
    ) -> Result<messages::type_tag::StructTag<'a>, &'static str> {
        let mut type_params = Vec::with_capacity_in(self.type_arguments.len(), bump);
        for t in self.type_arguments {
            type_params.push(t.type_tag_in(bump)?);
        }
        Ok(messages::type_tag::StructTag {
            address: containers::alloc(
                bump,
                messages::base::AccountAddress(self.module.address.into_bytes()),
            ),
            module: self.module.name,
            name: self.name,
            type_params: type_params.leak(),
        })
    }
}

impl TryFrom<&Datatype<'_>> for StructTag {
    type Error = &'static str;

    fn try_from(dt: &Datatype<'_>) -> Result<Self, Self::Error> {
        let Datatype {
            module,
            name,
            type_arguments,
            ..
        } = dt;
        Ok(StructTag {
            address: module.address,
            module: Identifier::new(module.name).map_err(|_| "invalid module name")?,
            name: Identifier::new(*name).map_err(|_| "invalid type name")?,
            type_params: type_arguments
                .iter()
                .map(|t| (*t).try_into())
                .collect::<Result<std::vec::Vec<TypeTag>, _>>()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `type_tag_in` is the owned conversion, as a view.
    #[test]
    fn type_tag_views_match_owned_tags() {
        let bump = Bump::with_capacity(1 << 12);
        let module = |address: u16, name| ModuleId {
            address: AccountAddress::from_suffix(address),
            name,
        };
        let inner = Datatype {
            abilities: AbilitySet::EMPTY,
            module: module(7, "m"),
            name: "Inner",
            type_arguments: containers::alloc_slice_copy(&bump, &[Type::U8, Type::Address]),
        };
        let vector = Vector {
            abilities: AbilitySet::EMPTY,
            element_type: Type::Datatype(&inner),
        };
        let outer = Datatype {
            abilities: AbilitySet::EMPTY,
            module: module(2, "coin"),
            name: "Coin",
            type_arguments: containers::alloc_slice_copy(
                &bump,
                &[Type::Vector(&vector), Type::U256, Type::Bool],
            ),
        };
        for ty in [
            Type::U64,
            Type::Signer,
            Type::Vector(&vector),
            Type::Datatype(&inner),
            Type::Datatype(&outer),
        ] {
            let owned: TypeTag = ty.try_into().unwrap();
            let view = ty.type_tag_in(&bump).unwrap();
            assert_eq!(
                exec_types::type_tags::to_move_type_tag(&view),
                owned,
                "{ty:?}"
            );
        }
        let reference = Type::Reference(false, &Type::U8);
        assert!(reference.type_tag_in(&bump).is_err());
    }
}
