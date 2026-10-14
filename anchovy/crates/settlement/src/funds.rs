// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Resolving funds reservations by a lower and an upper bound on each
//! account's balance at each accumulator version
//! (`docs/consensus_pipeline.md`, "Funds reservation").

use std::collections::{BTreeMap, HashMap, VecDeque};

use messages::base::ObjectId;
use validator::pipeline::items::ReservationStatus;

/// Names what waits on a reservation; the ledger hands it back resolved.
pub type Ticket = u64;

type FastMap<K, V> = HashMap<K, V, foldhash::fast::RandomState>;

/// The rules of "Funds reservation", for every account. Pure: balances it
/// does not know come from the caller's `base`, which must give an
/// account's exact balance at the settled version.
#[derive(Default)]
pub struct Ledger {
    /// σ, the highest settled version; none before the first settlement.
    settled: Option<u64>,
    /// The highest version reserved against: reservations come in version
    /// order.
    reserved: u64,
    accounts: FastMap<ObjectId, Account>,
}

/// One account's state at σ and above.
#[derive(Default)]
struct Account {
    /// `S(σ)`, once read; none only before the first settlement.
    base: Option<u128>,
    /// Each version from σ up with any state. A version without an entry
    /// has no reservations and no deposits, so leaves `L` unchanged.
    versions: BTreeMap<u64, Level>,
}

/// One account at one version.
#[derive(Default)]
struct Level {
    /// `F(v)`.
    fulfilled: u128,
    /// `P(v)`: the sum of `queue`.
    pending: u128,
    /// `D(v)`; part of `base` once `v` settles.
    deposits: u128,
    /// `C(v)`: at most `fulfilled`.
    credits: u128,
    /// The unresolved reservations, in arrival (consensus) order.
    queue: VecDeque<(u64, Ticket)>,
}

impl Level {
    fn is_empty(&self) -> bool {
        self.fulfilled == 0 && self.pending == 0 && self.deposits == 0
    }

    /// What `v`'s reservations may still withdraw: `F(v) − C(v) + P(v)`.
    fn outstanding(&self) -> u128 {
        self.fulfilled - self.credits + self.pending
    }
}

impl Ledger {
    pub fn new() -> Ledger {
        Ledger::default()
    }

    /// σ, the highest settled version.
    pub fn settled(&self) -> Option<u64> {
        self.settled
    }

    /// Accounts with state: those with reservations or deposits not yet
    /// settled, or fulfilled reservations at σ.
    pub fn tracked(&self) -> usize {
        self.accounts.len()
    }

    /// `Reserve(account, amount, version)`. Resolutions, this one's or any
    /// it unblocks, are pushed to `out`.
    ///
    /// A reservation against a version below σ is `Fulfilled` without being
    /// counted: its commit has settled, so its transaction executed before
    /// and is being replayed.
    ///
    /// # Panics
    /// If reservations come out of version order.
    pub fn reserve(
        &mut self,
        ticket: Ticket,
        account: ObjectId,
        amount: u64,
        version: u64,
        base: &mut impl FnMut(&ObjectId) -> u128,
        out: &mut Vec<(Ticket, ReservationStatus)>,
    ) {
        assert!(
            version >= self.reserved,
            "reservations against {version} after {}",
            self.reserved
        );
        self.reserved = version;
        if self.settled.is_some_and(|settled| version < settled) {
            out.push((ticket, ReservationStatus::Fulfilled));
            return;
        }
        let settled = self.settled;
        let state = self.account(account, base);
        let level = state.versions.entry(version).or_default();
        level.pending += u128::from(amount);
        level.queue.push_back((amount, ticket));
        if let Some(settled) = settled {
            drain(state, settled, out);
        }
    }

    /// `Deposit(account, amount, version)`: ignored once `version` has
    /// settled, whose balance already holds it.
    pub fn deposit(
        &mut self,
        account: ObjectId,
        amount: u64,
        version: u64,
        base: &mut impl FnMut(&ObjectId) -> u128,
        out: &mut Vec<(Ticket, ReservationStatus)>,
    ) {
        if self.settled.is_some_and(|settled| version <= settled) {
            return;
        }
        let settled = self.settled;
        let state = self.account(account, base);
        state.versions.entry(version).or_default().deposits += u128::from(amount);
        if let Some(settled) = settled {
            drain(state, settled, out);
        }
    }

