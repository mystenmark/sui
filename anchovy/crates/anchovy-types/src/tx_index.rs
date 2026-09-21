// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! What a transaction touches, gathered once while it is parsed so that
//! every later reader gets a slice instead of walking the commands.
//!
//! The reference computes these on demand; `docs/REFERENCE_STRICTNESS.md`
//! section (a) records its rules. Where it reports an error (the same object
//! named twice), this index keeps both entries and leaves the rejection to
//! validation.

use crate::arena::{Alloc, Ref, SliceWriter};
use crate::base::{ObjectId, ObjectRef};
use crate::error::Result;
use crate::system_transaction::EndOfEpochTransactionKind;
use crate::transaction::{
    CallArg, Command, FundsWithdrawalArg, GasData, ObjectArg, ProgrammableTransaction,
    SharedObjectArg, SharedObjectMutability, TransactionKind,
};
use crate::type_tag::TypeInput;

pub const SUI_SYSTEM_STATE_OBJECT_ID: ObjectId = ObjectId::from_u16(0x5);
pub const SUI_CLOCK_OBJECT_ID: ObjectId = ObjectId::from_u16(0x6);
pub const SUI_AUTHENTICATOR_STATE_OBJECT_ID: ObjectId = ObjectId::from_u16(0x7);
pub const SUI_RANDOMNESS_STATE_OBJECT_ID: ObjectId = ObjectId::from_u16(0x8);
pub const SUI_BRIDGE_OBJECT_ID: ObjectId = ObjectId::from_u16(0x9);

const SUI_SYSTEM_STATE: SharedObjectArg = SharedObjectArg::new(
    SUI_SYSTEM_STATE_OBJECT_ID,
    1,
    SharedObjectMutability::Mutable,
);
const SUI_CLOCK: SharedObjectArg =
    SharedObjectArg::new(SUI_CLOCK_OBJECT_ID, 1, SharedObjectMutability::Mutable);

impl ObjectRef {
    /// Whether this names an address balance reservation rather than an
    /// object: the digest's last twenty bytes are all `0xac`.
    pub fn is_coin_reservation(&self) -> bool {
        self.digest.bytes[12..] == [0xac; 20]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TransactionIndex<'a> {
    /// Every shared object input in input order, repeats included. System
    /// transactions name the system objects they write.
    pub shared_inputs: &'a [SharedObjectArg],
    /// Owned and immutable object inputs in input order, then the gas
    /// payment of a user transaction. Coin reservations are left out.
    pub owned_inputs: &'a [ObjectRef],
    /// Every package a command calls into, names in a type argument, or
    /// depends on, in increasing order without repeats.
    pub packages: &'a [ObjectId],
    /// The rest are empty unless the kind is `ProgrammableTransaction`.
    pub receiving: &'a [ObjectRef],
    /// Indices into `commands` of the `MoveCall`s.
    pub move_calls: &'a [u32],
    pub funds_withdrawals: &'a [Ref<'a, FundsWithdrawalArg<'a>>],
    /// Coin reservations among the inputs, then among the gas payment.
    pub coin_reservations: &'a [ObjectRef],
}

/// How many entries each index slice needs. Gathered while parsing, from
/// values that exist in both passes: enum variants and wire-backed slices.
#[derive(Default)]
pub(crate) struct IndexCounts {
    shared: usize,
    owned: usize,
    /// An upper bound: repeats are only found once the ids are sorted.
    packages: usize,
    receiving: usize,
    move_calls: usize,
    funds_withdrawals: usize,
    coin_reservations: usize,
}

impl IndexCounts {
    pub(crate) fn count_input(&mut self, arg: &CallArg<'_>) {
        match arg {
            CallArg::Pure(_) => {}
            CallArg::Object(ObjectArg::ImmOrOwnedObject(o)) => {
                if o.is_coin_reservation() {
                    self.coin_reservations += 1;
                } else {
                    self.owned += 1;
                }
            }
            CallArg::Object(ObjectArg::SharedObject(_)) => self.shared += 1,
            CallArg::Object(ObjectArg::Receiving(_)) => self.receiving += 1,
            CallArg::FundsWithdrawal(_) => self.funds_withdrawals += 1,
        }
    }

    /// `struct_tags` is how many struct tags the command's type arguments
    /// hold, at any depth: each names a package.
    pub(crate) fn count_command(&mut self, command: &Command<'_>, struct_tags: usize) {
        self.packages += match command {
            Command::MoveCall(_) => {
                self.move_calls += 1;
                1 + struct_tags
            }
            Command::Publish(_, dependencies) => dependencies.len(),
            Command::MakeMoveVec(..) => struct_tags,
            Command::Upgrade(_, dependencies, _, _) => dependencies.len() + 1,
            Command::TransferObjects(..) | Command::SplitCoins(..) | Command::MergeCoins(..) => 0,
        };
    }

