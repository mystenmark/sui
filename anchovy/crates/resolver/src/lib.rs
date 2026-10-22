// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Turning a chunk's transactions into the keys their inputs wait on
//! (`docs/consensus_pipeline.md`, Execution).

use std::sync::Arc;

use containers::Bump;
use executor::accumulator_root::{balance_type, get_field_id, sui_balance_type};
use messages::Message;
use messages::base::{ChainIdentifier, ObjectId};
use messages::object::Object;
use messages::transaction::{CallArg, Command as MoveCommand, ObjectArg, TransactionKind};
use messages::transaction::{Reservation, TransactionData, TxState};
use messages::transaction::{WithdrawFrom, WithdrawalTypeArg};
use messages::type_tag::TypeTag;
use validator::consensus::sequence::SharedAccess;
use validator::epoch::EpochParams;
use validator::pipeline::items::{
    Chunk, Executable, ExecutableTransaction, FundsCommand, InputCommand, InputValue,
    ScheduledEntry, Withdrawal,
};
use validator::pipeline::keys::{self, InputKey, StreamId};
use validator::pipeline::tables::{Generation, GenerationTable};
use waiter::{Availability, Command, WaitBatch};
use workqueue::{Processor, PushError, Queue};

/// Turns each scheduled transaction's inputs into input waiter keys:
/// `ObjectVersion` for owned, immutable and receiving objects; `Package` for
/// packages; `Generation` for a shared access at generation `k > 0`; for
/// generation 0, the base the commit handler linked it to, the stream's last
/// generation from an earlier round, or `StreamLive` if none.
///
/// Inward:
/// - `CommitStage`: `Chunk`.
///
/// Outward:
/// - input waiter, wait for: `Executable`, keyed by its inputs, and by
///   `Reserved(digest)` if it withdraws funds;
/// - `FundsReserver`: `FundsCommand::Reserve` of each user transaction that
///   withdraws funds, a `Reservation` per account against the chunk's
///   version, in consensus order.
pub struct VersionResolver {
    epoch: Arc<EpochParams>,
    pub inputs: Queue<InputCommand>,
    pub withdrawals: Queue<FundsCommand>,
    /// Scratch: one transaction's keys, its packages, and its withdrawals.
    keys: Vec<InputKey>,
    packages: Vec<ObjectId>,
    amounts: Vec<(ObjectId, u64)>,
}

impl VersionResolver {
    pub fn new(
        epoch: Arc<EpochParams>,
        inputs: Queue<InputCommand>,
        withdrawals: Queue<FundsCommand>,
    ) -> VersionResolver {
        VersionResolver {
            epoch,
            inputs,
            withdrawals,
            keys: Vec::new(),
            packages: Vec::new(),
            amounts: Vec::new(),
        }
    }

    /// Leaves `entry`'s keys in `keys` and its withdrawals in `amounts`,
    /// and returns the generations its generation-0 accesses resolve to.
    fn resolve(
        &mut self,
        entry: &ScheduledEntry,
        chain_identifier: &ChainIdentifier,
        bump: &Bump,
    ) -> Vec<(StreamId, u32, u32)> {
        self.keys.clear();
        self.amounts.clear();
        let mut resolved_base = Vec::new();
        self.resolve_shared(&entry.shared, &mut resolved_base);
        if let ExecutableTransaction::User(transaction) = &entry.transaction {
            let data = transaction.get().0.data();
            self.object_keys(data);
            funds_withdrawals(data, chain_identifier, &mut self.amounts, bump);
        }
        resolved_base
    }

    /// Pushes the keys of `shared`'s accesses to `keys`, with each
    /// generation 0 the commit handler linked to an earlier round's
    /// generation in `resolved_base`.
    fn resolve_shared(
        &mut self,
        shared: &[SharedAccess],
        resolved_base: &mut Vec<(StreamId, u32, u32)>,
    ) {
        for access in shared {
            let stream = StreamId {
                id: access.id,
                initial_shared_version: access.initial_shared_version,
            };
            let key = match (access.ordinal.writes_before, access.base) {
                (0, Some(base)) => {
                    resolved_base.push((stream, base.round, base.writes_before));
                    InputKey::Generation {
                        stream,
                        round: base.round,
                        generation: base.writes_before,
                    }
                }
                (0, None) => InputKey::StreamLive(stream),
                (generation, _) => InputKey::Generation {
                    stream,
                    round: access.ordinal.round,
                    generation,
                },
            };
            self.keys.push(key);
        }
    }

