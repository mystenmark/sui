// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The reference's modes without dev-inspect, the one mode that tracks results; with one error
//! type, anchovy's `ExecutionError` (the reference also has a lighter `ExecutionFailure`).

use std::marker::PhantomData;

pub type TransactionIndex = usize;

pub trait ExecutionMode {
    /// Controls the calling of arbitrary Move functions
    fn allow_arbitrary_function_calls() -> bool;

    /// Controls the ability to instantiate any Move function parameter with a Pure call arg.
    ///  In other words, you can instantiate any struct or object or other value with its BCS byte
    fn allow_arbitrary_values() -> bool;

    /// Do not perform conservation checks after execution.
    fn skip_conservation_checks() -> bool;

    /// If not set, the package ID should be calculated like an object and an
    /// UpgradeCap is produced
    fn packages_are_predefined() -> bool;

    const TRACK_EXECUTION: bool;
}

#[derive(Copy, Clone)]
pub struct Normal(PhantomData<()>);

impl ExecutionMode for Normal {
    fn allow_arbitrary_function_calls() -> bool {
        false
    }

    fn allow_arbitrary_values() -> bool {
        false
    }

    fn skip_conservation_checks() -> bool {
        false
    }

    fn packages_are_predefined() -> bool {
        false
    }

    const TRACK_EXECUTION: bool = false;
}

#[derive(Copy, Clone)]
pub struct Genesis;

impl ExecutionMode for Genesis {
    fn allow_arbitrary_function_calls() -> bool {
        true
    }

    fn allow_arbitrary_values() -> bool {
        true
    }

    fn packages_are_predefined() -> bool {
        true
    }

    fn skip_conservation_checks() -> bool {
        false
    }

    const TRACK_EXECUTION: bool = false;
}

#[derive(Copy, Clone)]
pub struct System(PhantomData<()>);

/// Execution mode for executing a system transaction, including the epoch change
/// transaction and the consensus commit prologue. In this mode, we allow calls to
/// any function bypassing visibility.
impl ExecutionMode for System {
    fn allow_arbitrary_function_calls() -> bool {
        // allows bypassing visibility for system calls
        true
    }

    fn allow_arbitrary_values() -> bool {
        // For AuthenticatorStateUpdate, we need to be able to pass in a vector of
        // JWKs, so we need to allow arbitrary values.
        true
    }

    fn skip_conservation_checks() -> bool {
        false
    }

    fn packages_are_predefined() -> bool {
        true
    }

    const TRACK_EXECUTION: bool = false;
}