    pub(crate) fn count_end_of_epoch(&mut self, kind: &EndOfEpochTransactionKind<'_>) {
        self.shared += match kind {
            EndOfEpochTransactionKind::ChangeEpoch(_)
            | EndOfEpochTransactionKind::AuthenticatorStateExpire { .. }
            | EndOfEpochTransactionKind::StoreExecutionTimeObservations(_)
            | EndOfEpochTransactionKind::WriteAccumulatorStorageCost { .. } => 1,
            EndOfEpochTransactionKind::BridgeCommitteeInit(_) => 2,
            EndOfEpochTransactionKind::AuthenticatorStateCreate
            | EndOfEpochTransactionKind::RandomnessStateCreate
            | EndOfEpochTransactionKind::DenyListStateCreate
            | EndOfEpochTransactionKind::BridgeStateCreate(_)
            | EndOfEpochTransactionKind::AccumulatorRootCreate
            | EndOfEpochTransactionKind::CoinRegistryCreate
            | EndOfEpochTransactionKind::DisplayRegistryCreate
            | EndOfEpochTransactionKind::AddressAliasStateCreate
            | EndOfEpochTransactionKind::ForwardingAddressRegistryCreate => 0,
        };
    }
}

fn push_end_of_epoch(
    kind: &EndOfEpochTransactionKind<'_>,
    shared: &mut SliceWriter<'_, SharedObjectArg>,
) {
    match *kind {
        EndOfEpochTransactionKind::ChangeEpoch(_)
        | EndOfEpochTransactionKind::StoreExecutionTimeObservations(_)
        | EndOfEpochTransactionKind::WriteAccumulatorStorageCost { .. } => {
            shared.push(SUI_SYSTEM_STATE);
        }
        EndOfEpochTransactionKind::AuthenticatorStateExpire {
            authenticator_obj_initial_shared_version,
            ..
        } => shared.push(SharedObjectArg::new(
            SUI_AUTHENTICATOR_STATE_OBJECT_ID,
            authenticator_obj_initial_shared_version,
            SharedObjectMutability::Mutable,
        )),
        EndOfEpochTransactionKind::BridgeCommitteeInit(version) => {
            shared.push(SharedObjectArg::new(
                SUI_BRIDGE_OBJECT_ID,
                version,
                SharedObjectMutability::Mutable,
            ));
            shared.push(SUI_SYSTEM_STATE);
        }
        EndOfEpochTransactionKind::AuthenticatorStateCreate
        | EndOfEpochTransactionKind::RandomnessStateCreate
        | EndOfEpochTransactionKind::DenyListStateCreate
        | EndOfEpochTransactionKind::BridgeStateCreate(_)
        | EndOfEpochTransactionKind::AccumulatorRootCreate
        | EndOfEpochTransactionKind::CoinRegistryCreate
        | EndOfEpochTransactionKind::DisplayRegistryCreate
        | EndOfEpochTransactionKind::AddressAliasStateCreate
        | EndOfEpochTransactionKind::ForwardingAddressRegistryCreate => {}
    }
}

fn push_type_packages(ty: &TypeInput<'_>, packages: &mut SliceWriter<'_, ObjectId>) {
    match ty {
        TypeInput::Vector(inner) => push_type_packages(inner, packages),
        TypeInput::Struct(s) => {
            packages.push_unless_repeat(ObjectId(s.address.0));
            for param in s.type_params {
                push_type_packages(param, packages);
            }
        }
        _ => {}
    }
}

fn push_command_packages(command: &Command<'_>, packages: &mut SliceWriter<'_, ObjectId>) {
    match command {
        Command::MoveCall(call) => {
            packages.push_unless_repeat(*call.package);
            for ty in call.type_arguments {
                push_type_packages(ty, packages);
            }
        }
        Command::Publish(_, dependencies) => {
            for id in *dependencies {
                packages.push(*id);
            }
        }
        Command::MakeMoveVec(Some(ty), _) => push_type_packages(ty, packages),
        Command::Upgrade(_, dependencies, package, _) => {
            for id in *dependencies {
                packages.push(*id);
            }
            packages.push(**package);
        }
        Command::TransferObjects(..)
        | Command::SplitCoins(..)
        | Command::MergeCoins(..)
        | Command::MakeMoveVec(None, _) => {}
    }
}

impl<'a> TransactionIndex<'a> {
    // Long because it is kept as one sequence of steps over shared state.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn build<A: Alloc<'a>>(
        kind: &TransactionKind<'a>,
        gas_data: &GasData<'a>,
        mut counts: IndexCounts,
        a: &mut A,
    ) -> Result<TransactionIndex<'a>> {
        // Step 1: settle the counts that depend on the kind. Only a user
        // transaction pays gas with objects or reports the last four slices.
        let user_pt: Option<&ProgrammableTransaction<'a>> = match kind {
            TransactionKind::ProgrammableTransaction(pt) => Some(pt),
            _ => None,
        };
        if user_pt.is_some() {
            for o in gas_data.payment {
                if o.is_coin_reservation() {
                    counts.coin_reservations += 1;
                } else {
                    counts.owned += 1;
                }
            }
        } else {
            counts.receiving = 0;
            counts.move_calls = 0;
            counts.funds_withdrawals = 0;
            counts.coin_reservations = 0;
        }
        counts.shared += match kind {
            TransactionKind::ChangeEpoch(_)
            | TransactionKind::ConsensusCommitPrologue(_)
            | TransactionKind::ConsensusCommitPrologueV2(_)
            | TransactionKind::ConsensusCommitPrologueV3(_)
            | TransactionKind::ConsensusCommitPrologueV4(_)
            | TransactionKind::AuthenticatorStateUpdate(_)
            | TransactionKind::RandomnessStateUpdate(_) => 1,
            TransactionKind::ProgrammableTransaction(_)
            | TransactionKind::ProgrammableSystemTransaction(_)
            | TransactionKind::Genesis(_)
            | TransactionKind::EndOfEpochTransaction(_) => 0,
        };

        // Step 2: reserve, identically in both passes.
        let mut shared = a.slice(counts.shared)?;
        let mut owned = a.slice(counts.owned)?;
        let mut packages = a.slice(counts.packages)?;
        let mut receiving = a.slice(counts.receiving)?;
        let mut move_calls = a.slice(counts.move_calls)?;
        let mut funds_withdrawals = a.slice(counts.funds_withdrawals)?;
        let mut coin_reservations = a.slice(counts.coin_reservations)?;

        // Step 3: fill. The measure pass has no parsed values to read.
        if A::BUILD {
            match kind {
                TransactionKind::ProgrammableTransaction(pt)
                | TransactionKind::ProgrammableSystemTransaction(pt) => {
                    for input in pt.inputs {
                        match *input {
                            CallArg::Pure(_) => {}
                            CallArg::Object(ObjectArg::ImmOrOwnedObject(o)) => {
                                if !o.is_coin_reservation() {
                                    owned.push(*o);
                                } else if user_pt.is_some() {
                                    coin_reservations.push(*o);
                                }
                            }
                            CallArg::Object(ObjectArg::SharedObject(s)) => shared.push(*s),
                            CallArg::Object(ObjectArg::Receiving(o)) => {
                                if user_pt.is_some() {
                                    receiving.push(*o);
                                }
                            }
                            CallArg::FundsWithdrawal(w) => {
                                if user_pt.is_some() {
                                    funds_withdrawals.push(w);
                                }
                            }
                        }
                    }
                    for (i, command) in pt.commands.iter().enumerate() {
                        push_command_packages(command, &mut packages);
                        if user_pt.is_some() && matches!(command, Command::MoveCall(_)) {
                            move_calls.push(i as u32);
                        }
                    }
                    packages.sort_dedup();
                }
                TransactionKind::ChangeEpoch(_) => shared.push(SUI_SYSTEM_STATE),
                TransactionKind::Genesis(_) => {}
                TransactionKind::ConsensusCommitPrologue(_)
                | TransactionKind::ConsensusCommitPrologueV2(_)
                | TransactionKind::ConsensusCommitPrologueV3(_)
                | TransactionKind::ConsensusCommitPrologueV4(_) => shared.push(SUI_CLOCK),
                TransactionKind::AuthenticatorStateUpdate(update) => {
                    shared.push(SharedObjectArg::new(
                        SUI_AUTHENTICATOR_STATE_OBJECT_ID,
                        update.authenticator_obj_initial_shared_version,
                        SharedObjectMutability::Mutable,
                    ));
                }
                TransactionKind::RandomnessStateUpdate(update) => {
                    shared.push(SharedObjectArg::new(
                        SUI_RANDOMNESS_STATE_OBJECT_ID,
                        update.randomness_obj_initial_shared_version,
                        SharedObjectMutability::Mutable,
                    ));
                }
                TransactionKind::EndOfEpochTransaction(kinds) => {
                    for kind in *kinds {
                        push_end_of_epoch(kind, &mut shared);
                    }
                }
            }
            if user_pt.is_some() {
                for o in gas_data.payment {
                    if o.is_coin_reservation() {
                        coin_reservations.push(*o);
                    } else {
                        owned.push(*o);
                    }
                }
            }
        }

        Ok(TransactionIndex {
            shared_inputs: shared.finish(),
            owned_inputs: owned.finish(),
            packages: packages.finish(),
            receiving: receiving.finish(),
            move_calls: move_calls.finish(),
            funds_withdrawals: funds_withdrawals.finish(),
            coin_reservations: coin_reservations.finish(),
        })
    }
}
