// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `sui_types::execution_params`: what is decided about a transaction before it runs.

use containers::{Bump, Vec};
use messages::base::{SequenceNumber, TransactionDigest};
use messages::execution_status::ExecutionErrorKind;

use crate::inputs::{CONGESTED, ExecutionInputs, RANDOMNESS_UNAVAILABLE};

/// Execution inputs computed before running a transaction: whether to fail it early (and with
/// which errors), plus context for gas charging. An execution input only - never serialized into
/// `TransactionEffects`, so adding fields here does not change effects or their digests.
#[derive(Debug)]
pub struct ExecutionOrEarlyError<'a> {
    /// Non-empty when present (the reference's `NonEmpty`).
    early_errors: Option<Vec<'a, ExecutionErrorKind<'a>>>,
    /// Accumulator (settlement) root version assigned to this transaction. Gates the mainnet
    /// address-balance gas-smash short-circuit. Populated only for mainnet committed execution;
    /// `None` elsewhere, leaving that gate inert.
    accumulator_version: Option<SequenceNumber>,
}

impl<'a> ExecutionOrEarlyError<'a> {
    /// Execute the transaction normally (no predetermined early error).
    pub fn ok(accumulator_version: Option<SequenceNumber>) -> Self {
        Self {
            early_errors: None,
            accumulator_version,
        }
    }

    /// Skip execution and fail the transaction with `errors`, which must not be empty.
    ///
    /// # Panics
    /// If `errors` is empty.
    pub fn failed(
        errors: Vec<'a, ExecutionErrorKind<'a>>,
        accumulator_version: Option<SequenceNumber>,
    ) -> Self {
        assert!(!errors.is_empty(), "early errors are not empty");
        Self {
            early_errors: Some(errors),
            accumulator_version,
        }
    }

    pub fn is_ok(&self) -> bool {
        self.early_errors.is_none()
    }

    /// The predetermined early errors, if any.
    pub fn early_errors(&self) -> Option<&[ExecutionErrorKind<'a>]> {
        self.early_errors.as_deref()
    }

    /// The first early error, if any: the one the transaction fails with.
    pub fn head(&self) -> Option<ExecutionErrorKind<'a>> {
        self.early_errors.as_ref().and_then(|e| e.first().copied())
    }

    pub fn accumulator_version(&self) -> Option<SequenceNumber> {
        self.accumulator_version
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FundsWithdrawStatus {
    /// Either we don't know yet whether the funds withdrawals are sufficient or not,
    /// or we know for sure that the funds withdrawals are sufficient.
    /// The reason we don't need to distinguish between unknown and sufficient funds is that
    /// in either case we would have to go ahead and execute the transaction anyway.
    MaybeSufficient,
    /// We know for sure that the funds withdrawals in this transaction do not all have enough funds.
    /// This takes account of both address and object funds withdrawals.
    Insufficient,
}

/// Determine if a transaction is predetermined to fail execution.
/// Returns all matching error kinds, or `None` if there is no early failure.
/// When we pass this to the execution engine, we will not execute the transaction
/// if it is predetermined to fail execution.
///
/// # Panics
/// On a cancellation reason other than congestion or randomness, as the reference does.
pub fn get_early_execution_error<'a>(
    bump: &'a Bump,
    transaction_digest: &TransactionDigest,
    input_objects: &ExecutionInputs<'a>,
    is_certificate_denied: impl FnOnce(&TransactionDigest) -> bool,
    funds_withdraw_status: &FundsWithdrawStatus,
) -> Option<Vec<'a, ExecutionErrorKind<'a>>> {
    let mut errors = Vec::new_in(bump);
    if is_certificate_denied(transaction_digest) {
        errors.push(ExecutionErrorKind::CertificateDenied);
    }

    if !input_objects.consensus_stream_ended_objects().is_empty() {
        errors.push(ExecutionErrorKind::InputObjectDeleted);
    }

    let cancelled_objects = input_objects.get_cancelled_objects(bump);
    if let Some((cancelled_objects, reason)) = cancelled_objects {
        match reason {
            CONGESTED => {
                errors.push(
                    ExecutionErrorKind::ExecutionCancelledDueToSharedObjectCongestion {
                        congested_objects: cancelled_objects.leak(),
                    },
                );
            }
            RANDOMNESS_UNAVAILABLE => {
                errors.push(ExecutionErrorKind::ExecutionCancelledDueToRandomnessUnavailable);
            }
            _ => panic!("invalid cancellation reason SequenceNumber: {reason}"),
        }
    }

    if matches!(funds_withdraw_status, FundsWithdrawStatus::Insufficient) {
        errors.push(ExecutionErrorKind::InsufficientFundsForWithdraw);
    }

    (!errors.is_empty()).then_some(errors)
}
