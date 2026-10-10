// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::ops::Add;

use move_core_types::gas_algebra::{
    GasQuantity, InternalGas, InternalGasUnit, ToUnit, ToUnitFractional,
};

pub enum GasUnit {}

pub type Gas = GasQuantity<GasUnit>;

impl ToUnit<InternalGasUnit> for GasUnit {
    const MULTIPLIER: u64 = 1000;
}

impl ToUnitFractional<GasUnit> for InternalGasUnit {
    const NOMINATOR: u64 = 1;
    const DENOMINATOR: u64 = 1000;
}

pub const INSTRUCTION_TIER_DEFAULT: u64 = 1;

pub const STACK_HEIGHT_TIER_DEFAULT: u64 = 1;
pub const STACK_SIZE_TIER_DEFAULT: u64 = 1;

/// The cost table holds the tiers and curves for instruction costs. Each
/// tier list is `(tier start, cost)` in increasing start order: the
/// reference's `BTreeMap<u64, u64>` as a constant, so no transaction builds
/// or clones one.
#[derive(Debug, PartialEq, Eq)]
pub struct CostTable {
    pub instruction_tiers: &'static [(u64, u64)],
    pub stack_height_tiers: &'static [(u64, u64)],
    pub stack_size_tiers: &'static [(u64, u64)],
}

impl CostTable {
    /// The cost of the tier `current` falls in (the last starting at or
    /// below it), and where the next tier starts.
    fn get_current_and_future_tier(
        tiers: &[(u64, u64)],
        current: u64,
        default: u64,
    ) -> (u64, Option<u64>) {
        let next = tiers.partition_point(|&(start, _)| start <= current);
        let current_cost = match next.checked_sub(1) {
            Some(i) => tiers[i].1,
            None => default,
        };
        let next_tier_start = tiers.get(next).map(|&(start, _)| start);
        (current_cost, next_tier_start)
    }

    pub fn instruction_tier(&self, instr_count: u64) -> (u64, Option<u64>) {
        Self::get_current_and_future_tier(
            self.instruction_tiers,
            instr_count,
            INSTRUCTION_TIER_DEFAULT,
        )
    }

    pub fn stack_height_tier(&self, stack_height: u64) -> (u64, Option<u64>) {
        Self::get_current_and_future_tier(
            self.stack_height_tiers,
            stack_height,
            STACK_HEIGHT_TIER_DEFAULT,
        )
    }

    pub fn stack_size_tier(&self, stack_size: u64) -> (u64, Option<u64>) {
        Self::get_current_and_future_tier(
            self.stack_size_tiers,
            stack_size,
            STACK_SIZE_TIER_DEFAULT,
        )
    }
}

/// The  `GasCost` tracks:
/// - instruction cost: how much time/computational power is needed to perform the instruction
/// - memory cost: how much memory is required for the instruction, and storage overhead
/// - stack height: how high is the stack growing (regardless of size in bytes)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GasCost {
    pub instruction_gas: u64,
    pub memory_gas: u64,
    pub stack_height_gas: u64,
}

impl GasCost {
    pub fn new(instruction_gas: u64, memory_gas: u64, stack_height_gas: u64) -> Self {
        Self {
            instruction_gas,
            memory_gas,
            stack_height_gas,
        }
    }

    /// Convert a GasCost to a total gas charge in `InternalGas`.
    #[inline]
    pub fn total(&self) -> u64 {
        self.instruction_gas
            .add(self.memory_gas)
            .add(self.stack_height_gas)
    }

    #[inline]
    pub fn total_internal(&self) -> InternalGas {
        GasQuantity::new(
            self.instruction_gas
                .add(self.memory_gas)
                .add(self.stack_height_gas),
        )
    }
}
