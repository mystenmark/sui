// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The stateful input checks a validator makes before voting for a
//! transaction, in the reference's order: loading the inputs
//! (`TransactionInputLoader::read_objects_for_signing`), the checks of
//! `sui-transaction-checks`' `check_transaction_input`, and that owned
//! inputs are live (`validate_owned_object_versions`). Objects come from an
//! [`Objects`] source, the node's store.
//!
//! Not checked, and refused as unsupported instead where they matter:
//! address balances (fund withdrawals, coin reservations, gasless
//! transactions). Not checked: the operator's deny config and the coin
//! deny list, the bytecode verification of packages to be published
//! (execution verifies them), and previously received objects (the
//! reference's markers).

use std::collections::BTreeSet;

use messages::Message;
use messages::base::{ObjectDigest, ObjectId, ObjectRef, SuiAddress};
use messages::object::{Data, MoveObjectType, Object, Owner};
use messages::transaction::{
    CallArg, Command, ObjectArg, SenderSignedData, SharedObjectArg, SharedObjectMutability,
    TransactionData, TransactionKind, TxState,
};
use messages::type_tag::TypeTag;

use crate::{Context, Error, ErrorKind, transaction_data};

/// Where the checks read objects. Reads cannot fail: a store that cannot
/// read has nothing a check could do about it, so it panics.
pub trait Objects {
    /// The object's live version.
    fn live(&self, id: &ObjectId) -> Option<Message<Object<'static>>>;

    /// The object at exactly `version`, live or not.
    fn at(&self, id: &ObjectId, version: u64) -> Option<Message<Object<'static>>>;

    /// The live version's number and digest, without reading the object.
    fn live_ref(&self, id: &ObjectId) -> Option<(u64, ObjectDigest)> {
        self.live(id).map(|o| (o.get().version(), o.get().digest()))
    }
}

/// One input object, as the reference's `InputObjectKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind<'a> {
    Package(ObjectId),
    ImmOrOwned(&'a ObjectRef),
    Shared(&'a SharedObjectArg),
}

impl InputKind<'_> {
    fn id(&self) -> &ObjectId {
        match self {
            InputKind::Package(id) => id,
            InputKind::ImmOrOwned(r) => &r.id,
            InputKind::Shared(s) => &s.id,
        }
    }
}

fn error(kind: ErrorKind, detail: impl Into<String>) -> Error {
    Error::new(kind, detail)
}

fn not_found(id: &ObjectId, version: Option<u64>) -> Error {
    error(
        ErrorKind::ObjectNotFound,
        format!("{id:?} at {version:?} not found"),
    )
}

/// The reference's `TransactionData::input_objects`: the inputs' objects, in
/// order and none twice; then the packages commands name, in id order; then
/// the gas coins. Coin reservations and receiving objects are not inputs.
pub fn input_objects<'a>(
    data: &TransactionData<'a, impl TxState>,
) -> Result<Vec<InputKind<'a>>, Error> {
    let TransactionKind::ProgrammableTransaction(pt) = data.kind() else {
        return Ok(vec![]);
    };
    let mut inputs = Vec::new();
    for arg in pt.inputs {
        match arg {
            CallArg::Object(ObjectArg::ImmOrOwnedObject(r)) if !r.is_coin_reservation() => {
                inputs.push(InputKind::ImmOrOwned(r));
            }
            CallArg::Object(ObjectArg::SharedObject(s)) => inputs.push(InputKind::Shared(s)),
            _ => {}
        }
    }
    let mut used = BTreeSet::new();
    if !inputs.iter().all(|i| used.insert(i.id().0)) {
        return Err(error(
            ErrorKind::DuplicateObjectRefInput,
            "an object is input twice",
        ));
    }
    let mut packages: BTreeSet<ObjectId> = BTreeSet::new();
    for command in pt.commands {
        match command {
            Command::MoveCall(call) => {
                packages.insert(*call.package);
                for t in call.type_arguments {
                    type_packages(t, &mut packages);
                }
            }
            Command::Publish(_, deps) => packages.extend(deps.iter().copied()),
            Command::Upgrade(_, deps, package, _) => {
                packages.extend(deps.iter().copied());
                packages.insert(**package);
            }
            Command::MakeMoveVec(Some(t), _) => type_packages(t, &mut packages),
            _ => {}
        }
    }
    inputs.extend(packages.into_iter().map(InputKind::Package));
    inputs.extend(
        data.gas_data()
            .payment
            .iter()
            .filter(|r| !r.is_coin_reservation())
            .map(InputKind::ImmOrOwned),
    );
    Ok(inputs)
}