    /// Pushes the keys of a user transaction's object inputs, receiving
    /// objects, packages (each once) and gas coins to `keys`: the
    /// reference's `input_objects` and `receiving_objects`, without the
    /// shared inputs, which come from the schedule.
    fn object_keys(&mut self, data: &TransactionData<'_, impl TxState>) {
        if let TransactionKind::ProgrammableTransaction(pt) = data.kind() {
            for arg in pt.inputs {
                match arg {
                    CallArg::Object(ObjectArg::ImmOrOwnedObject(r) | ObjectArg::Receiving(r))
                        if !r.is_coin_reservation() =>
                    {
                        self.keys.push(InputKey::ObjectVersion {
                            id: r.id,
                            version: r.version.get(),
                        });
                    }
                    _ => {}
                }
            }
            self.packages.clear();
            for command in pt.commands {
                match command {
                    MoveCommand::MoveCall(call) => {
                        self.packages.push(*call.package);
                        for t in call.type_arguments {
                            type_packages(t, &mut self.packages);
                        }
                    }
                    MoveCommand::Publish(_, deps) => self.packages.extend_from_slice(deps),
                    MoveCommand::Upgrade(_, deps, package, _) => {
                        self.packages.extend_from_slice(deps);
                        self.packages.push(**package);
                    }
                    MoveCommand::MakeMoveVec(Some(t), _) => type_packages(t, &mut self.packages),
                    _ => {}
                }
            }
            self.packages.sort_unstable();
            self.packages.dedup();
            self.keys
                .extend(self.packages.iter().map(|id| InputKey::Package(*id)));
        }
        self.keys.extend(
            data.gas_data()
                .payment
                .iter()
                .filter(|r| !r.is_coin_reservation())
                .map(|r| InputKey::ObjectVersion {
                    id: r.id,
                    version: r.version.get(),
                }),
        );
    }
}

/// Pushes `data`'s withdrawals to `amounts`, one per account, summed and
/// in increasing account order: the reference's
/// `process_funds_withdrawals_for_execution`. Its funds withdrawal inputs
/// and gas paid from an address balance debit the owner's accumulator field
/// for the type; a coin reservation debits the account its id masks with
/// `chain_identifier`, which may since have been deleted.
///
/// # Panics
/// On what the validity check refuses: a zero amount, a type that is not a
/// balance, or amounts that overflow.
fn funds_withdrawals(
    data: &TransactionData<'_, impl TxState>,
    chain_identifier: &ChainIdentifier,
    amounts: &mut Vec<(ObjectId, u64)>,
    bump: &Bump,
) {
    let index = data.index();
    let gas = data.gas_data();
    let gas_from_balance = gas.payment.is_empty()
        && matches!(data.kind(), TransactionKind::ProgrammableTransaction(_))
        && gas.budget > 0;
    if index.funds_withdrawals.is_empty() && index.coin_reservations.is_empty() && !gas_from_balance
    {
        return;
    }
    for withdrawal in index.funds_withdrawals {
        let withdrawal = withdrawal.get();
        let Reservation::MaxAmountU64(amount) = withdrawal.reservation;
        assert!(amount > 0, "verified in validity check");
        let owner = match withdrawal.withdraw_from {
            WithdrawFrom::Sender => data.sender(),
            WithdrawFrom::Sponsor => gas.owner,
            WithdrawFrom::SenderAllowance { funder, .. } => funder,
        };
        let WithdrawalTypeArg::Balance(inner) = withdrawal.type_arg;
        let account =
            get_field_id(owner, &balance_type(bump, inner)).expect("checked at signing time");
        amounts.push((account, amount));
    }
    if gas_from_balance {
        // Sponsor or sender: either way the gas owner.
        let account =
            get_field_id(gas.owner, &sui_balance_type(bump)).expect("checked at signing time");
        amounts.push((account, gas.budget));
    }
    for reservation in index.coin_reservations {
        let d = &reservation.digest.bytes;
        let amount = u64::from_le_bytes(d[0..8].try_into().expect("eight bytes"));
        let account = std::array::from_fn(|i| reservation.id.0[i] ^ chain_identifier.bytes[i]);
        amounts.push((ObjectId(account), amount));
    }
    amounts.sort_unstable_by_key(|(account, _)| *account);
    amounts.dedup_by(|(account, amount), (kept, total)| {
        let same = account == kept;
        if same {
            *total = total.checked_add(*amount).expect("checked at signing time");
        }
        same
    });
}