    /// `Credit(account, amount, version)`: a fulfilled reservation against
    /// `version` withdrew `amount` less than it reserved. Ignored once
    /// `version + 1` has settled, whose balance holds what was withdrawn.
    ///
    /// # Panics
    /// If more is credited than was fulfilled.
    pub fn credit(
        &mut self,
        account: ObjectId,
        amount: u64,
        version: u64,
        out: &mut Vec<(Ticket, ReservationStatus)>,
    ) {
        if self.settled.is_some_and(|settled| version < settled) {
            return;
        }
        let Some(state) = self.accounts.get_mut(&account) else {
            panic!("a credit to {account:?}, which reserved nothing");
        };
        let level = state
            .versions
            .get_mut(&version)
            .expect("a credit against a version with fulfilled reservations");
        level.credits += u128::from(amount);
        assert!(
            level.credits <= level.fulfilled,
            "credits exceed what was fulfilled"
        );
        if let Some(settled) = self.settled {
            drain(state, settled, out);
        }
    }

    /// `Settled(balances, version)`: `balances` holds the exact balance of
    /// every account the settlement changed. The first settlement may name
    /// any version; `base` must give balances at it.
    ///
    /// # Panics
    /// If a settlement after the first does not follow σ.
    pub fn settle(
        &mut self,
        version: u64,
        balances: &[(ObjectId, u128)],
        base: &mut impl FnMut(&ObjectId) -> u128,
        out: &mut Vec<(Ticket, ReservationStatus)>,
    ) {
        if let Some(settled) = self.settled {
            assert_eq!(version, settled + 1, "versions settle in order");
        }
        self.settled = Some(version);
        for (account, balance) in balances {
            if let Some(state) = self.accounts.get_mut(account) {
                state.base = Some(*balance);
            }
        }
        self.accounts.retain(|account, state| {
            // An account the settlement did not change keeps `S(v − 1)`.
            state.base.get_or_insert_with(|| base(account));
            let above = state.versions.split_off(&version);
            // Only before the first settlement can a version below it hold
            // reservations: replayed, as in `reserve`.
            for level in std::mem::replace(&mut state.versions, above).into_values() {
                out.extend(
                    level
                        .queue
                        .into_iter()
                        .map(|(_, ticket)| (ticket, ReservationStatus::Fulfilled)),
                );
            }
            drain(state, version, out);
            state.versions.retain(|&v, level| {
                // At σ, `F` still counts against reservations yet to arrive,
                // and `F − C` against `L(σ + 1)`.
                !level.is_empty() && (v > version || level.fulfilled > 0)
            });
            !state.versions.is_empty()
        });
    }

    fn account(
        &mut self,
        account: ObjectId,
        base: &mut impl FnMut(&ObjectId) -> u128,
    ) -> &mut Account {
        let settled = self.settled;
        self.accounts.entry(account).or_insert_with(|| Account {
            base: settled.map(|_| base(&account)),
            versions: BTreeMap::new(),
        })
    }
}

/// Applies R1-R3 to `state` at every version from `settled` up, in order:
/// each version's lower bound depends on the ones below it.
fn drain(state: &mut Account, settled: u64, out: &mut Vec<(Ticket, ReservationStatus)>) {
    let base = state
        .base
        .expect("an account's balance is read once a version settles");
    // `L` at the version being visited, and `F − C + P` at the one before.
    let mut lower = base;
    let mut below = 0;
    for (&version, level) in state.versions.range_mut(settled..) {
        if version > settled {
            // L(v) = max(0, L(v−1) − (F(v−1) − C(v−1)) − P(v−1)) + D(v).
            lower = lower.saturating_sub(below) + level.deposits;
        }
        while let Some(&(amount, ticket)) = level.queue.front() {
            let amount = u128::from(amount);
            let status = if level.fulfilled + amount <= lower {
                level.fulfilled += amount;
                ReservationStatus::Fulfilled
            } else if version <= settled {
                ReservationStatus::Cancelled
            } else {
                break;
            };
            level.queue.pop_front();
            level.pending -= amount;
            out.push((ticket, status));
        }
        // Credits count only from the next version: here R1 counts declared
        // maxima, as the naive rule does.
        below = level.outstanding();
    }
}

/// `Field<Key<Balance<T>>, U128>`, the field's id, the key's owner, then the
/// value as a little-endian `u128`.
pub fn account_balance(contents: &[u8]) -> u128 {
    let value: [u8; 16] = contents
        .get(64..80)
        .filter(|_| contents.len() == 80)
        .and_then(|v| v.try_into().ok())
        .unwrap_or_else(|| panic!("an accumulator field of {} bytes", contents.len()));
    u128::from_le_bytes(value)
}
