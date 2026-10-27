// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Shared-object congestion control, as the reference's
//! `SharedObjectCongestionTracker` in its `ExecutionTimeEstimate` mode
//! (`docs/congestion_control_reference.md` §1): within a commit, each
//! scheduled transaction's estimate is charged to the consensus objects it
//! writes, and a transaction starting past the commit's budget is deferred.
//! What an object's cost exceeds the budget by carries into later commits as
//! its debt.

use containers::Bump;
use messages::base::ObjectId;
use messages::transaction::SharedObjectMutability;
use sui_protocol_config::ExecutionTimeEstimateParams;

use crate::checks::VerifiedTransaction;
use crate::consensus::sequence::shared_inputs;

/// One commit's accumulated cost per consensus object, in microseconds of
/// estimated execution time.
pub struct CongestionTracker<'b> {
    costs: containers::HashMap<'b, ObjectId, u64>,
    budget: u64,
    burst: u64,
}

impl<'b> CongestionTracker<'b> {
    /// A tracker for a commit whose estimated commit period is
    /// `commit_period_ms`, starting from each object's `debts`.
    pub fn new(
        params: &ExecutionTimeEstimateParams,
        commit_period_ms: u64,
        debts: impl IntoIterator<Item = (ObjectId, u64)>,
        bump: &'b Bump,
    ) -> CongestionTracker<'b> {
        let mut costs = containers::hash_map(bump, 0);
        costs.extend(debts);
        CongestionTracker {
            costs,
            // `Params::commit_budget`, of the period's whole microseconds.
            budget: commit_period_ms
                .saturating_mul(1000)
                .saturating_mul(params.target_utilization)
                / 100,
            burst: params.allowed_txn_cost_overage_burst_limit_us,
        }
    }

    /// `compute_tx_start_at_cost`: the highest cost among `transaction`'s
    /// consensus objects, read-only ones included; none without any.
    fn start_cost(&self, transaction: &VerifiedTransaction) -> Option<u64> {
        shared_inputs(transaction)
            .map(|input| self.costs.get(&input.id).copied().unwrap_or(0))
            .max()
    }

    /// `should_defer_due_to_object_congestion`: if `transaction` starts past
    /// the budget and its burst, the objects at its start cost, in input
    /// order; none if it may run.
    pub fn congested(&self, transaction: &VerifiedTransaction) -> Option<Vec<ObjectId>> {
        let start = self.start_cost(transaction)?;
        if start <= self.budget.saturating_add(self.burst) {
            return None;
        }
        Some(
            shared_inputs(transaction)
                .filter(|input| self.costs.get(&input.id).copied().unwrap_or(0) == start)
                .map(|input| input.id)
                .collect(),
        )
    }

    /// `bump_object_execution_cost`, for a scheduled `transaction` estimated
    /// at `cost`: each object it writes exclusively ends at its start cost
    /// plus `cost`.
    pub fn charge(&mut self, transaction: &VerifiedTransaction, cost: u64) {
        let Some(start) = self.start_cost(transaction) else {
            return;
        };
        let end = start.saturating_add(cost);
        for input in shared_inputs(transaction) {
            if input.mutability() == SharedObjectMutability::Mutable {
                let previous = self.costs.insert(input.id, end);
                debug_assert!(previous.is_none_or(|previous| previous <= end));
            }
        }
    }

    /// `finish_commit`: each object's cost beyond the budget, where any.
    pub fn debts(self) -> impl Iterator<Item = (ObjectId, u64)> + 'b {
        let budget = self.budget;
        self.costs.into_iter().filter_map(move |(id, cost)| {
            let debt = cost.saturating_sub(budget);
            (debt > 0).then_some((id, debt))
        })
    }
}

/// `load_initial_object_debts`' decay: a debt recorded in round `recorded`
/// loses `per_round` for each round between it and `round`.
///
/// # Panics
/// Unless `recorded` is before `round`.
pub fn decayed(debt: u64, recorded: u64, round: u64, per_round: u64) -> u64 {
    assert!(round > recorded, "a debt from an earlier round");
    debt.saturating_sub(per_round * (round - recorded - 1))
}
