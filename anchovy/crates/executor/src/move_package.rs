// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The parts of `sui_types::move_package` that publish and upgrade use: a new package built as a
//! view in the arena, which the temporary store encodes once with the object it is written in.

use std::cmp::Ordering;
use std::hash::Hash;

use containers::{BTreeMap, BTreeSet, Bump, Vec, alloc, alloc_slice_copy, alloc_str};
use exec_types::base::object_id;
use exec_types::error::{ExecutionError, ExecutionErrorKind};
use exec_types::object::{OBJECT_START_VERSION, move_package_size, original_package_id};
use exec_types::storage::{SuiError, SuiResult};
use messages::base::{ObjectId, SequenceNumber, U64Le};
use messages::execution_status::PackageUpgradeError;
use messages::object::{Linkage, MovePackage, TypeOrigin};
use move_binary_format::{
    CompiledModule, binary_config::BinaryConfig, file_format_common::VERSION_6, normalized,
};
use sui_protocol_config::ProtocolConfig;

/// `MovePackage::new`: the package, if it is within `max_move_package_size`.
fn new<'a>(
    bump: &'a Bump,
    id: ObjectId,
    version: SequenceNumber,
    module_map: &'a [(&'a str, &'a [u8])],
    max_move_package_size: u64,
    type_origin_table: &'a [TypeOrigin<'a>],
    linkage_table: &'a [Linkage],
) -> Result<MovePackage<'a>, ExecutionError<'a>> {
    let pkg = MovePackage {
        id: alloc(bump, id),
        version,
        module_map,
        type_origin_table,
        linkage_table,
    };
    let object_size = move_package_size(&pkg) as u64;
    if object_size > max_move_package_size {
        return Err(ExecutionErrorKind::MovePackageTooBig {
            object_size,
            max_object_size: max_move_package_size,
        }
        .into());
    }
    Ok(pkg)
}

/// `MovePackage::new_initial`: an initial version of the package along with this version's type
/// origin and linkage tables.
pub fn new_initial<'a, 'p, 'd: 'p>(
    bump: &'a Bump,
    modules: &[CompiledModule],
    protocol_config: &ProtocolConfig,
    transitive_dependencies: impl IntoIterator<Item = &'p MovePackage<'d>>,
) -> Result<MovePackage<'a>, ExecutionError<'a>> {
    let module = modules
        .first()
        .expect("Tried to build a Move package from an empty iterator of Compiled modules");
    let runtime_id = object_id(module.address());
    let storage_id = runtime_id;
    let type_origin_table = build_initial_type_origin_table(bump, modules);
    from_module_iter_with_type_origin_table(
        bump,
        storage_id,
        runtime_id,
        OBJECT_START_VERSION,
        modules,
        protocol_config,
        type_origin_table,
        transitive_dependencies,
    )
}

/// `MovePackage::new_upgraded`: an upgraded version of `predecessor` along with this version's
/// type origin and linkage tables.
pub fn new_upgraded<'a, 'p, 'd: 'p>(
    bump: &'a Bump,
    predecessor: &MovePackage<'_>,
    storage_id: ObjectId,
    modules: &[CompiledModule],
    protocol_config: &ProtocolConfig,
    transitive_dependencies: impl IntoIterator<Item = &'p MovePackage<'d>>,
) -> Result<MovePackage<'a>, ExecutionError<'a>> {
    let module = modules
        .first()
        .expect("Tried to build a Move package from an empty iterator of Compiled modules");
    let runtime_id = object_id(module.address());
    let type_origin_table =
        build_upgraded_type_origin_table(bump, predecessor, modules, storage_id, protocol_config)?;
    let new_version = predecessor.version + 1;
    from_module_iter_with_type_origin_table(
        bump,
        storage_id,
        runtime_id,
        new_version,
        modules,
        protocol_config,
        type_origin_table,
        transitive_dependencies,
    )
}

fn from_module_iter_with_type_origin_table<'a, 'p, 'd: 'p>(
    bump: &'a Bump,
    storage_id: ObjectId,
    self_id: ObjectId,
    version: SequenceNumber,
    modules: &[CompiledModule],
    protocol_config: &ProtocolConfig,
    type_origin_table: &'a [TypeOrigin<'a>],
    transitive_dependencies: impl IntoIterator<Item = &'p MovePackage<'d>>,
) -> Result<MovePackage<'a>, ExecutionError<'a>> {
    // The reference's map is ordered by name; the encoding orders it by encoded name (below).
    let mut module_map: BTreeMap<&'a str, &'a [u8]> = BTreeMap::new_in(bump);
    let mut immediate_dependencies = BTreeSet::new_in(bump);

    for module in modules {
        let name = alloc_str(bump, module.name().as_str());

        immediate_dependencies.extend(
            module
                .immediate_dependencies()
                .into_iter()
                .map(|dep| object_id(dep.address())),
        );

        let mut bytes = std::vec::Vec::new();
        let version = if protocol_config.move_binary_format_version() > VERSION_6 {
            module.version
        } else {
            VERSION_6
        };
        module.serialize_with_version(version, &mut bytes).unwrap();
        let prev = module_map.insert(name, alloc_slice_copy(bump, &bytes));
        if protocol_config.new_vm_enabled() && prev.is_some() {
            return Err(ExecutionError::new_with_source(
                ExecutionErrorKind::VMVerificationOrDeserializationError,
                format!(
                    "Duplicate module {} in package {}",
                    module.self_id(),
                    storage_id
                ),
            ));
        }
    }

    immediate_dependencies.remove(&self_id);
    let linkage_table = build_linkage_table(
        bump,
        immediate_dependencies,
        transitive_dependencies,
        protocol_config,
    )?;
    let mut module_map_entries = Vec::with_capacity_in(module_map.len(), bump);
    module_map_entries.extend(module_map);
    module_map_entries.sort_by(|(a, _), (b, _)| cmp_encoded_names(a, b));
    new(
        bump,
        storage_id,
        version,
        module_map_entries.leak(),
        protocol_config.max_move_package_size(),
        type_origin_table,
        linkage_table,
    )
}

