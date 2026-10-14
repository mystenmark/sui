// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The funds ledger against "Funds reservation" in
//! `docs/consensus_pipeline.md`: worked cases, and a randomized check that,
//! whatever order reservations, deposits and settlements arrive in, every
//! outcome is the naive rule's.

use std::collections::BTreeMap;

use messages::base::ObjectId;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use settlement::funds::{Ledger, Ticket};
use validator::pipeline::items::ReservationStatus::{self, Cancelled, Fulfilled};

fn account(n: u8) -> ObjectId {
    ObjectId([n; 32])
}

/// A ledger and what it resolved, for the worked cases.
struct Case {
    ledger: Ledger,
    balances: BTreeMap<ObjectId, u128>,
    out: Vec<(Ticket, ReservationStatus)>,
}

impl Case {
    fn new(settled: u64, balances: &[(u8, u128)]) -> Case {
        let mut case = Case {
            ledger: Ledger::new(),
            balances: balances.iter().map(|&(a, b)| (account(a), b)).collect(),
            out: Vec::new(),
        };
        case.settle(settled, &[]);
        case
    }

    fn reserve(&mut self, ticket: Ticket, a: u8, amount: u64, version: u64) {
        let balances = &self.balances;
        self.ledger.reserve(
            ticket,
            account(a),
            amount,
            version,
            &mut |id| balances.get(id).copied().unwrap_or(0),
            &mut self.out,
        );
    }

    fn deposit(&mut self, a: u8, amount: u64, version: u64) {
        let balances = &self.balances;
        self.ledger.deposit(
            account(a),
            amount,
            version,
            &mut |id| balances.get(id).copied().unwrap_or(0),
            &mut self.out,
        );
    }

    /// Settles `version` with `changed` as its new balances.
    fn settle(&mut self, version: u64, changed: &[(u8, u128)]) {
        let changed: Vec<(ObjectId, u128)> =
            changed.iter().map(|&(a, b)| (account(a), b)).collect();
        self.balances.extend(changed.iter().copied());
        let balances = &self.balances;
        self.ledger.settle(
            version,
            &changed,
            &mut |id| balances.get(id).copied().unwrap_or(0),
            &mut self.out,
        );
    }

    fn take(&mut self) -> Vec<(Ticket, ReservationStatus)> {
        std::mem::take(&mut self.out)
    }
}

#[test]
fn below_the_lower_bound_fulfils_at_once() {
    let mut case = Case::new(5, &[(1, 1000)]);
    case.reserve(1, 1, 400, 6);
    case.reserve(2, 1, 300, 6);
    assert_eq!(case.take(), [(1, Fulfilled), (2, Fulfilled)]);
    // L(7) = 1000 − 700.
    case.reserve(3, 1, 300, 7);
    case.reserve(4, 1, 1, 7);
    assert_eq!(case.take(), [(3, Fulfilled)]);
}

#[test]
fn a_deposit_raises_the_lower_bound_before_settlement() {
    let mut case = Case::new(5, &[(1, 100)]);
    case.reserve(1, 1, 200, 6);
    assert_eq!(case.take(), []);
    // Commit 5's transactions deposit into version 6.
    case.deposit(1, 60, 6);
    assert_eq!(case.take(), []);
    case.deposit(1, 40, 6);
    assert_eq!(case.take(), [(1, Fulfilled)]);
    // The deposit also raises every later version: L(7) = 250 − 200.
    case.deposit(1, 50, 6);
    case.reserve(2, 1, 50, 7);
    assert_eq!(case.take(), [(2, Fulfilled)]);
}

#[test]
fn cancellation_waits_for_settlement() {
    let mut case = Case::new(5, &[(1, 100)]);
    case.reserve(1, 1, 150, 6);
    case.reserve(2, 1, 50, 6);
    // The second fits, but waits behind the first.
    assert_eq!(case.take(), []);
    case.settle(6, &[(1, 120)]);
    assert_eq!(case.take(), [(1, Cancelled), (2, Fulfilled)]);
}

#[test]
fn a_waiting_reservation_holds_back_later_versions() {
    // The case from the design: without `P(v − 1)` in `L(v)`, B would be
    // fulfilled and overdraw once A is.
    let mut case = Case::new(5, &[(1, 50)]);
    case.reserve(1, 1, 80, 6);
    case.reserve(2, 1, 40, 7);
    assert_eq!(case.take(), []);
    case.settle(6, &[(1, 100)]);
    // A fulfilled at S(6) = 100, so L(7) = 100 − 80 = 20 < 40.
    assert_eq!(case.take(), [(1, Fulfilled)]);
    case.settle(7, &[(1, 20)]);
    assert_eq!(case.take(), [(2, Cancelled)]);
}

