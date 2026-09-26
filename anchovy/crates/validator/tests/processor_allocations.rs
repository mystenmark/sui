// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Warm processors accept an Ed25519 or Secp256k1 transaction, validation
//! and signature verification both, without touching the heap. Its own
//! binary, for the counting global allocator.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use tokio::sync::oneshot;
use validator::processors::{
    SignatureVerifier, TransactionValidator, ValidateTransactions, VerifySignatures,
};
use workqueue::{Inbox, Processor};

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.with(|c| c.set(c.get() + 1));
        // SAFETY: the caller's contract, passed on.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// Runs one transaction through both processors, counting their
/// allocations; the request and its reply channel are the handler's, made
/// before counting. Returns whether it was accepted.
fn process(
    validator: &mut TransactionValidator,
    verifier: &mut SignatureVerifier,
    verification: &Inbox<VerifySignatures>,
    case: &common::Case,
) -> (bool, usize) {
    let (reply, verdict) = oneshot::channel();
    let item = ValidateTransactions {
        transactions: vec![case.parse().unwrap()],
        reply,
    };
    let before = ALLOCATIONS.with(Cell::get);
    validator.process(item);
    if let Some(next) = verification.try_pop() {
        verifier.process(next);
    }
    let allocations = ALLOCATIONS.with(Cell::get) - before;
    (verdict.blocking_recv().unwrap().is_ok(), allocations)
}

/// Whether every signature is a single Ed25519 or Secp256k1 one, by flag.
fn plain_signatures(case: &common::Case) -> bool {
    let transaction = case.parse().unwrap();
    transaction
        .get()
        .0
        .tx_signatures
        .iter()
        .all(|s| matches!(s.0.first(), Some(0 | 1)))
}

#[test]
fn accepting_allocates_nothing_once_warm() {
    let (cases, epoch) = common::mainnet();
    let epoch = common::mainnet_epoch(epoch);
    let (signatures, verification) = workqueue::queue(1);
    let mut validator = TransactionValidator::new(epoch.clone(), signatures);
    let mut verifier = SignatureVerifier::new(epoch);

    for case in &cases {
        process(&mut validator, &mut verifier, &verification, case);
    }
    let mut accepted = 0;
    for case in &cases {
        let (ok, allocations) = process(&mut validator, &mut verifier, &verification, case);
        if ok && plain_signatures(case) {
            accepted += 1;
            assert_eq!(allocations, 0, "{}", case.label);
        }
    }
    assert!(accepted > 0, "no Ed25519 or Secp256k1 transaction accepted");
    eprintln!("{accepted} of {} accepted without allocating", cases.len());
}