/// The order BCS gives map keys: by their encoding, length prefix included.
fn cmp_encoded_names(a: &str, b: &str) -> Ordering {
    fn uleb128(mut v: usize, out: &mut [u8; 10]) -> &[u8] {
        let mut n = 0;
        while v >= 0x80 {
            out[n] = (v & 0x7f) as u8 | 0x80;
            v >>= 7;
            n += 1;
        }
        out[n] = v as u8;
        &out[..=n]
    }
    let (mut la, mut lb) = ([0; 10], [0; 10]);
    let pa = uleb128(a.len(), &mut la);
    let pb = uleb128(b.len(), &mut lb);
    pa.iter()
        .chain(a.as_bytes())
        .cmp(pb.iter().chain(b.as_bytes()))
}

/// `build_linkage_table`: the linkage table, in `original_id` order.
fn build_linkage_table<'a, 'p, 'd: 'p>(
    bump: &'a Bump,
    mut immediate_dependencies: BTreeSet<'a, ObjectId>,
    transitive_dependencies: impl IntoIterator<Item = &'p MovePackage<'d>>,
    protocol_config: &ProtocolConfig,
) -> Result<&'a [Linkage], ExecutionError<'a>> {
    let mut linkage_table: BTreeMap<ObjectId, Linkage> = BTreeMap::new_in(bump);
    let mut dep_linkage_tables: Vec<&'d [Linkage]> = Vec::new_in(bump);

    for transitive_dep in transitive_dependencies {
        // original_package_id will deserialize a module but only for the purpose of obtaining
        // "original ID" of the package containing it so using max Move binary version during
        // deserialization is OK
        let original_id = original_package_id(transitive_dep);

        let imm_dep = immediate_dependencies.remove(&original_id);

        let info = Linkage {
            original_id,
            upgraded_id: *transitive_dep.id,
            upgraded_version: U64Le::new(transitive_dep.version),
        };
        if protocol_config.dependency_linkage_error() {
            dep_linkage_tables.push(transitive_dep.linkage_table);
            let existing = linkage_table.insert(original_id, info);

            if existing.is_some() {
                return Err(ExecutionErrorKind::InvalidLinkage.into());
            }
        } else {
            if imm_dep {
                // Found an immediate dependency, mark it as seen, and stash a reference to its linkage
                // table to check later.
                dep_linkage_tables.push(transitive_dep.linkage_table);
            }
            linkage_table.insert(original_id, info);
        }
    }
    // (1) Every dependency is represented in the transitive dependencies
    if !immediate_dependencies.is_empty() {
        return Err(ExecutionErrorKind::PublishUpgradeMissingDependency.into());
    }

    // (2) Every dependency's linkage table is superseded by this linkage table
    for dep_linkage_table in dep_linkage_tables {
        for dep_info in dep_linkage_table {
            let Some(our_info) = linkage_table.get(&dep_info.original_id) else {
                return Err(ExecutionErrorKind::PublishUpgradeMissingDependency.into());
            };

            if our_info.upgraded_version.get() < dep_info.upgraded_version.get() {
                return Err(ExecutionErrorKind::PublishUpgradeDependencyDowngrade.into());
            }
        }
    }

    let mut table = Vec::with_capacity_in(linkage_table.len(), bump);
    table.extend(linkage_table.into_values());
    Ok(table.leak())
}

