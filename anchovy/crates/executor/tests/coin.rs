// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Coin value edits against sui-types': the edited object encodes to the same bytes.

mod common;

use common::*;
use containers::Bump;
use sui_types::object::Owner;

#[test]
fn deduct_gas_matches_reference() {
    let mut rng = Rng(7);
    for _ in 0..500 {
        let bump = Bump::with_capacity(1 << 12);
        let id = rng.id();
        let owner = Owner::AddressOwner(rng.address());
        let version = 1 + rng.below(1000);
        let mut reference = coin(&mut rng, id, version, owner);
        let balance = reference.get_coin_value_unsafe();
        let charge = if rng.below(2) == 0 {
            -i64::try_from(rng.below(1 << 30)).unwrap()
        } else {
            i64::try_from(rng.below(balance + 1)).unwrap()
        };

        let port = stored_object(&bump, &reference);
        let port = executor::gas::deduct_gas(&bump, &port, charge).seal(&bump);
        sui_types::gas::deduct_gas(&mut reference, charge);

        assert_eq!(
            port.stored_bytes().unwrap(),
            bcs::to_bytes(&reference).unwrap().as_slice()
        );
        assert_eq!(
            executor::gas::get_coin_value_unsafe(port.try_as_move().unwrap()),
            reference.get_coin_value_unsafe()
        );
    }
}