#[test]
fn accounts_are_independent() {
    let mut case = Case::new(5, &[(1, 10), (2, 1000)]);
    case.reserve(1, 1, 100, 6);
    case.reserve(2, 2, 100, 6);
    assert_eq!(case.take(), [(2, Fulfilled)]);
}

#[test]
fn a_fulfilled_reservation_at_the_settled_version_still_counts() {
    let mut case = Case::new(5, &[(1, 100)]);
    case.reserve(1, 1, 70, 5);
    assert_eq!(case.take(), [(1, Fulfilled)]);
    // L(6) = 100 − 70.
    case.reserve(2, 1, 40, 6);
    assert_eq!(case.take(), []);
    case.reserve(3, 1, 30, 7);
    case.settle(6, &[(1, 30)]);
    assert_eq!(case.take(), [(2, Cancelled), (3, Fulfilled)]);
}

#[test]
fn before_the_first_settlement_everything_waits() {
    let mut ledger = Ledger::new();
    let mut out = Vec::new();
    let balance = &mut |_: &ObjectId| 100;
    ledger.reserve(1, account(1), 10, 5, balance, &mut out);
    ledger.reserve(2, account(1), 10, 6, balance, &mut out);
    ledger.deposit(account(1), 10, 6, balance, &mut out);
    assert_eq!(out, []);
    // Version 5 settles first: its reservation is decided exactly, and
    // version 6's by its lower bound, 100 − 10 + 10.
    ledger.settle(5, &[], balance, &mut out);
    assert_eq!(out, [(1, Fulfilled), (2, Fulfilled)]);
}

#[test]
fn reservations_against_settled_versions_are_replays() {
    let mut case = Case::new(5, &[(1, 0)]);
    case.settle(6, &[]);
    case.reserve(1, 1, 1000, 5);
    assert_eq!(case.take(), [(1, Fulfilled)]);
}

#[test]
fn settled_accounts_are_forgotten() {
    let mut case = Case::new(5, &[(1, 100)]);
    case.reserve(1, 1, 10, 6);
    case.deposit(2, 10, 7);
    assert_eq!(case.ledger.tracked(), 2);
    case.settle(6, &[(1, 90)]);
    // Account 1 keeps `F(6)` for `L(7)`.
    assert_eq!(case.ledger.tracked(), 2);
    case.settle(7, &[(2, 10)]);
    assert_eq!(case.ledger.tracked(), 0);
}

#[test]
#[should_panic(expected = "reservations against 5 after 6")]
fn reservations_come_in_version_order() {
    let mut case = Case::new(5, &[]);
    case.reserve(1, 1, 1, 6);
    case.reserve(2, 1, 1, 5);
}

/// A transaction of the randomized world.
#[derive(Clone)]
struct Tx {
    /// Per account, in account order.
    reservations: Vec<(u8, u64)>,
    /// What it withdraws if it runs, each at most its reservation.
    withdrawals: Vec<(u8, u64)>,
    deposits: Vec<(u8, u64)>,
}

/// Each commit's transactions' reservations' outcomes.
type Outcomes = Vec<Vec<Vec<ReservationStatus>>>;

struct World {
    first: u64,
    initial: BTreeMap<u8, u128>,
    /// Commit `first + i`'s transactions.
    commits: Vec<Vec<Tx>>,
}

fn world(rng: &mut StdRng) -> World {
    let accounts = rng.gen_range(1..=4u8);
    let initial = (0..accounts)
        .map(|a| (a, u128::from(rng.gen_range(0..200u64))))
        .collect();
    let commits = (0..rng.gen_range(1..8))
        .map(|_| {
            (0..rng.gen_range(0..6))
                .map(|_| {
                    let mut reservations = Vec::new();
                    for a in 0..accounts {
                        if rng.gen_bool(0.4) {
                            reservations.push((a, rng.gen_range(1..120)));
                        }
                    }
                    let withdrawals = reservations
                        .iter()
                        .map(|&(a, r)| (a, rng.gen_range(0..=r)))
                        .collect();
                    let mut deposits = Vec::new();
                    for a in 0..accounts {
                        if rng.gen_bool(0.3) {
                            deposits.push((a, rng.gen_range(1..100)));
                        }
                    }
                    Tx {
                        reservations,
                        withdrawals,
                        deposits,
                    }
                })
                .collect()
        })
        .collect();
    World {
        first: rng.gen_range(1..100),
        initial,
        commits,
    }
}