/// Every package a type names.
fn type_packages(t: &TypeTag<'_>, packages: &mut BTreeSet<ObjectId>) {
    match t {
        TypeTag::Vector(inner) => type_packages(inner, packages),
        TypeTag::Struct(s) => {
            // An address and an object id are the same 32 bytes.
            packages.insert(ObjectId(s.address.0));
            for p in s.type_params {
                type_packages(p, packages);
            }
        }
        _ => {}
    }
}

/// The objects the transaction receives.
pub fn receiving_objects<'a>(data: &TransactionData<'a, impl TxState>) -> Vec<&'a ObjectRef> {
    let TransactionKind::ProgrammableTransaction(pt) = data.kind() else {
        return vec![];
    };
    pt.inputs
        .iter()
        .filter_map(|arg| match arg {
            CallArg::Object(ObjectArg::Receiving(r)) => Some(*r),
            _ => None,
        })
        .collect()
}

/// A loaded input.
struct Input<'a> {
    kind: InputKind<'a>,
    object: Message<Object<'static>>,
    /// An owned input's live version and digest, read when it was loaded.
    live: Option<(u64, ObjectDigest)>,
}

impl Input<'_> {
    fn object(&self) -> &Object<'_> {
        self.object.get()
    }

    /// The object's digest: the live one's if it is live, which spares
    /// hashing it.
    fn digest(&self) -> ObjectDigest {
        match self.live {
            Some((version, digest)) if version == self.object().version() => digest,
            _ => self.object().digest(),
        }
    }

    /// The reference's `ObjectReadResult::is_mutable`.
    fn is_mutable(&self) -> bool {
        match self.kind {
            InputKind::Package(_) => false,
            InputKind::ImmOrOwned(_) => !matches!(self.object().owner, Owner::Immutable),
            InputKind::Shared(s) => s.mutability() == SharedObjectMutability::Mutable,
        }
    }

    /// The reference's `get_address_owned_objref`: owned, and not immutable.
    fn address_owned(&self) -> Option<&ObjectRef> {
        match self.kind {
            InputKind::ImmOrOwned(r) if !matches!(self.object().owner, Owner::Immutable) => Some(r),
            _ => None,
        }
    }
}

/// Everything above, for one transaction that passed validation and
/// signature verification.
pub fn check(
    signed: &SenderSignedData<'_, impl TxState>,
    ctx: &Context<'_>,
    objects: &impl Objects,
) -> Result<(), Error> {
    let data = signed.data();
    refuse_address_balances(data, ctx)?;
    let kinds = input_objects(data)?;
    let receiving = receiving_objects(data);
    let inputs = load(&kinds, objects)?;
    let received = load_receiving(&receiving, objects)?;

    check_gas(data, ctx, &inputs)?;
    check_objects(data, &inputs)?;
    check_replay_protection(data, &inputs)?;
    check_receiving_objects(&inputs, &receiving, &received)?;
    check_owned_objects_live(&inputs)
}