/// `build_initial_type_origin_table`.
fn build_initial_type_origin_table<'a>(
    bump: &'a Bump,
    modules: &[CompiledModule],
) -> &'a [TypeOrigin<'a>] {
    let mut table = Vec::with_capacity_in(
        modules
            .iter()
            .map(|m| m.struct_defs().len() + m.enum_defs().len())
            .sum(),
        bump,
    );
    for m in modules {
        let module_name = alloc_str(bump, m.name().as_str());
        let package = alloc(bump, object_id(m.self_id().address()));
        for struct_def in m.struct_defs() {
            let struct_handle = m.datatype_handle_at(struct_def.struct_handle);
            let struct_name = alloc_str(bump, m.identifier_at(struct_handle.name).as_str());
            table.push(TypeOrigin {
                module_name,
                datatype_name: struct_name,
                package,
            });
        }
        for enum_def in m.enum_defs() {
            let enum_handle = m.datatype_handle_at(enum_def.enum_handle);
            let enum_name = alloc_str(bump, m.identifier_at(enum_handle.name).as_str());
            table.push(TypeOrigin {
                module_name,
                datatype_name: enum_name,
                package,
            });
        }
    }
    table.leak()
}

/// `build_upgraded_type_origin_table`.
fn build_upgraded_type_origin_table<'a>(
    bump: &'a Bump,
    predecessor: &MovePackage<'_>,
    modules: &[CompiledModule],
    storage_id: ObjectId,
    protocol_config: &ProtocolConfig,
) -> Result<&'a [TypeOrigin<'a>], ExecutionError<'a>> {
    let mut new_table = Vec::with_capacity_in(predecessor.type_origin_table.len(), bump);
    // `MovePackage::type_origin_map`.
    let mut existing_table: BTreeMap<(&str, &str), ObjectId> = BTreeMap::new_in(bump);
    existing_table.extend(
        predecessor
            .type_origin_table
            .iter()
            .map(|t| ((t.module_name, t.datatype_name), *t.package)),
    );
    let storage_id: &'a ObjectId = alloc(bump, storage_id);
    for m in modules {
        let module_name = alloc_str(bump, m.name().as_str());
        for struct_def in m.struct_defs() {
            let struct_handle = m.datatype_handle_at(struct_def.struct_handle);
            let struct_name = alloc_str(bump, m.identifier_at(struct_handle.name).as_str());
            // if id exists in the predecessor's table, use it, otherwise use the id of the upgraded
            // module
            let package = existing_table
                .remove(&(module_name, struct_name))
                .map_or(storage_id, |id| alloc(bump, id));
            new_table.push(TypeOrigin {
                module_name,
                datatype_name: struct_name,
                package,
            });
        }

        for enum_def in m.enum_defs() {
            let enum_handle = m.datatype_handle_at(enum_def.enum_handle);
            let enum_name = alloc_str(bump, m.identifier_at(enum_handle.name).as_str());
            // if id exists in the predecessor's table, use it, otherwise use the id of the upgraded
            // module
            let package = existing_table
                .remove(&(module_name, enum_name))
                .map_or(storage_id, |id| alloc(bump, id));
            new_table.push(TypeOrigin {
                module_name,
                datatype_name: enum_name,
                package,
            });
        }
    }

    if !existing_table.is_empty() {
        if protocol_config.missing_type_is_compatibility_error() {
            Err(ExecutionError::from_kind(
                ExecutionErrorKind::PackageUpgradeError {
                    upgrade_error: PackageUpgradeError::IncompatibleUpgrade,
                },
            ))
        } else {
            Err(ExecutionError::invariant_violation(
                "Package upgrade missing type from previous version.",
            ))
        }
    } else {
        Ok(new_table.leak())
    }
}

/// `MovePackage::normalize` (`normalize_modules` over its modules). If `include_code` is set to
/// `false`, the normalized module will skip function bodies but still include the signatures.
pub fn normalize<'a, S: Hash + Eq + Clone + ToString, Pool: normalized::StringPool<String = S>>(
    bump: &'a Bump,
    package: &MovePackage<'_>,
    pool: &mut Pool,
    binary_config: &BinaryConfig,
    include_code: bool,
) -> SuiResult<BTreeMap<'a, String, normalized::Module<S>>> {
    let mut normalized_modules = BTreeMap::new_in(bump);
    for (_, bytecode) in package.module_map {
        let module = CompiledModule::deserialize_with_config(bytecode, binary_config)
            .map_err(|error| SuiError(format!("ModuleDeserializationFailure: {error}")))?;
        let normalized_module = normalized::Module::new(pool, &module, include_code);
        normalized_modules.insert(normalized_module.name().to_string(), normalized_module);
    }
    Ok(normalized_modules)
}

/// `normalize_deserialized_modules`. If `include_code` is set to `false`, the normalized module
/// will skip function bodies but still include the signatures.
pub fn normalize_deserialized_modules<
    'a,
    'm,
    S: Hash + Eq + Clone + ToString,
    Pool: normalized::StringPool<String = S>,
    I,
>(
    bump: &'a Bump,
    pool: &mut Pool,
    modules: I,
    include_code: bool,
) -> BTreeMap<'a, String, normalized::Module<S>>
where
    I: Iterator<Item = &'m CompiledModule>,
{
    let mut normalized_modules = BTreeMap::new_in(bump);
    for module in modules {
        let normalized_module = normalized::Module::new(pool, module, include_code);
        normalized_modules.insert(normalized_module.name().to_string(), normalized_module);
    }
    normalized_modules
}
