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

/// A request's decoded transactions to validate, and where the verdict
/// goes: the first failure, in order, or success. One item per request, as
/// the handoff costs far more than validating a transaction does.
pub struct ValidateTransactions {
    pub transactions: Vec<Message<Transaction<'static>>>,
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

impl Processor<ValidateTransactions> for TransactionValidator {
    fn process(&mut self, item: ValidateTransactions) {
        let context = self.epoch.context();
        let result = item.transactions.iter().try_for_each(|transaction| {
            // Reset first, so an item that panicked leaves nothing behind.
            self.bump.reset();
            validation::transaction_data::validity_check(
                &transaction.get().0.data,
                &context,
                &self.bump,
            )
        });
        // The handler may have given up; nothing to do then.
        let _ = item.reply.send(result);
    }
}

/// The processors, and the queues that feed them. Validation runs on one
/// thread for now.
pub struct Processors {
    pub transactions: Queue<ValidateTransactions>,
    // Dropped last: stops and joins the thread.
    _validation: Pool<ValidateTransactions>,
}

/// Queued requests beyond which submissions are refused.
pub const VALIDATION_QUEUE: usize = 4096;

impl Processors {
    pub fn start(epoch: &Arc<EpochState>, validation_queue: usize) -> Processors {
        let make = {
            let epoch = epoch.clone();
            move || TransactionValidator::new(epoch.clone())
        };
        let (transactions, validation) = Pool::spawn("validate", 1, validation_queue, make);
        Processors {
            transactions,
            _validation: validation,
        }
    }
}
