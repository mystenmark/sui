// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Vectors for the stateful input checks: one set of objects, then
//! transactions against it with the reference's verdict. The verdict is
//! what a validator voting on the transaction reports: sui-core's loading
//! (`TransactionInputLoader::read_objects_for_signing`, replicated here, as
//! sui-core is too heavy to link), `sui_transaction_checks::
//! check_transaction_input` itself, then that owned inputs are live
//! (`ObjectLocks::validate_owned_object_versions`, replicated).
//!
//! ```text
//! context <protocol version> <reference gas price> <epoch>
//! object <live|stale> <Object BCS hex>
//! case <label> <TransactionData BCS hex> <verdict>
//! ```
//!
//! A verdict is `ok` or the `UserInputError` variant's name. Every
//! transaction passes the static validity checks; none draws on address
//! balances or publishes a package (the checks anchovy refuses or skips).

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::Arc;

use move_core_types::ident_str;
use move_core_types::language_storage::{StructTag, TypeTag};
use sui_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use sui_types::base_types::{ObjectID, ObjectRef, SequenceNumber, SuiAddress};
use sui_types::digests::{ObjectDigest, TransactionDigest};
use sui_types::error::{SuiError, SuiErrorKind, UserInputError};
use sui_types::metrics::BytecodeVerifierMetrics;
use sui_types::object::{MoveObject, Object, Owner};
use sui_types::transaction::{
    Argument, CallArg, Command, InputObjectKind, InputObjects, ObjectArg, ObjectReadResult,
    ObjectReadResultKind, ProgrammableTransaction, ReceivingObjectReadResult,
    ReceivingObjectReadResultKind, ReceivingObjects, SharedObjectMutability, TransactionData,
    TransactionDataAPI,
};
use sui_types::{SUI_CLOCK_OBJECT_ID, SUI_FRAMEWORK_ADDRESS};

const RGP: u64 = 1000;
const BUDGET: u64 = 50_000_000;
const SUI: u64 = 1_000_000_000;

/// Every version of every object, and which is live.
#[derive(Default)]
struct Store {
    versions: HashMap<ObjectID, BTreeMap<SequenceNumber, Object>>,
    live: HashMap<ObjectID, SequenceNumber>,
}

impl Store {
    fn add(&mut self, object: Object, live: bool) {
        let id = object.id();
        let version = object.version();
        self.versions.entry(id).or_default().insert(version, object);
        if live {
            self.live.insert(id, version);
        }
    }

    fn live(&self, id: &ObjectID) -> Option<&Object> {
        let version = self.live.get(id)?;
        self.versions.get(id)?.get(version)
    }

    fn at(&self, id: &ObjectID, version: SequenceNumber) -> Option<&Object> {
        self.versions.get(id)?.get(&version)
    }
}

/// `read_objects_for_signing`, replicated.
fn load(
    store: &Store,
    kinds: &[InputObjectKind],
    receiving: &[ObjectRef],
) -> Result<(InputObjects, ReceivingObjects), SuiError> {
    let mut results: Vec<Option<ObjectReadResult>> = vec![None; kinds.len()];
    let mut owned = vec![];
    for (i, kind) in kinds.iter().enumerate() {
        match kind {
            InputObjectKind::MovePackage(id) => {
                // `get_package_object`.
                let Some(object) = store.live(id) else {
                    return Err(kind.object_not_found_error().into());
                };
                if !object.is_package() {
                    return Err(UserInputError::MoveObjectAsPackage { object_id: *id }.into());
                }
                results[i] = Some(ObjectReadResult::new(*kind, object.clone().into()));
            }
            InputObjectKind::SharedMoveObject { .. } => match store.live(&kind.object_id()) {
                Some(object) if object.full_id() == kind.full_object_id() => {
                    results[i] = Some(ObjectReadResult::new(*kind, object.clone().into()));
                }
                // No consensus streams end here.
                _ => return Err(kind.object_not_found_error().into()),
            },
            InputObjectKind::ImmOrOwnedMoveObject(r) => owned.push((i, *r)),
        }
    }
    // `multi_get_objects_with_more_accurate_error_return`.
    for (i, r) in owned {
        let Some(object) = store.at(&r.0, r.1) else {
            let Some(live) = store.live(&r.0) else {
                return Err(UserInputError::ObjectNotFound {
                    object_id: r.0,
                    version: None,
                }
                .into());
            };
            return Err(if live.version() >= r.1 {
                UserInputError::ObjectVersionUnavailableForConsumption {
                    provided_obj_ref: r,
                    current_version: live.version(),
                }
            } else {
                UserInputError::ObjectNotFound {
                    object_id: r.0,
                    version: Some(r.1),
                }
            }
            .into());
        };
        results[i] = Some(ObjectReadResult::new(
            kinds[i],
            ObjectReadResultKind::Object(object.clone()),
        ));
    }
    // `read_receiving_objects_for_signing`, with no received markers.
    let mut received = vec![];
    for r in receiving {
        let Some(object) = store.live(&r.0) else {
            return Err(UserInputError::ObjectNotFound {
                object_id: r.0,
                version: Some(r.1),
            }
            .into());
        };
        received.push(ReceivingObjectReadResult::new(
            *r,
            ReceivingObjectReadResultKind::Object(object.clone()),
        ));
    }
    Ok((
        results
            .into_iter()
            .map(Option::unwrap)
            .collect::<Vec<_>>()
            .into(),
        received.into(),
    ))
}

