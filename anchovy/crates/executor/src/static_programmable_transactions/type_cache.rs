// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Types and layouts computed by the VM, kept across the transactions of an epoch. Not in the
//! reference, which asks the VM again in every transaction.

use crate::static_programmable_transactions::loading::ast::{Datatype, ModuleId, Type, Vector};
use containers::Bump;
use move_binary_format::file_format::AbilitySet;
use move_core_types::{
    account_address::AccountAddress, annotated_value, language_storage::TypeTag, runtime_value,
};
use move_vm_runtime::shared::linkage_context::LinkageContext;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// Entries kept before the cache is emptied, which bounds its memory however many linkages and
/// types an epoch's transactions name.
const MAX_ENTRIES: usize = 16 * 1024;

type LinkageTable = [(AccountAddress, AccountAddress)];

type ByLinkage = HashMap<Box<LinkageTable>, Arc<Mutex<Bucket>>>;

/// Buckets by `ExecutionMode::packages_are_predefined`, then by linkage table.
#[derive(Default)]
struct Buckets {
    predefined: ByLinkage,
    published: ByLinkage,
}

/// The answers of VMs for type tags, by the VM's linkage table and the execution mode's linkage
/// config.
///
/// A hit is the answer the operation would compute. Each operation (see `Bucket`) is a function
/// of its tag, the VM's config, the linkage config and the packages the linkage table and the
/// tag's addresses name, by ID. Within an epoch the configs are fixed (the linkage config also
/// depends on the mode, which is part of the key) and a package never changes or disappears, with
/// two exceptions that users must exclude: packages published by the transaction, which may be
/// rolled back or not yet committed, so nothing is kept or read while the transaction has new
/// packages; and the system packages, which are upgraded in place at the end of an epoch, so a
/// cache serves one epoch and system transactions do not use it. Only answers computed without
/// error are kept.
#[derive(Default)]
pub struct TypeCache {
    buckets: Mutex<Buckets>,
    entries: AtomicUsize,
}

/// One linkage's answers, by type tag.
#[derive(Default)]
pub struct Bucket {
    /// `Env::fully_annotated_layout`.
    pub annotated: HashMap<TypeTag, Arc<annotated_value::MoveTypeLayout>>,
    /// `Env::vm_runtime_layout`.
    pub runtime: HashMap<TypeTag, Arc<runtime_value::MoveTypeLayout>>,
    /// `Env::load_type_from_struct`.
    pub types: HashMap<TypeTag, OwnedType>,
    /// The type and layout of a written object's type, in the write-out VM.
    pub writeout: HashMap<TypeTag, (OwnedType, Arc<runtime_value::MoveTypeLayout>)>,
    /// The layout of an event's type, in the emitting function's VM.
    pub events: HashMap<TypeTag, Arc<runtime_value::MoveTypeLayout>>,
}

/// The bucket of one VM's linkage.
#[derive(Clone)]
pub struct CachedLinkage {
    cache: Arc<TypeCache>,
    bucket: Arc<Mutex<Bucket>>,
}

impl TypeCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The bucket for a VM over `linkage`, under the linkage config of a mode with
    /// `packages_are_predefined`.
    pub fn bucket(
        self: &Arc<Self>,
        bump: &Bump,
        packages_are_predefined: bool,
        linkage: &LinkageContext,
    ) -> CachedLinkage {
        let mut key = containers::Vec::with_capacity_in(linkage.linkage_table().len(), bump);
        key.extend(linkage.linkage_table().iter().map(|(k, v)| (*k, *v)));
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        if self.entries.load(Ordering::Relaxed) >= MAX_ENTRIES {
            // Buckets already handed out stay valid; they are only no longer shared.
            *buckets = Buckets::default();
            self.entries.store(0, Ordering::Relaxed);
        }
        let by_linkage = if packages_are_predefined {
            &mut buckets.predefined
        } else {
            &mut buckets.published
        };
        let bucket = match by_linkage.get(&key[..]) {
            Some(bucket) => bucket.clone(),
            None => {
                let bucket = Arc::default();
                by_linkage.insert(key[..].into(), Arc::clone(&bucket));
                bucket
            }
        };
        CachedLinkage {
            cache: Arc::clone(self),
            bucket,
        }
    }
}

impl CachedLinkage {
    /// `f`'s answer from the bucket.
    pub fn get<R>(&self, f: impl FnOnce(&Bucket) -> Option<R>) -> Option<R> {
        f(&self.bucket.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Keeps an entry `f` inserts.
    pub fn insert(&self, f: impl FnOnce(&mut Bucket)) {
        f(&mut self.bucket.lock().unwrap_or_else(PoisonError::into_inner));
        self.cache.entries.fetch_add(1, Ordering::Relaxed);
    }
}

/// A `Type` outside the arena.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnedType {
    Bool,
    U8,
    U16,
    U32,
    U64,
    U128,
    U256,
    Address,
    Signer,
    Vector(AbilitySet, Box<OwnedType>),
    Datatype(Box<OwnedDatatype>),
    Reference(/* is mut */ bool, Box<OwnedType>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedDatatype {
    abilities: AbilitySet,
    address: AccountAddress,
    module: Box<str>,
    name: Box<str>,
    type_arguments: std::vec::Vec<OwnedType>,
}

impl OwnedType {
    pub fn new(ty: &Type<'_>) -> Self {
        match ty {
            Type::Bool => Self::Bool,
            Type::U8 => Self::U8,
            Type::U16 => Self::U16,
            Type::U32 => Self::U32,
            Type::U64 => Self::U64,
            Type::U128 => Self::U128,
            Type::U256 => Self::U256,
            Type::Address => Self::Address,
            Type::Signer => Self::Signer,
            Type::Vector(v) => Self::Vector(v.abilities, Box::new(Self::new(&v.element_type))),
            Type::Datatype(d) => Self::Datatype(Box::new(OwnedDatatype {
                abilities: d.abilities,
                address: d.module.address,
                module: d.module.name.into(),
                name: d.name.into(),
                type_arguments: d.type_arguments.iter().map(Self::new).collect(),
            })),
            Type::Reference(is_mut, inner) => Self::Reference(*is_mut, Box::new(Self::new(inner))),
        }
    }

    /// The `Type`, in `bump`.
    pub fn in_arena<'a>(&self, bump: &'a Bump) -> Type<'a> {
        match self {
            Self::Bool => Type::Bool,
            Self::U8 => Type::U8,
            Self::U16 => Type::U16,
            Self::U32 => Type::U32,
            Self::U64 => Type::U64,
            Self::U128 => Type::U128,
            Self::U256 => Type::U256,
            Self::Address => Type::Address,
            Self::Signer => Type::Signer,
            Self::Vector(abilities, element) => Type::Vector(containers::leak(
                bump,
                Vector {
                    abilities: *abilities,
                    element_type: element.in_arena(bump),
                },
            )),
            Self::Datatype(d) => {
                let mut type_arguments =
                    containers::Vec::with_capacity_in(d.type_arguments.len(), bump);
                type_arguments.extend(d.type_arguments.iter().map(|t| t.in_arena(bump)));
                Type::Datatype(containers::leak(
                    bump,
                    Datatype {
                        abilities: d.abilities,
                        module: ModuleId {
                            address: d.address,
                            name: containers::alloc_str(bump, &d.module),
                        },
                        name: containers::alloc_str(bump, &d.name),
                        type_arguments: type_arguments.leak(),
                    },
                ))
            }
            Self::Reference(is_mut, inner) => {
                Type::Reference(*is_mut, containers::alloc(bump, inner.in_arena(bump)))
            }
        }
    }
}