/// Every package a type names.
fn type_packages(t: &TypeTag<'_>, packages: &mut Vec<ObjectId>) {
    match t {
        TypeTag::Vector(inner) => type_packages(inner, packages),
        TypeTag::Struct(s) => {
            // An address and an object id are the same 32 bytes.
            packages.push(ObjectId(s.address.0));
            for p in s.type_params {
                type_packages(p, packages);
            }
        }
        _ => {}
    }
}

impl Processor for VersionResolver {
    type Input = Chunk;
    type Output = ();

    fn process(&mut self, chunk: Chunk, bump: &mut Bump) -> Option<()> {
        let Chunk {
            version,
            root_version,
            height,
            entries,
            ..
        } = chunk;
        let mut batch = WaitBatch::with_capacity(entries.len(), entries.len() * 4);
        let mut withdrawals = Vec::new();
        for entry in entries {
            let chain_identifier = self.epoch.chain_identifier;
            let resolved_base = self.resolve(&entry, &chain_identifier, bump);
            let reserved: Vec<keys::Reservation> = self
                .amounts
                .drain(..)
                .map(|(account, amount)| keys::Reservation {
                    account,
                    amount,
                    version,
                })
                .collect();
            if !reserved.is_empty() {
                let ExecutableTransaction::User(transaction) = &entry.transaction else {
                    unreachable!("only user transactions withdraw funds");
                };
                let digest = transaction.get().0.digest().bytes;
                self.keys.push(InputKey::Reserved(digest));
                withdrawals.push(Withdrawal {
                    digest,
                    reservations: reserved.clone(),
                });
            }
            let executable = Executable {
                version,
                root_version,
                reserved,
                height,
                entry,
                resolved_base,
                inputs: Vec::new(),
            };
            batch.push(executable, self.keys.drain(..));
        }
        // Inside the pipeline a full queue is a sizing bug, not load to shed;
        // a closed one means the pipeline is shutting down.
        if !batch.is_empty()
            && let Err(PushError::Full(_)) = self.inputs.try_push(Command::Wait(batch))
        {
            panic!("the input waiter's queue refused work");
        }
        if !withdrawals.is_empty()
            && let Err(PushError::Full(_)) = self
                .withdrawals
                .try_push(FundsCommand::Reserve(withdrawals))
        {
            panic!("the funds reserver's queue refused work");
        }
        None
    }
}

/// The input waiter's lookup of keys resolved before their wait: an
/// `ObjectVersion` the object store holds, a `Package` it holds live, and a
/// `Generation` the generation table holds (with its object, unless the
/// stream ended), each as its value; any `StreamLive` at once. `Reserved` is
/// never found: `FundsReserver` resolves it only after the resolver queued
/// its wait, through the same queue.
pub struct InputAvailability {
    pub store: Arc<store::Store>,
    pub generations: Arc<GenerationTable>,
}

impl InputAvailability {
    fn object(&self, id: &ObjectId, version: u64) -> Option<Message<Object<'static>>> {
        self.store.object(id, version).expect("the store reads")
    }
}

impl Availability<InputKey, InputValue> for InputAvailability {
    /// # Panics
    /// If the store cannot be read, or holds no object at a version the
    /// generation table records.
    fn check(&mut self, keys: &[InputKey], found: &mut Vec<Option<InputValue>>) {
        found.clear();
        found.extend(keys.iter().map(|key| {
            match *key {
                InputKey::ObjectVersion { id, version } => {
                    self.object(&id, version).map(InputValue::Object)
                }
                InputKey::Package(id) => self
                    .store
                    .live_object(&id)
                    .expect("the store reads")
                    .map(InputValue::Object),
                InputKey::Generation {
                    stream,
                    round,
                    generation,
                } => self
                    .generations
                    .get(stream, round, generation)
                    .map(|g| match g {
                        Generation::Version { version, .. } => InputValue::Generation(
                            g,
                            Some(
                                self.object(&stream.id, version)
                                    .expect("a recorded generation's object is stored"),
                            ),
                        ),
                        Generation::Ended { .. } => InputValue::Generation(g, None),
                    }),
                InputKey::StreamLive(_) => Some(InputValue::Live),
                InputKey::Reserved(_) => None,
            }
        }));
    }
}

#[cfg(test)]
mod tests;
