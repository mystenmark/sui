// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A warm validation processor accepts a transaction without touching the
//! heap. Its own binary, for the counting global allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use messages::Message;
use messages::base::Digest;
use messages::transaction::{DigestPending, Transaction};
use protocol_config::{Chain, ProtocolVersion};
use tokio::sync::oneshot;
use validator::epoch::EpochState;
use validator::processors::{TransactionValidator, ValidateTransactions};
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

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn as_transaction(data: &[u8]) -> Vec<u8> {
    let mut out = vec![1, 0, 0, 0];
    out.extend_from_slice(data);
    out.push(0);
    out
}

/// Validates `transaction`, counting the processor's allocations; the work
/// item and its reply channel are the handler's, made before counting.
fn process(validator: &mut TransactionValidator, transaction: &[u8]) -> (bool, usize) {
    let transaction = Message::<Transaction<DigestPending>>::parse(transaction.to_vec())
        .map_err(|(e, _)| e)
        .unwrap();
    let (reply, verdict) = oneshot::channel();
    let item = ValidateTransactions {
        transactions: vec![transaction],
        reply,
    };
    let before = ALLOCATIONS.with(Cell::get);
    validator.process(item);
    let allocations = ALLOCATIONS.with(Cell::get) - before;
    (verdict.blocking_recv().unwrap().is_ok(), allocations)
}

#[test]
fn accepting_allocates_nothing_once_warm() {
    let epoch = Arc::new(EpochState::new(
        Chain::Unknown,
        ProtocolVersion::MAX.as_u64(),
        5,
        Digest::new([0x11; 32]),
        1000,
        4,
    ));
    let mut validator = TransactionValidator::new(epoch);
    let transactions: Vec<Vec<u8>> = include_str!("../../validation/tests/data/validity.vectors")
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            let ["tx", _, "tx_data", _, hex] = fields[..] else {
                return None;
            };
            let bytes = as_transaction(&unhex(hex));
            Message::<Transaction<DigestPending>>::parse(bytes.clone()).ok()?;
            Some(bytes)
        })
        .collect();

    for transaction in &transactions {
        process(&mut validator, transaction);
    }
    let mut accepted = 0;
    for transaction in &transactions {
        let (ok, allocations) = process(&mut validator, transaction);
        if ok {
            accepted += 1;
            assert_eq!(allocations, 0, "accepted transaction {accepted}");
        }
    }
    assert!(accepted > 50, "only {accepted} accepted");
}
