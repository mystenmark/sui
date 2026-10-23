// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Ordering a commit's transactions and placing their shared-object accesses
//! in consensus order. Where the reference assigns each shared input an
//! exact version from per-epoch state, this assigns an ordinal, which
//! execution maps to a version later: no state outlives a commit.

use containers::Bump;
use hashbrown::HashMap;
use messages::base::ObjectId;
use messages::build::system_transaction::{Jwk, JwkId};
use messages::transaction::{
    CallArg, ObjectArg, SharedObjectArg, SharedObjectMutability, TransactionKind,
};
use protocol_config::{ConsensusTransactionOrdering, ProtocolConfig};

use crate::checks::VerifiedTransaction;

/// The clock (`SUI_CLOCK_OBJECT_ID`), which the commit prologue writes.
const CLOCK: ObjectId = ObjectId::from_u16(0x6);
/// `SUI_CLOCK_OBJECT_SHARED_VERSION`.
const CLOCK_INITIAL_SHARED_VERSION: u64 = 1;
/// The authenticator state (`SUI_AUTHENTICATOR_STATE_OBJECT_ID`), which
/// `AuthenticatorStateUpdate` writes.
const AUTHENTICATOR_STATE: ObjectId = ObjectId::from_u16(0x7);
/// The randomness state (`SUI_RANDOMNESS_STATE_OBJECT_ID`).
const RANDOMNESS_STATE: ObjectId = ObjectId::from_u16(0x8);

/// Where an access to a shared-object stream falls in consensus order: the
/// commit's round, and how many of the commit's earlier transactions write
/// the stream exclusively. It names generation `writes_before` of the stream
/// in `round`: `(round, 0)` is the stream as earlier commits left it, and an
/// exclusive writer at `(round, k)` reads generation `k` and produces
/// `k + 1`. Ordered by round, then generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SharedOrdinal {
    pub round: u32,
    pub writes_before: u32,
}

/// One shared input of a scheduled transaction. A stream is an object id and
/// the version its sharing started at, as the reference keys them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SharedAccess {
    pub id: ObjectId,
    pub initial_shared_version: u64,
    pub mutability: SharedObjectMutability,
    pub ordinal: SharedOrdinal,
    /// At generation 0, the stream's last generation from an earlier round,
    /// which the access reads, as (round, generation); none if it reads the
    /// stream live (first scheduled this epoch, or read live earlier this
    /// round). Unused at a later generation. Set by the commit handler.
    pub base: Option<SharedOrdinal>,
}

/// What building a commit's consensus commit prologue takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prologue {
    pub epoch: u64,
    pub round: u32,
    pub commit_index: u32,
    pub timestamp_ms: u64,
    pub commit_digest: [u8; 32],
}

/// A transaction in a commit's schedule.
// Nearly every entry is a user transaction; boxing them would allocate each.
#[derive(Debug)]
pub enum Scheduled {
    Prologue(Prologue),
    /// The JSON web keys that became active in the last commit.
    AuthenticatorStateUpdate(Vec<(JwkId, Jwk)>),
    User(VerifiedTransaction),
}

/// An `AuthenticatorStateUpdate` to schedule: the authenticator state's
/// initial shared version, and the keys it adds.
pub struct AuthenticatorUpdate {
    pub initial_shared_version: u64,
    pub keys: Vec<(JwkId, Jwk)>,
}

/// A commit's transactions in execution order, each with its shared
/// accesses, laid out flat.
#[derive(Debug)]
pub struct Schedule<'b> {
    pub transactions: containers::Vec<'b, Scheduled>,
    accesses: containers::Vec<'b, SharedAccess>,
    /// Where each transaction's accesses end in `accesses`.
    ends: containers::Vec<'b, u32>,
}

impl Schedule<'_> {
    /// The shared accesses of the `i`th transaction, in input order.
    pub fn accesses(&self, i: usize) -> &[SharedAccess] {
        let start = i.checked_sub(1).map_or(0, |j| self.ends[j] as usize);
        &self.accesses[start..self.ends[i] as usize]
    }

    /// Every transaction's shared accesses, in schedule order.
    pub fn all_accesses_mut(&mut self) -> &mut [SharedAccess] {
        &mut self.accesses
    }

    pub fn len(&self) -> usize {
        self.transactions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.transactions.is_empty()
    }
}

/// A stream's exclusive writers so far in the commit.
struct Writes {
    count: u32,
    /// The schedule index of the last writer counted: a transaction that
    /// names a stream twice writes it once.
    last_writer: u32,
}

/// Schedules commits, keeping its scratch tables between them. Nothing it
/// keeps outlives a commit's schedule.
#[derive(Default)]
pub struct Sequencer {
    writes: HashMap<(ObjectId, u64), Writes, foldhash::fast::RandomState>,
    order: Vec<(bool, std::cmp::Reverse<u64>, u32)>,
}

impl Sequencer {
    pub fn new() -> Sequencer {
        Sequencer::default()
    }

