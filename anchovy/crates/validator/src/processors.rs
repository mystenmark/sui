// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The processors transaction work runs on, off the RPC runtime.

use std::sync::Arc;

use containers::Bump;
use messages::Message;
use messages::transaction::Transaction;
use tokio::sync::oneshot;
use workqueue::{Pool, Processor, Queue};

use crate::epoch::EpochState;

/// A decoded transaction to validate, and where the verdict goes.
pub struct ValidateTransaction {
    pub transaction: Message<Transaction<'static>>,
    pub reply: oneshot::Sender<Result<(), validation::Error>>,
}

/// Runs `TransactionData::validity_check`, nothing more for now. Each
/// thread's arena is reused for every transaction it validates.
pub struct TransactionValidator {
    epoch: Arc<EpochState>,
    bump: Bump,
}

/// Enough for the temporaries of nearly every transaction.
const ARENA_BYTES: usize = 64 * 1024;

impl TransactionValidator {
    pub fn new(epoch: Arc<EpochState>) -> TransactionValidator {
        TransactionValidator {
            epoch,
            bump: Bump::with_capacity(ARENA_BYTES),
        }
    }
}

impl Processor<ValidateTransaction> for TransactionValidator {
    fn process(&mut self, item: ValidateTransaction) {
        // Reset first, so an item that panicked leaves nothing behind.
        self.bump.reset();
        let data = &item.transaction.get().0.data;
        let result =
            validation::transaction_data::validity_check(data, &self.epoch.context(), &self.bump);
        // The handler may have given up; nothing to do then.
        let _ = item.reply.send(result);
    }
}

/// The processor pools, and the queues that feed them.
pub struct Processors {
    pub transactions: Queue<ValidateTransaction>,
    // Dropped last: stops and joins the threads.
    _validation: Pool,
}

/// Queued transactions beyond which submissions are refused.
const VALIDATION_QUEUE: usize = 4096;

impl Processors {
    pub fn start(epoch: &Arc<EpochState>, validation_threads: usize) -> Processors {
        let make = {
            let epoch = epoch.clone();
            move || TransactionValidator::new(epoch.clone())
        };
        let (transactions, validation) =
            Pool::spawn("validate", validation_threads, VALIDATION_QUEUE, make);
        Processors {
            transactions,
            _validation: validation,
        }
    }
}