/// Address balances are not kept yet, so nothing that draws on them can be
/// checked: refused rather than passed unchecked.
fn refuse_address_balances(
    data: &TransactionData<'_, impl TxState>,
    ctx: &Context<'_>,
) -> Result<(), Error> {
    let unsupported = |what: &str| {
        Err(error(
            ErrorKind::Unsupported,
            format!("{what}: address balances are not supported yet"),
        ))
    };
    if transaction_data::is_gasless(data, ctx) {
        return unsupported("a gasless transaction");
    }
    if data.gas_data().payment.is_empty()
        || data
            .gas_data()
            .payment
            .iter()
            .any(ObjectRef::is_coin_reservation)
    {
        return unsupported("gas from an address balance");
    }
    if let TransactionKind::ProgrammableTransaction(pt) = data.kind() {
        for arg in pt.inputs {
            match arg {
                CallArg::FundsWithdrawal(_) => return unsupported("a funds withdrawal"),
                CallArg::Object(ObjectArg::ImmOrOwnedObject(r)) if r.is_coin_reservation() => {
                    return unsupported("a coin reservation");
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// `read_objects_for_signing`: packages and shared objects one at a time,
/// in order; then the owned objects, whose errors say more.
fn load<'a>(kinds: &[InputKind<'a>], objects: &impl Objects) -> Result<Vec<Input<'a>>, Error> {
    let mut loaded: Vec<Option<Input<'a>>> = kinds.iter().map(|_| None).collect();
    for (i, kind) in kinds.iter().enumerate() {
        let object = match kind {
            InputKind::Package(id) => {
                let Some(object) = objects.live(id) else {
                    return Err(error(
                        ErrorKind::DependentPackageNotFound,
                        format!("package {id:?} not found"),
                    ));
                };
                if !matches!(object.get().data, Data::Package(_)) {
                    return Err(error(
                        ErrorKind::MoveObjectAsPackage,
                        format!("{id:?} is not a package"),
                    ));
                }
                object
            }
            // The live version, if it is the consensus object the input
            // names (same id and start version).
            InputKind::Shared(s) => match objects.live(&s.id) {
                Some(object)
                    if start_version(&object.get().owner)
                        == Some(s.initial_shared_version.get()) =>
                {
                    object
                }
                _ => return Err(not_found(&s.id, None)),
            },
            InputKind::ImmOrOwned(_) => continue,
        };
        loaded[i] = Some(Input {
            kind: *kind,
            object,
            live: None,
        });
    }
    for (i, kind) in kinds.iter().enumerate() {
        let InputKind::ImmOrOwned(r) = kind else {
            continue;
        };
        let version = r.version.get();
        let live = objects.live_ref(&r.id);
        let Some(object) = objects.at(&r.id, version) else {
            let Some((current, _)) = live else {
                return Err(not_found(&r.id, None));
            };
            return Err(if current >= version {
                error(
                    ErrorKind::ObjectVersionUnavailableForConsumption,
                    format!("{:?} at {version}: current version {current}", r.id),
                )
            } else {
                not_found(&r.id, Some(version))
            });
        };
        loaded[i] = Some(Input {
            kind: *kind,
            object,
            live,
        });
    }
    Ok(loaded
        .into_iter()
        .map(|i| i.expect("every input loaded"))
        .collect())
}

/// The version a consensus object's stream starts at; none for other owners.
fn start_version(owner: &Owner<'_>) -> Option<u64> {
    match owner {
        Owner::Shared {
            initial_shared_version,
        } => Some(*initial_shared_version),
        Owner::ConsensusAddressOwner { start_version, .. } => Some(*start_version),
        Owner::Party(party) => Some(party.start_version),
        _ => None,
    }
}

/// `read_receiving_objects_for_signing`: each live object, or none.
fn load_receiving(
    receiving: &[&ObjectRef],
    objects: &impl Objects,
) -> Result<Vec<Message<Object<'static>>>, Error> {
    receiving
        .iter()
        .map(|r| {
            objects
                .live(&r.id)
                .ok_or_else(|| not_found(&r.id, Some(r.version.get())))
        })
        .collect()
}

/// `check_gas`: the price, then the gas coins' owners and balance. (The
/// budget's bounds passed validation already, under the same config.)
fn check_gas(
    data: &TransactionData<'_, impl TxState>,
    ctx: &Context<'_>,
    inputs: &[Input<'_>],
) -> Result<(), Error> {
    let gas = data.gas_data();
    transaction_data::check_gas_price(gas.price, ctx)?;
    let coins: Vec<&Input<'_>> = gas
        .payment
        .iter()
        .map(|r| {
            inputs
                .iter()
                .find(|i| i.kind.id() == &r.id)
                .expect("gas coins are inputs")
        })
        .collect();
    // `check_gas_objects`, which `check_gas_balance` repeats.
    for coin in &coins {
        if !matches!(coin.object().owner, Owner::AddressOwner(_)) {
            return Err(error(
                ErrorKind::GasObjectNotOwnedObject,
                format!("gas coin {:?} is not address-owned", coin.kind.id()),
            ));
        }
    }
    let mut balance: u128 = 0;
    for coin in &coins {
        let object = coin.object();
        let value = match object.data {
            Data::Move(m) if m.type_ == MoveObjectType::GasCoin && m.contents.len() >= 40 => {
                u64::from_le_bytes(m.contents[32..40].try_into().expect("eight bytes"))
            }
            _ => {
                return Err(error(
                    ErrorKind::InvalidGasObject,
                    format!("{:?} is not a SUI coin", coin.kind.id()),
                ));
            }
        };
        balance += u128::from(value);
    }
    if balance < u128::from(gas.budget) {
        return Err(error(
            ErrorKind::GasBalanceTooLow,
            format!("gas balance {balance} under budget {}", gas.budget),
        ));
    }
    Ok(())
}

/// `check_objects`: no mutable object twice, at least one input, and each
/// object is what its input says.
fn check_objects(
    data: &TransactionData<'_, impl TxState>,
    inputs: &[Input<'_>],
) -> Result<(), Error> {
    let mut mutable = BTreeSet::new();
    for input in inputs {
        if input.is_mutable() && !mutable.insert(input.kind.id().0) {
            return Err(error(
                ErrorKind::MutableObjectUsedMoreThanOnce,
                format!("{:?} is used mutably twice", input.kind.id()),
            ));
        }
    }
    if inputs.is_empty() {
        return Err(error(
            ErrorKind::ObjectInputArityViolation,
            "no input objects",
        ));
    }
    let gas = data.gas_data();
    for input in inputs {
        let owner = if gas.payment.iter().any(|r| &r.id == input.kind.id()) {
            gas.owner
        } else {
            data.sender()
        };
        check_one_object(owner, input)?;
    }
    Ok(())
}

/// `check_one_object`.
fn check_one_object(owner: &SuiAddress, input: &Input<'_>) -> Result<(), Error> {
    let object = input.object();
    let id = input.kind.id();
    if matches!(object.owner, Owner::Party(_)) {
        return Err(error(
            ErrorKind::Unsupported,
            "party objects are not supported yet",
        ));
    }
    let is_package = matches!(object.data, Data::Package(_));
    match input.kind {
        InputKind::Package(_) => {
            if !is_package {
                return Err(error(
                    ErrorKind::MoveObjectAsPackage,
                    format!("{id:?} is not a package"),
                ));
            }
        }
        InputKind::ImmOrOwned(r) => {
            if is_package {
                return Err(error(
                    ErrorKind::MovePackageAsObject,
                    format!("{id:?} is a package"),
                ));
            }
            if r.version.get() == u64::MAX {
                return Err(error(ErrorKind::InvalidSequenceNumber, "version u64::MAX"));
            }
            if input.digest() != r.digest {
                return Err(error(
                    ErrorKind::InvalidObjectDigest,
                    format!("{id:?}: digest mismatch"),
                ));
            }
            match object.owner {
                Owner::Immutable => {}
                Owner::AddressOwner(actual) => {
                    if actual != owner {
                        return Err(incorrect_signer(id, actual, owner));
                    }
                }
                Owner::ObjectOwner(parent) => {
                    return Err(error(
                        ErrorKind::InvalidChildObjectArgument,
                        format!("{id:?} is a child of {parent:?}"),
                    ));
                }
                Owner::Shared { .. } | Owner::ConsensusAddressOwner { .. } | Owner::Party(_) => {
                    return Err(error(
                        ErrorKind::NotOwnedObjectError,
                        format!("{id:?} is a consensus object"),
                    ));
                }
            }
        }
        InputKind::Shared(s) => check_shared(owner, id, object, s)?,
    }
    Ok(())
}

fn check_shared(
    owner: &SuiAddress,
    id: &ObjectId,
    object: &Object<'_>,
    s: &SharedObjectArg,
) -> Result<(), Error> {
    if object.version() == u64::MAX {
        return Err(error(ErrorKind::InvalidSequenceNumber, "version u64::MAX"));
    }
    if is_system_object(id) && !system_object_usable(id, s.mutability()) {
        return Err(error(
            ErrorKind::ImmutableParameterExpectedError,
            format!("system object {id:?} cannot be used so"),
        ));
    }
    match object.owner {
        Owner::AddressOwner(_) | Owner::ObjectOwner(_) | Owner::Immutable => {
            return Err(error(
                ErrorKind::NotSharedObjectError,
                format!("{id:?} is not shared"),
            ));
        }
        Owner::Shared {
            initial_shared_version,
        } => {
            if initial_shared_version != s.initial_shared_version.get() {
                return Err(error(
                    ErrorKind::SharedObjectStartingVersionMismatch,
                    format!("{id:?}"),
                ));
            }
        }
        Owner::ConsensusAddressOwner {
            start_version,
            owner: actual,
        } => {
            if start_version != s.initial_shared_version.get() {
                return Err(error(
                    ErrorKind::SharedObjectStartingVersionMismatch,
                    format!("{id:?}"),
                ));
            }
            if actual != owner {
                return Err(incorrect_signer(id, actual, owner));
            }
        }
        Owner::Party(_) => unreachable!("check_one_object refuses party objects"),
    }
    Ok(())
}

fn incorrect_signer(id: &ObjectId, actual: &SuiAddress, given: &SuiAddress) -> Error {
    error(
        ErrorKind::IncorrectUserSignature,
        format!("{id:?} is owned by {actual:?}, not {given:?}"),
    )
}

/// The reference's `ObjectID::is_system_object`: the first 24 bytes are zero.
fn is_system_object(id: &ObjectId) -> bool {
    id.0[..24] == [0; 24]
}

/// Which system objects a user transaction may take as a shared input, and
/// how.
fn system_object_usable(id: &ObjectId, mutability: SharedObjectMutability) -> bool {
    const SYSTEM_STATE: u16 = 0x5;
    const CLOCK: u16 = 0x6;
    const RANDOMNESS: u16 = 0x8;
    const BRIDGE: u16 = 0x9;
    const COIN_REGISTRY: u16 = 0xc;
    const DISPLAY_REGISTRY: u16 = 0xd;
    const ADDRESS_ALIAS_STATE: u16 = 0xa;
    const DENY_LIST: u16 = 0x403;
    const ACCUMULATOR_ROOT: u16 = 0xacc;
    let is = |n: u16| *id == ObjectId::from_u16(n);
    let immutable = mutability == SharedObjectMutability::Immutable;
    [
        SYSTEM_STATE,
        ADDRESS_ALIAS_STATE,
        COIN_REGISTRY,
        DISPLAY_REGISTRY,
        DENY_LIST,
        BRIDGE,
    ]
    .into_iter()
    .any(is)
        || (immutable && [CLOCK, RANDOMNESS, ACCUMULATOR_ROOT].into_iter().any(is))
}

/// `check_replay_protection`: a transaction must not be replayable, which
/// gas coins (owned, so consumed) prevent. (A `ValidDuring` expiration
/// would too.)
fn check_replay_protection(
    data: &TransactionData<'_, impl TxState>,
    inputs: &[Input<'_>],
) -> Result<(), Error> {
    if transaction_data::is_replay_protected(data.expiration())
        || !data.gas_data().payment.is_empty()
        || inputs.iter().any(|i| i.address_owned().is_some())
    {
        Ok(())
    } else {
        Err(error(
            ErrorKind::InvalidExpiration,
            "no owned inputs and no ValidDuring expiration of at most two epochs",
        ))
    }
}

/// `check_receiving_objects`: each received object is owned, at the version
/// and digest given, and not also an input.
fn check_receiving_objects(
    inputs: &[Input<'_>],
    receiving: &[&ObjectRef],
    received: &[Message<Object<'static>>],
) -> Result<(), Error> {
    let mut seen: BTreeSet<[u8; 32]> = inputs.iter().map(|i| i.kind.id().0).collect();
    for (i, r) in receiving.iter().enumerate() {
        let version = r.version.get();
        if version == u64::MAX {
            return Err(error(ErrorKind::InvalidSequenceNumber, "version u64::MAX"));
        }
        let object = received[i].get();
        let fine = matches!(object.owner, Owner::AddressOwner(_))
            && object.version() == version
            && object.digest() == r.digest;
        if !fine {
            if object.version() != version {
                return Err(error(
                    ErrorKind::ObjectVersionUnavailableForConsumption,
                    format!(
                        "{:?} at {version}: current version {}",
                        r.id,
                        object.version()
                    ),
                ));
            }
            if matches!(object.data, Data::Package(_)) {
                return Err(error(
                    ErrorKind::MovePackageAsObject,
                    format!("{:?} is a package", r.id),
                ));
            }
            if object.digest() != r.digest {
                return Err(error(
                    ErrorKind::InvalidObjectDigest,
                    format!("{:?}: digest mismatch", r.id),
                ));
            }
            return Err(match object.owner {
                Owner::AddressOwner(_) => unreachable!("fine, then"),
                Owner::ObjectOwner(parent) => error(
                    ErrorKind::InvalidChildObjectArgument,
                    format!("{:?} is a child of {parent:?}", r.id),
                ),
                Owner::Shared { .. } | Owner::ConsensusAddressOwner { .. } | Owner::Party(_) => {
                    error(
                        ErrorKind::NotSharedObjectError,
                        format!("{:?} is a consensus object", r.id),
                    )
                }
                Owner::Immutable => error(
                    ErrorKind::MutableParameterExpected,
                    format!("{:?} is immutable", r.id),
                ),
            });
        }
        if !seen.insert(r.id.0) {
            return Err(error(
                ErrorKind::DuplicateObjectRefInput,
                format!("{:?} is input twice", r.id),
            ));
        }
    }
    Ok(())
}

/// `validate_owned_object_versions`: every address-owned input is live at
/// the version and digest given, as read when it was loaded. (Owned-object
/// locks are taken after consensus.)
fn check_owned_objects_live(inputs: &[Input<'_>]) -> Result<(), Error> {
    let owned = || {
        inputs
            .iter()
            .filter_map(|input| input.address_owned().map(|r| (r, input.live)))
    };
    if let Some((r, _)) = owned().find(|(_, live)| live.is_none()) {
        return Err(not_found(&r.id, None));
    }
    for (r, live) in owned() {
        let (version, digest) = live.expect("checked above");
        if version != r.version.get() {
            return Err(error(
                ErrorKind::ObjectVersionUnavailableForConsumption,
                format!(
                    "{:?} at {}: current version {version}",
                    r.id,
                    r.version.get(),
                ),
            ));
        }
        if digest != r.digest {
            return Err(error(
                ErrorKind::InvalidObjectDigest,
                format!("{:?}: digest mismatch", r.id),
            ));
        }
    }
    Ok(())
}