    /// `transactions` (accepted, deduplicated, those deferred first, then
    /// the commit's in commit order) as the protocol orders them: those
    /// using randomness last, each sorted by gas price if the protocol says
    /// so, ties keeping their order. The reference's
    /// `merge_and_reorder_transactions`.
    pub fn order<'b>(
        &mut self,
        config: &ProtocolConfig,
        transactions: containers::Vec<'b, VerifiedTransaction>,
        bump: &'b Bump,
    ) -> containers::Vec<'b, VerifiedTransaction> {
        let by_gas_price =
            config.consensus_transaction_ordering() == ConsensusTransactionOrdering::ByGasPrice;
        self.order.clear();
        self.order
            .extend(transactions.iter().enumerate().map(|(i, transaction)| {
                let price = if by_gas_price {
                    transaction.get().0.data().gas_data().price
                } else {
                    0
                };
                (
                    uses_randomness(transaction),
                    std::cmp::Reverse(price),
                    i as u32,
                )
            }));
        self.order
            .sort_by_key(|(randomness, price, _)| (*randomness, *price));
        let mut slots = containers::Vec::with_capacity_in(transactions.len(), bump);
        slots.extend(transactions.into_iter().map(Some));
        let mut ordered = containers::Vec::with_capacity_in(slots.len(), bump);
        ordered.extend(
            self.order
                .iter()
                .map(|(_, _, i)| slots[*i as usize].take().expect("each transaction once")),
        );
        ordered
    }

    /// The commit's schedule: the prologue, the authenticator state's
    /// update if any, then `transactions`, in order, each with its shared
    /// inputs' ordinals: the reference's `assign_versions_from_consensus`,
    /// with ordinals for versions.
    pub fn schedule<'b>(
        &mut self,
        prologue: Prologue,
        update: Option<AuthenticatorUpdate>,
        transactions: containers::Vec<'b, VerifiedTransaction>,
        bump: &'b Bump,
    ) -> Schedule<'b> {
        let round = prologue.round;
        let accesses = 2 + transactions
            .iter()
            .map(|t| shared_inputs(t).count())
            .sum::<usize>();
        let mut schedule = Schedule {
            transactions: containers::Vec::with_capacity_in(transactions.len() + 2, bump),
            accesses: containers::Vec::with_capacity_in(accesses, bump),
            ends: containers::Vec::with_capacity_in(transactions.len() + 2, bump),
        };

        // 1. The prologue writes the clock, first.
        schedule.transactions.push(Scheduled::Prologue(prologue));
        let clock = SharedAccess {
            id: CLOCK,
            initial_shared_version: CLOCK_INITIAL_SHARED_VERSION,
            mutability: SharedObjectMutability::Mutable,
            ordinal: SharedOrdinal {
                round,
                writes_before: 0,
            },
            base: None,
        };
        Self::count(&mut self.writes, 0, std::iter::once(&clock));
        schedule.accesses.push(clock);
        schedule.ends.push(1);

        // 2. The authenticator state's update writes it, next.
        if let Some(update) = update {
            let authenticator = SharedAccess {
                id: AUTHENTICATOR_STATE,
                initial_shared_version: update.initial_shared_version,
                mutability: SharedObjectMutability::Mutable,
                ordinal: SharedOrdinal {
                    round,
                    writes_before: 0,
                },
                base: None,
            };
            Self::count(&mut self.writes, 1, std::iter::once(&authenticator));
            schedule.accesses.push(authenticator);
            schedule.ends.push(2);
            schedule
                .transactions
                .push(Scheduled::AuthenticatorStateUpdate(update.keys));
        }

        // 3. Each transaction's shared inputs: every access reads the
        //    generation before the transaction's own writes.
        let first = schedule.transactions.len();
        for (offset, transaction) in transactions.into_iter().enumerate() {
            let start = schedule.accesses.len();
            for shared in shared_inputs(&transaction) {
                let stream = (shared.id, shared.initial_shared_version.get());
                schedule.accesses.push(SharedAccess {
                    id: shared.id,
                    initial_shared_version: stream.1,
                    mutability: shared.mutability(),
                    ordinal: SharedOrdinal {
                        round,
                        writes_before: self.writes.get(&stream).map_or(0, |w| w.count),
                    },
                    base: None,
                });
            }
            let index = u32::try_from(first + offset).expect("fewer than 2^32 transactions");
            let accesses = &schedule.accesses[start..];
            Self::count(&mut self.writes, index, accesses.iter());
            schedule
                .ends
                .push(u32::try_from(schedule.accesses.len()).expect("fewer than 2^32 accesses"));
            schedule.transactions.push(Scheduled::User(transaction));
        }

        self.writes.clear();
        schedule
    }

    /// Counts the exclusive writes of the transaction at `index`, once per
    /// stream.
    fn count<'s>(
        writes: &mut HashMap<(ObjectId, u64), Writes, foldhash::fast::RandomState>,
        index: u32,
        accesses: impl Iterator<Item = &'s SharedAccess>,
    ) {
        for access in accesses {
            if access.mutability != SharedObjectMutability::Mutable {
                continue;
            }
            let w = writes
                .entry((access.id, access.initial_shared_version))
                .or_insert(Writes {
                    count: 0,
                    last_writer: u32::MAX,
                });
            if w.last_writer != index {
                w.count += 1;
                w.last_writer = index;
            }
        }
    }
}

/// Whether `transaction` reads the randomness state.
pub fn uses_randomness(transaction: &VerifiedTransaction) -> bool {
    shared_inputs(transaction).any(|shared| shared.id == RANDOMNESS_STATE)
}

/// A user transaction's shared inputs, in input order: the reference's
/// `shared_input_objects`.
fn shared_inputs(transaction: &VerifiedTransaction) -> impl Iterator<Item = &SharedObjectArg> + '_ {
    let inputs = match transaction.get().0.data().kind() {
        TransactionKind::ProgrammableTransaction(pt) => pt.inputs,
        _ => &[],
    };
    inputs.iter().filter_map(|input| match input {
        CallArg::Object(ObjectArg::SharedObject(shared)) => Some(*shared),
        _ => None,
    })
}