/// The naive rule: each version settled before any of its reservations is
/// decided, in consensus order. Each transaction's outcomes, and the
/// balances at each version from `first`.
fn naive(world: &World) -> (Outcomes, Vec<BTreeMap<u8, u128>>) {
    let mut balances = vec![world.initial.clone()];
    let mut outcomes = Vec::new();
    for txs in &world.commits {
        let current = balances.last().unwrap().clone();
        let mut fulfilled: BTreeMap<u8, u128> = BTreeMap::new();
        let mut next = current.clone();
        let mut commit = Vec::new();
        for tx in txs {
            let statuses: Vec<ReservationStatus> = tx
                .reservations
                .iter()
                .map(|&(a, r)| {
                    let f = fulfilled.entry(a).or_default();
                    if *f + u128::from(r) <= current[&a] {
                        *f += u128::from(r);
                        Fulfilled
                    } else {
                        Cancelled
                    }
                })
                .collect();
            if statuses.iter().all(|s| *s == Fulfilled) {
                for &(a, w) in &tx.withdrawals {
                    *next.get_mut(&a).unwrap() -= u128::from(w);
                }
                for &(a, d) in &tx.deposits {
                    *next.get_mut(&a).unwrap() += u128::from(d);
                }
            }
            commit.push(statuses);
        }
        outcomes.push(commit);
        balances.push(next);
    }
    (outcomes, balances)
}

/// Runs `world` through a ledger with events in a random order that
/// respects causality, and returns each transaction's outcomes, and how
/// many reservations were fulfilled before their version settled.
#[allow(clippy::too_many_lines)]
fn run(world: &World, rng: &mut StdRng, truth: &[BTreeMap<u8, u128>]) -> (Outcomes, usize) {
    let mut ledger = Ledger::new();
    let mut out = Vec::new();
    // Tickets: (commit, transaction, reservation index).
    let mut tickets = Vec::new();
    let mut resolved: BTreeMap<Ticket, ReservationStatus> = BTreeMap::new();
    let mut early = 0;
    // The next transaction to deliver, and deposits made but not delivered.
    let mut next = (0, 0);
    let mut deposits: Vec<(u8, u64, u64)> = Vec::new();
    // Credits of what executed transactions reserved but did not withdraw,
    // not yet delivered.
    let mut credits: Vec<(u8, u64, u64)> = Vec::new();
    // Transactions delivered whose deposits are not yet made.
    let mut running: Vec<(usize, usize)> = Vec::new();
    // Settled through `first + settled`, once the first settlement came.
    let mut settled: Option<usize> = None;
    loop {
        let base_at = settled.unwrap_or(0);
        let mut balance = |id: &ObjectId| truth[base_at][&id.0[0]];
        let delivered_all = next.0 == world.commits.len();
        // Transactions ready to run: delivered, reservations resolved.
        let ready: Vec<usize> = running
            .iter()
            .enumerate()
            .filter(|&(_, &(c, t))| {
                (0..world.commits[c][t].reservations.len())
                    .all(|i| resolved.contains_key(&ticket_of(&tickets, (c, t, i))))
            })
            .map(|(i, _)| i)
            .collect();
        // Settling `first + s + 1` takes commit `s` delivered and run.
        let can_settle = match settled {
            None => true,
            Some(s) => {
                s < world.commits.len()
                    && (next.0 > s || delivered_all)
                    && !running.iter().any(|&(c, _)| c <= s)
            }
        };
        let mut actions = Vec::new();
        if !delivered_all {
            actions.push(0);
        }
        if !ready.is_empty() {
            actions.push(1);
        }
        if !deposits.is_empty() {
            actions.push(2);
        }
        if can_settle {
            actions.push(3);
        }
        if !credits.is_empty() {
            actions.push(4);
        }
        let Some(&action) = actions.get(rng.gen_range(0..actions.len().max(1))) else {
            break;
        };
        match action {
            0 => {
                let (c, t) = next;
                if t == world.commits[c].len() {
                    next = (c + 1, 0);
                    continue;
                }
                let version = world.first + c as u64;
                for (i, &(a, r)) in world.commits[c][t].reservations.iter().enumerate() {
                    let ticket = tickets.len() as Ticket;
                    tickets.push((c, t, i));
                    ledger.reserve(ticket, account(a), r, version, &mut balance, &mut out);
                }
                running.push((c, t));
                next = (c, t + 1);
            }
            1 => {
                let (c, t) = running.remove(ready[rng.gen_range(0..ready.len())]);
                let tx = &world.commits[c][t];
                let ran = (0..tx.reservations.len())
                    .all(|i| resolved[&ticket_of(&tickets, (c, t, i))] == Fulfilled);
                let version = world.first + c as u64;
                if ran {
                    deposits.extend(tx.deposits.iter().map(|&(a, d)| (a, d, version + 1)));
                    for (i, &(a, r)) in tx.reservations.iter().enumerate() {
                        let w = tx.withdrawals[i].1;
                        if r > w {
                            credits.push((a, r - w, version));
                        }
                    }
                } else {
                    // Cancelled: `FundsReserver` credits its fulfilled
                    // reservations at once.
                    for (i, &(a, r)) in tx.reservations.iter().enumerate() {
                        if resolved[&ticket_of(&tickets, (c, t, i))] == Fulfilled {
                            ledger.credit(account(a), r, version, &mut out);
                        }
                    }
                }
            }
            2 => {
                let (a, d, version) = deposits.swap_remove(rng.gen_range(0..deposits.len()));
                ledger.deposit(account(a), d, version, &mut balance, &mut out);
            }
            3 => {
                let s = settled.map_or(0, |s| s + 1);
                settled = Some(s);
                let changed: Vec<(ObjectId, u128)> = truth[s]
                    .iter()
                    .filter(|(a, b)| s == 0 || truth[s - 1][a] != **b)
                    .map(|(&a, &b)| (account(a), b))
                    .collect();
                let mut balance = |id: &ObjectId| truth[s][&id.0[0]];
                ledger.settle(world.first + s as u64, &changed, &mut balance, &mut out);
            }
            4 => {
                let (a, amount, version) = credits.swap_remove(rng.gen_range(0..credits.len()));
                ledger.credit(account(a), amount, version, &mut out);
            }
            _ => unreachable!(),
        }
        for (ticket, status) in out.drain(..) {
            let (c, ..) = tickets[ticket as usize];
            if status == Fulfilled && settled.is_none_or(|s| s < c) {
                early += 1;
            }
            assert!(resolved.insert(ticket, status).is_none(), "resolved once");
        }
    }
    assert_eq!(resolved.len(), tickets.len(), "every reservation resolved");
    let outcomes = world
        .commits
        .iter()
        .enumerate()
        .map(|(c, txs)| {
            txs.iter()
                .enumerate()
                .map(|(t, tx)| {
                    (0..tx.reservations.len())
                        .map(|i| resolved[&ticket_of(&tickets, (c, t, i))])
                        .collect()
                })
                .collect()
        })
        .collect();
    (outcomes, early)
}