/// `validate_owned_object_versions`, replicated.
fn validate_owned(store: &Store, owned: &[ObjectRef]) -> Result<(), SuiError> {
    let mut live = vec![];
    for r in owned {
        let object = store.live(&r.0).ok_or(UserInputError::ObjectNotFound {
            object_id: r.0,
            version: None,
        })?;
        live.push((r, object));
    }
    for (r, object) in live {
        if r.1 != object.version() {
            return Err(UserInputError::ObjectVersionUnavailableForConsumption {
                provided_obj_ref: *r,
                current_version: object.version(),
            }
            .into());
        }
        if r.2 != object.digest() {
            return Err(UserInputError::InvalidObjectDigest {
                object_id: r.0,
                expected_digest: object.digest(),
            }
            .into());
        }
    }
    Ok(())
}

fn verdict(store: &Store, config: &ProtocolConfig, tx: &TransactionData) -> String {
    let result = (|| -> Result<(), SuiError> {
        let kinds = tx.input_objects()?;
        let receiving = tx.receiving_objects();
        let (inputs, received) = load(store, &kinds, &receiving)?;
        let metrics = Arc::new(BytecodeVerifierMetrics::new(&prometheus::Registry::new()));
        let (_, checked) = sui_transaction_checks::check_transaction_input(
            config,
            RGP,
            tx,
            inputs,
            &received,
            &metrics,
            &sui_config::verifier_signing_config::VerifierSigningConfig::default(),
        )?;
        validate_owned(store, &checked.inner().filter_owned_objects())
    })();
    match result {
        Ok(()) => "ok".to_owned(),
        Err(e) => name(&e),
    }
}

/// The `UserInputError` variant's name, or the `SuiError` kind's.
fn name(e: &SuiError) -> String {
    let text = match e.as_inner() {
        SuiErrorKind::UserInputError { error } => format!("{error:?}"),
        other => format!("{other:?}"),
    };
    text.split(|c: char| !c.is_alphanumeric())
        .next()
        .unwrap()
        .to_owned()
}

/// The objects, by role.
struct World {
    sender: SuiAddress,
    sponsor: SuiAddress,
    gas: Object,
    second_gas: Object,
    poor_gas: Object,
    sponsor_gas: Object,
    stranger_gas: Object,
    not_sui: Object,
    frozen: Object,
    shared: Object,
    consensus_owned: Object,
    stranger_consensus_owned: Object,
    child: Object,
    stale: Object,
    deleted: Object,
    to_receive: Object,
    clock: Object,
}

fn id(n: u8) -> ObjectID {
    ObjectID::new([n; 32])
}

fn coin(n: u8, owner: Owner, value: u64) -> Object {
    let coin = MoveObject::new_gas_coin(SequenceNumber::from_u64(1), id(n), value);
    Object::new_move(coin, owner, TransactionDigest::genesis_marker())
}

