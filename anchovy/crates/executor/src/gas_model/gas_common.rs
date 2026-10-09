// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::arithmetic_side_effects)]
#![deny(clippy::cast_possible_truncation)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::cast_possible_wrap)]
#![deny(clippy::cast_sign_loss)]

use messages::base::ObjectId;

/// Portion of the storage rebate that gets passed on to the transaction sender. The remainder
/// will be burned, then re-minted + added to the storage fund at the next epoch change
pub fn sender_rebate(storage_rebate: u64, storage_rebate_rate: u64) -> u64 {
    // we round storage rebate such that `>= x.5` goes to x+1 (rounds up) and
    // `< x.5` goes to x (truncates). We replicate `f32/64::round()`
    const BASIS_POINTS: u128 = 10000;
    let rebate = u128::from(storage_rebate)
        .saturating_mul(u128::from(storage_rebate_rate))
        .saturating_add(BASIS_POINTS / 2) // integer rounding adds half of the denominator
        / BASIS_POINTS;
    u64::try_from(rebate).unwrap_or(u64::MAX)
}

pub fn half_digits_rounding(n: u64) -> u64 {
    if n < 1000 {
        return 1000;
    }
    let digits = n.ilog10();
    let drop = digits / 2;
    let base = 10u64.pow(drop);
    n.div_ceil(base).saturating_mul(base)
}

/// Per-object storage-gas accumulator. Pure data + arithmetic; the Move meter stays on the outer
/// `SuiGasStatus`, which passes the `unmetered` flag into `track_mutation`. The reference also
/// keeps each object's figures for a usage report, which execution never reads; they are not
/// kept here.
#[derive(Debug)]
pub struct StorageGas {
    /// Running total of per-object storage cost. Metered path only.
    total_storage_cost: u64,
    /// Running total of per-object storage rebate. Metered path only.
    total_storage_rebate: u64,
    /// Storage rebate accrued while running unmetered (system transactions), retained in effects
    /// and parked onto 0x5. Kept separate from `total_storage_rebate`: it must read 0 on metered
    /// txns (its consumer `conserve_unmetered_storage_rebate` runs unconditionally).
    unmetered_storage_rebate: u64,
    /// Multiplier applied to the storage byte cost (`ProtocolConfig::storage_gas_price`).
    pub storage_gas_price: u64,
    /// Refundable per-byte storage cost (`ProtocolConfig::obj_data_cost_refundable`).
    storage_per_byte_cost: u64,
}

impl StorageGas {
    pub fn new(storage_gas_price: u64, storage_per_byte_cost: u64) -> Self {
        Self {
            total_storage_cost: 0,
            total_storage_rebate: 0,
            unmetered_storage_rebate: 0,
            storage_gas_price,
            storage_per_byte_cost,
        }
    }

    pub fn storage_gas_units(&self) -> u64 {
        self.total_storage_cost
    }

    pub fn storage_rebate(&self) -> u64 {
        self.total_storage_rebate
    }

    pub fn unmetered_storage_rebate(&self) -> u64 {
        self.unmetered_storage_rebate
    }

    pub fn reset(&mut self) {
        self.total_storage_cost = 0;
        self.total_storage_rebate = 0;
        self.unmetered_storage_rebate = 0;
    }

    /// Update the running storage cost/rebate totals for the object.
    /// Returns the new object storage cost (based on `new_size`), or `None` on overflow.
    pub fn track_mutation(
        &mut self,
        _object_id: &ObjectId,
        new_size: usize,
        storage_rebate: u64,
        unmetered: bool,
    ) -> Option<u64> {
        if unmetered {
            let total = self.unmetered_storage_rebate.checked_add(storage_rebate)?;
            self.unmetered_storage_rebate = total;
            return Some(0);
        }

        let new_size = new_size as u64;
        let storage_cost = new_size
            .checked_mul(self.storage_per_byte_cost)?
            .checked_mul(self.storage_gas_price)?;
        self.total_storage_cost = self.total_storage_cost.checked_add(storage_cost)?;
        self.total_storage_rebate = self.total_storage_rebate.checked_add(storage_rebate)?;
        Some(storage_cost)
    }
}

#[test]
fn test_half_digits_rounding() {
    assert_eq!(half_digits_rounding(0), 1000);
    assert_eq!(half_digits_rounding(1), 1000);
    assert_eq!(half_digits_rounding(999), 1000);
    assert_eq!(half_digits_rounding(1000), 1000);
    assert_eq!(half_digits_rounding(1001), 1010);
    assert_eq!(half_digits_rounding(1050), 1050);
    assert_eq!(half_digits_rounding(1999), 2000);
    assert_eq!(half_digits_rounding(20_000), 20_000);
    assert_eq!(half_digits_rounding(20_001), 20_100);
    assert_eq!(half_digits_rounding(20_500), 20_500);
    assert_eq!(half_digits_rounding(29_999), 30_000);
    assert_eq!(half_digits_rounding(300_000), 300_000);
    assert_eq!(half_digits_rounding(300_001), 300_100);
    assert_eq!(half_digits_rounding(305_500), 305_500);
    assert_eq!(half_digits_rounding(305_501), 305_600);
    assert_eq!(half_digits_rounding(999_999), 1_000_000);
    assert_eq!(half_digits_rounding(1_000_000), 1_000_000);
    assert_eq!(half_digits_rounding(1_000_001), 1_001_000);
    assert_eq!(half_digits_rounding(1_005_000), 1_005_000);
    assert_eq!(half_digits_rounding(1_005_001), 1_006_000);
    assert_eq!(half_digits_rounding(1_999_999), 2_000_000);
    assert_eq!(half_digits_rounding(10_000_001), 10_001_000);
    assert_eq!(half_digits_rounding(100_000_001), 100_010_000);
}