fn ticket_of(tickets: &[(usize, usize, usize)], key: (usize, usize, usize)) -> Ticket {
    tickets.iter().position(|k| *k == key).expect("delivered") as Ticket
}

#[test]
fn outcomes_are_the_naive_rules_in_any_order() {
    let mut early = 0;
    for seed in 0..2000 {
        let mut rng = StdRng::seed_from_u64(seed);
        let world = world(&mut rng);
        let (expected, truth) = naive(&world);
        for _ in 0..4 {
            let (outcomes, fast) = run(&world, &mut rng, &truth);
            assert_eq!(outcomes, expected, "seed {seed}");
            early += fast;
        }
    }
    // Not a correctness property, but the point of the bounds.
    assert!(early > 1000, "only {early} reservations fulfilled early");
}

#[test]
fn what_a_reservation_did_not_withdraw_is_credited_to_later_versions() {
    let mut case = Case::new(5, &[(1, 100)]);
    case.reserve(1, 1, 80, 6);
    assert_eq!(case.take(), [(1, Fulfilled)]);
    case.reserve(2, 1, 50, 7);
    assert_eq!(case.take(), []);
    // The first withdrew 30 of its 80: L(7) = 100 − 30.
    case.ledger.credit(account(1), 50, 6, &mut case.out);
    assert_eq!(case.take(), [(2, Fulfilled)]);
}

#[test]
fn a_credit_does_not_apply_at_its_own_version() {
    let mut case = Case::new(5, &[(1, 100)]);
    case.reserve(1, 1, 80, 6);
    case.reserve(2, 1, 50, 6);
    assert_eq!(case.take(), [(1, Fulfilled)]);
    // The naive rule counts the 80 reserved against version 6, whatever
    // was withdrawn.
    case.ledger.credit(account(1), 80, 6, &mut case.out);
    assert_eq!(case.take(), []);
    case.settle(6, &[]);
    assert_eq!(case.take(), [(2, Cancelled)]);
}

#[test]
fn credits_after_the_next_version_settles_are_ignored() {
    let mut case = Case::new(5, &[(1, 100)]);
    case.reserve(1, 1, 80, 6);
    case.settle(6, &[(1, 100)]);
    case.settle(7, &[(1, 20)]);
    case.ledger.credit(account(1), 80, 6, &mut case.out);
    assert_eq!(case.ledger.tracked(), 0);
}

#[test]
fn a_settled_version_remembers_what_it_fulfilled_though_credited_back() {
    let mut case = Case::new(5, &[(1, 100)]);
    case.reserve(1, 1, 80, 6);
    case.ledger.credit(account(1), 80, 6, &mut case.out);
    case.settle(6, &[]);
    assert_eq!(case.take(), [(1, Fulfilled)]);
    // A late reservation against 6 still sees the 80 reserved.
    case.reserve(2, 1, 50, 6);
    assert_eq!(case.take(), [(2, Cancelled)]);
}
