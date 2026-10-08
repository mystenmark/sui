// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Warm processors accept an Ed25519 or Secp256k1 transaction, validation
//! and signature verification both, without touching the heap, and answer
//! a resubmission from the signature cache without it either. Its own
//! binary, for the counting global allocator.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use std::sync::Arc;

use tokio::sync::oneshot;
use validator::epoch::EpochState;
use validator::processors::{Request, SignatureVerifier, TransactionValidator, answer};
use workqueue::Processor;

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

/// Runs one transaction through the processors, each one's output the next
/// one's input, counting their allocations; the request and its reply
/// channel are the handler's, made before counting. Returns whether it was
/// accepted.
fn process(
    epoch: &Arc<EpochState>,
    validator: &mut TransactionValidator,
    verifier: &mut SignatureVerifier,
    case: &common::Case,
) -> (bool, usize) {
    let (reply, verdict) = oneshot::channel();
    let request = Request::new(epoch.clone(), vec![case.parse().unwrap()], reply);
    let before = ALLOCATIONS.with(Cell::get);
    if let Some(valid) = validator.process(request)
        && let Some(verified) = verifier.process(valid)
    {
        answer(verified);
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
        .tx_signatures()
        .iter()
        .all(|s| matches!(s.0.first(), Some(0 | 1)))
}

#[test]
fn verifying_allocates_nothing_once_warm() {
    let (cases, epoch) = common::mainnet();
    let epoch = common::mainnet_epoch(epoch);
    let mut validator = TransactionValidator::new();
    let mut warm = SignatureVerifier::new();
    for case in &cases {
        process(&epoch, &mut validator, &mut warm, case);
    }
    let mut accepted = 0;
    for case in &cases {
        // A cache that has not seen it, made before counting: it is verified.
        let mut verifier = SignatureVerifier::new();
        let (ok, allocations) = process(&epoch, &mut validator, &mut verifier, case);
        assert_eq!(verifier.cache_stats().0, 0);
        if ok && plain_signatures(case) {
            accepted += 1;
            assert_eq!(allocations, 0, "{}", case.label);
        }
    }
    assert!(accepted > 0, "no Ed25519 or Secp256k1 transaction accepted");
    eprintln!("{accepted} of {} accepted without allocating", cases.len());
}

#[test]
fn a_cache_hit_allocates_nothing() {
    let (cases, epoch) = common::mainnet();
    let epoch = common::mainnet_epoch(epoch);
    let mut validator = TransactionValidator::new();
    let mut verifier = SignatureVerifier::new();
    let accepted: Vec<_> = cases
        .iter()
        .filter(|case| process(&epoch, &mut validator, &mut verifier, case).0)
        .collect();
    assert!(!accepted.is_empty());
    let (hits, _) = verifier.cache_stats();
    for case in &accepted {
        let (ok, allocations) = process(&epoch, &mut validator, &mut verifier, case);
        assert!(ok);
        assert_eq!(allocations, 0, "{}", case.label);
    }
    assert_eq!(verifier.cache_stats().0, hits + accepted.len() as u64);
}