fn world(store: &mut Store) -> World {
    let address = |n: u8| SuiAddress::from(ObjectID::new([n; 32]));
    let (sender, sponsor, stranger) = (address(0xa1), address(0xa2), address(0xa3));
    for package in sui_framework::BuiltInFramework::genesis_objects() {
        store.add(package, true);
    }
    let other_coin = TypeTag::Struct(Box::new(StructTag {
        address: SUI_FRAMEWORK_ADDRESS,
        module: ident_str!("other").to_owned(),
        name: ident_str!("OTHER").to_owned(),
        type_params: vec![],
    }));
    let not_sui = Object::new_move(
        MoveObject::new_coin(other_coin, SequenceNumber::from_u64(1), id(0x16), 5 * SUI),
        Owner::AddressOwner(sender),
        TransactionDigest::genesis_marker(),
    );
    // A version the sender's coin no longer has, and the live one.
    let stale = coin(0x1a, Owner::AddressOwner(sender), SUI);
    let mut current = stale.clone();
    current
        .data
        .try_as_move_mut()
        .unwrap()
        .increment_version_to(SequenceNumber::from_u64(2));
    store.add(stale.clone(), false);
    store.add(current, true);
    let deleted = coin(0x1b, Owner::AddressOwner(sender), SUI);
    store.add(deleted.clone(), false);
    let clock = {
        let mut contents = SUI_CLOCK_OBJECT_ID.to_vec();
        contents.extend(0u64.to_le_bytes());
        let config = ProtocolConfig::get_for_max_version_UNSAFE();
        let object = unsafe {
            MoveObject::new_from_execution(
                sui_types::clock::Clock::type_().into(),
                false,
                SequenceNumber::from_u64(1),
                contents,
                &config,
                true,
            )
            .unwrap()
        };
        Object::new_move(
            object,
            Owner::Shared {
                initial_shared_version: SequenceNumber::from_u64(1),
            },
            TransactionDigest::genesis_marker(),
        )
    };
    let world = World {
        sender,
        sponsor,
        gas: coin(0x11, Owner::AddressOwner(sender), 10 * SUI),
        second_gas: coin(0x12, Owner::AddressOwner(sender), 10 * SUI),
        poor_gas: coin(0x13, Owner::AddressOwner(sender), 100),
        sponsor_gas: coin(0x14, Owner::AddressOwner(sponsor), 10 * SUI),
        stranger_gas: coin(0x15, Owner::AddressOwner(stranger), 10 * SUI),
        not_sui,
        frozen: coin(0x17, Owner::Immutable, 10 * SUI),
        shared: coin(
            0x18,
            Owner::Shared {
                initial_shared_version: SequenceNumber::from_u64(1),
            },
            SUI,
        ),
        consensus_owned: coin(
            0x19,
            Owner::ConsensusAddressOwner {
                start_version: SequenceNumber::from_u64(1),
                owner: sender,
            },
            SUI,
        ),
        stranger_consensus_owned: coin(
            0x1c,
            Owner::ConsensusAddressOwner {
                start_version: SequenceNumber::from_u64(1),
                owner: stranger,
            },
            SUI,
        ),
        child: coin(0x1d, Owner::ObjectOwner(address(0x11)), SUI),
        stale,
        deleted,
        to_receive: coin(0x1e, Owner::AddressOwner(address(0x11)), SUI),
        clock,
    };
    for object in [
        &world.gas,
        &world.second_gas,
        &world.poor_gas,
        &world.sponsor_gas,
        &world.stranger_gas,
        &world.not_sui,
        &world.frozen,
        &world.shared,
        &world.consensus_owned,
        &world.stranger_consensus_owned,
        &world.child,
        &world.to_receive,
        &world.clock,
    ] {
        store.add(object.clone(), true);
    }
    world
}

/// A transaction: inputs and commands from `build`, paid by `gas`.
fn tx(
    w: &World,
    gas: Vec<ObjectRef>,
    gas_owner: SuiAddress,
    price: u64,
    build: impl FnOnce(&mut Call),
) -> TransactionData {
    let mut c = Call {
        package: SUI_FRAMEWORK_ADDRESS.into(),
        type_args: vec![],
        args: vec![],
    };
    build(&mut c);
    // Built by hand: the builder merges and checks inputs, and some cases
    // need inputs it refuses.
    let arguments = (0..c.args.len() as u16).map(Argument::Input).collect();
    let pt = ProgrammableTransaction {
        inputs: c.args,
        commands: vec![Command::move_call(
            c.package,
            ident_str!("coin").to_owned(),
            ident_str!("value").to_owned(),
            c.type_args,
            arguments,
        )],
    };
    TransactionData::new_programmable_allow_sponsor(w.sender, gas, pt, BUDGET, price, gas_owner)
}

/// One call, of `package::coin::value`, whatever its signature: the checks
/// never look past the names.
struct Call {
    package: ObjectID,
    type_args: Vec<TypeTag>,
    args: Vec<CallArg>,
}

