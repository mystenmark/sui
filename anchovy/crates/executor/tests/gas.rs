// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The gas model against sui-types' over random charge sequences at the
//! latest protocol version: every result and summary agree, metered and
//! unmetered.

use executor::gas::{SuiGasStatus, SuiGasStatusAPI};
use messages::base::ObjectId;
use sui_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use sui_types::gas::{SuiGasStatus as Reference, SuiGasStatusAPI as _};

/// xorshift64*: deterministic, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn same_summary(ours: &SuiGasStatus, theirs: &Reference, context: &str) {
    let (a, b) = (ours.summary(), theirs.summary());
    assert_eq!(
        (
            a.computation_cost,
            a.storage_cost,
            a.storage_rebate,
            a.non_refundable_storage_fee
        ),
        (
            b.computation_cost,
            b.storage_cost,
            b.storage_rebate,
            b.non_refundable_storage_fee
        ),
        "{context}"
    );
    assert_eq!(ours.gas_used(), theirs.gas_used(), "{context}");
    assert_eq!(ours.storage_rebate(), theirs.storage_rebate(), "{context}");
    assert_eq!(
        ours.unmetered_storage_rebate(),
        theirs.unmetered_storage_rebate(),
        "{context}"
    );
}

fn run(rng: &mut Rng, mut ours: SuiGasStatus, mut theirs: Reference, label: &str) {
    for step in 0..60 {
        let context = format!("{label} step {step}");
        match rng.below(9) {
            0 => {
                let size = rng.below(200_000) as usize;
                let (a, b) = (
                    ours.charge_storage_read(size),
                    theirs.charge_storage_read(size),
                );
                assert_eq!(a.is_ok(), b.is_ok(), "{context}");
            }
            1 => {
                let size = rng.below(200_000) as usize;
                let (a, b) = (
                    ours.charge_publish_package(size),
                    theirs.charge_publish_package(size),
                );
                assert_eq!(a.is_ok(), b.is_ok(), "{context}");
            }
            2 | 3 => {
                let (n, push, incr) = (rng.below(5000), rng.below(50), rng.below(5000));
                // The meter treats popping below empty as a bug.
                let height = ours.move_gas_status().stack_height_current() + push;
                let pop = rng.below(height + 1);
                let a = ours.move_gas_status_mut().charge(n, push, pop, incr, 0);
                let b = theirs.move_gas_status_mut().charge(n, push, pop, incr, 0);
                assert_eq!(a.is_ok(), b.is_ok(), "{context}");
            }
            4 => {
                let (size, rebate) = (rng.below(10_000) as usize, rng.below(10_000_000));
                let id = [rng.next() as u8; 32];
                let a = ours.track_storage_mutation(&ObjectId(id), size, rebate);
                let b = theirs.track_storage_mutation(
                    sui_types::base_types::ObjectID::new(id),
                    size,
                    rebate,
                );
                assert_eq!(a, b, "{context}");
            }
            5 => {
                let aborted = match rng.below(3) {
                    0 => None,
                    1 => Some(false),
                    _ => Some(true),
                };
                let (a, b) = (
                    ours.bucketize_computation(aborted),
                    theirs.bucketize_computation(aborted),
                );
                assert_eq!(a.is_ok(), b.is_ok(), "{context}");
            }
            6 => {
                let (a, b) = (
                    ours.charge_storage_and_rebate(),
                    theirs.charge_storage_and_rebate(),
                );
                assert_eq!(a.is_ok(), b.is_ok(), "{context}");
            }
            7 if rng.below(8) == 0 => {
                ours.adjust_computation_on_out_of_gas();
                theirs.adjust_computation_on_out_of_gas();
            }
            _ => {
                ours.reset_storage_cost_and_rebate();
                theirs.reset_storage_cost_and_rebate();
            }
        }
        same_summary(&ours, &theirs, &context);
    }
}

#[test]
fn gas_status_matches_sui_types() {
    let config = ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Unknown);
    let rgp = 1000;
    let mut rng = Rng(0x5eed_1234_abcd_0001);
    for i in 0..300 {
        let price = rgp + rng.below(5) * rng.below(100_000);
        let budget = rng.below(5_000_000_000) + 2_000_000;
        let ours = SuiGasStatus::new(budget, price, rgp, &config).unwrap();
        let theirs = Reference::new(budget, price, rgp, &config).unwrap();
        run(&mut rng, ours, theirs, &format!("metered {i}"));
    }
    for i in 0..50 {
        let ours = SuiGasStatus::new_unmetered();
        let theirs = Reference::new_unmetered(&config);
        run(&mut rng, ours, theirs, &format!("unmetered {i}"));
    }
}

#[test]
fn gas_price_checks_match_sui_types() {
    let config = ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Unknown);
    for price in [
        999,
        1000,
        config.max_gas_price() - 1,
        config.max_gas_price(),
    ] {
        let ours = SuiGasStatus::new(1_000_000, price, 1000, &config).is_ok();
        let theirs = Reference::new(1_000_000, price, 1000, &config).is_ok();
        assert_eq!(ours, theirs, "price {price}");
    }
}