fn call(c: &mut Call, package: ObjectID, type_args: Vec<TypeTag>, args: Vec<CallArg>) {
    *c = Call {
        package,
        type_args,
        args,
    };
}

fn owned(o: &Object) -> CallArg {
    CallArg::Object(ObjectArg::ImmOrOwnedObject(o.compute_object_reference()))
}

fn shared(o: &Object, initial: u64, mutable: bool) -> CallArg {
    CallArg::Object(ObjectArg::SharedObject {
        id: o.id(),
        initial_shared_version: SequenceNumber::from_u64(initial),
        mutability: if mutable {
            SharedObjectMutability::Mutable
        } else {
            SharedObjectMutability::Immutable
        },
    })
}

fn receiving(r: ObjectRef) -> CallArg {
    CallArg::Object(ObjectArg::Receiving(r))
}

fn with_version(o: &Object, version: u64) -> ObjectRef {
    let r = o.compute_object_reference();
    (r.0, SequenceNumber::from_u64(version), r.2)
}

fn with_digest(o: &Object, digest: u8) -> ObjectRef {
    let r = o.compute_object_reference();
    (r.0, r.1, ObjectDigest::new([digest; 32]))
}

fn cases(w: &World) -> Vec<(&'static str, TransactionData)> {
    let gas = || vec![w.gas.compute_object_reference()];
    let sui = SUI_FRAMEWORK_ADDRESS.into();
    let one = |label, args: Vec<CallArg>| {
        (
            label,
            tx(w, gas(), w.sender, RGP, |b| call(b, sui, vec![], args)),
        )
    };
    let pay = |label, payment: Vec<ObjectRef>, owner: SuiAddress| {
        (
            label,
            tx(w, payment, owner, RGP, |b| call(b, sui, vec![], vec![])),
        )
    };
    let missing_type = TypeTag::Struct(Box::new(StructTag {
        address: id(0xbe).into(),
        module: ident_str!("m").to_owned(),
        name: ident_str!("T").to_owned(),
        type_params: vec![],
    }));
    vec![
        one("plain", vec![]),
        one("owned", vec![owned(&w.second_gas)]),
        one("immutable", vec![owned(&w.frozen)]),
        one("not_sui_input", vec![owned(&w.not_sui)]),
        (
            "missing_package",
            tx(w, gas(), w.sender, RGP, |b| {
                call(b, id(0xde), vec![], vec![])
            }),
        ),
        (
            "object_as_package",
            tx(w, gas(), w.sender, RGP, |b| {
                call(b, w.second_gas.id(), vec![], vec![])
            }),
        ),
        (
            "missing_type_package",
            tx(w, gas(), w.sender, RGP, |b| {
                call(b, sui, vec![missing_type], vec![])
            }),
        ),
        one("shared", vec![shared(&w.shared, 1, true)]),
        one("shared_immutably", vec![shared(&w.shared, 1, false)]),
        one("shared_wrong_start", vec![shared(&w.shared, 2, true)]),
        one("owned_as_shared", vec![shared(&w.second_gas, 1, true)]),
        one("shared_as_owned", vec![owned(&w.shared)]),
        one("consensus_owned", vec![shared(&w.consensus_owned, 1, true)]),
        one(
            "consensus_owned_by_stranger",
            vec![shared(&w.stranger_consensus_owned, 1, true)],
        ),
        one("consensus_owned_as_owned", vec![owned(&w.consensus_owned)]),
        one("clock", vec![shared(&w.clock, 1, false)]),
        one("clock_mutably", vec![shared(&w.clock, 1, true)]),
        one(
            "stale_version",
            vec![CallArg::Object(ObjectArg::ImmOrOwnedObject(
                w.stale.compute_object_reference(),
            ))],
        ),
        one(
            "future_version",
            vec![CallArg::Object(ObjectArg::ImmOrOwnedObject(with_version(
                &w.second_gas,
                99,
            )))],
        ),
        one(
            "no_such_object",
            vec![CallArg::Object(ObjectArg::ImmOrOwnedObject((
                id(0xef),
                SequenceNumber::from_u64(1),
                ObjectDigest::new([1; 32]),
            )))],
        ),
        one("deleted", vec![owned(&w.deleted)]),
        one(
            "wrong_digest",
            vec![CallArg::Object(ObjectArg::ImmOrOwnedObject(with_digest(
                &w.second_gas,
                7,
            )))],
        ),
        one("strangers_object", vec![owned(&w.stranger_gas)]),
        one("child", vec![owned(&w.child)]),
        one(
            "package_as_object",
            vec![owned(
                &sui_framework::BuiltInFramework::genesis_objects()
                    .next()
                    .unwrap(),
            )],
        ),
        one("gas_also_input", vec![owned(&w.gas)]),
        one(
            "duplicate_input",
            vec![owned(&w.second_gas), owned(&w.second_gas)],
        ),
        pay(
            "two_gas_coins",
            vec![
                w.gas.compute_object_reference(),
                w.second_gas.compute_object_reference(),
            ],
            w.sender,
        ),
        pay(
            "poor_gas",
            vec![w.poor_gas.compute_object_reference()],
            w.sender,
        ),
        pay(
            "frozen_gas",
            vec![w.frozen.compute_object_reference()],
            w.sender,
        ),
        pay(
            "shared_gas",
            vec![w.shared.compute_object_reference()],
            w.sender,
        ),
        pay(
            "not_sui_gas",
            vec![w.not_sui.compute_object_reference()],
            w.sender,
        ),
        pay(
            "sponsored",
            vec![w.sponsor_gas.compute_object_reference()],
            w.sponsor,
        ),
        pay(
            "sponsor_pays_with_senders_coin",
            vec![w.gas.compute_object_reference()],
            w.sponsor,
        ),
        pay(
            "strangers_gas",
            vec![w.stranger_gas.compute_object_reference()],
            w.sender,
        ),
        pay(
            "stale_gas",
            vec![w.stale.compute_object_reference()],
            w.sender,
        ),
        (
            "price_under_rgp",
            tx(w, gas(), w.sender, RGP - 1, |b| {
                call(b, sui, vec![], vec![])
            }),
        ),
        one(
            "receive",
            vec![receiving(w.to_receive.compute_object_reference())],
        ),
        one(
            "receive_stale",
            vec![receiving(w.stale.compute_object_reference())],
        ),
        one(
            "receive_max_version",
            vec![receiving(with_version(&w.to_receive, u64::MAX))],
        ),
        one(
            "receive_future",
            vec![receiving(with_version(&w.to_receive, 5))],
        ),
        one(
            "receive_wrong_digest",
            vec![receiving(with_digest(&w.to_receive, 3))],
        ),
        one(
            "receive_child",
            vec![receiving(w.child.compute_object_reference())],
        ),
        one(
            "receive_shared",
            vec![receiving(w.shared.compute_object_reference())],
        ),
        one(
            "receive_frozen",
            vec![receiving(w.frozen.compute_object_reference())],
        ),
        one(
            "receive_missing",
            vec![receiving((
                id(0xee),
                SequenceNumber::from_u64(1),
                ObjectDigest::new([2; 32]),
            ))],
        ),
        one(
            "receive_an_input",
            vec![
                owned(&w.second_gas),
                receiving(w.second_gas.compute_object_reference()),
            ],
        ),
        one(
            "receive_package",
            vec![receiving(
                sui_framework::BuiltInFramework::genesis_objects()
                    .next()
                    .unwrap()
                    .compute_object_reference(),
            )],
        ),
    ]
}

pub fn vectors() -> Vec<u8> {
    let version = ProtocolVersion::MAX;
    let config = ProtocolConfig::get_for_version(version, Chain::Unknown);
    let mut store = Store::default();
    let w = world(&mut store);
    let mut out = String::new();
    writeln!(out, "context {} {RGP} 0", version.as_u64()).unwrap();
    let mut ids: Vec<_> = store.versions.keys().copied().collect();
    ids.sort();
    for id in ids {
        for (version, object) in &store.versions[&id] {
            let live = store.live.get(&id) == Some(version);
            let state = if live { "live" } else { "stale" };
            writeln!(
                out,
                "object {state} {}",
                crate::hex(&bcs::to_bytes(object).unwrap())
            )
            .unwrap();
        }
    }
    for (label, tx) in cases(&w) {
        let verdict = verdict(&store, &config, &tx);
        writeln!(
            out,
            "case {label} {} {verdict}",
            crate::hex(&bcs::to_bytes(&tx).unwrap())
        )
        .unwrap();
    }
    let mut gz = flate2::write::GzEncoder::new(vec![], flate2::Compression::best());
    std::io::Write::write_all(&mut gz, out.as_bytes()).unwrap();
    gz.finish().unwrap()
}
