// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use consensus::Block;
use consensus_core::{TransactionVerifier, ValidationError};
use consensus_types::block::{BlockRef, TransactionIndex};
use tokio::sync::oneshot;
use validator::consensus::vote::VoteRequest;
use workqueue::{PushError, Queue};

use crate::commits::block_ref;

/// How long to wait before offering a block again to a full voter.
const RETRY: Duration = Duration::from_millis(1);

/// consensus-core's verifier over the pipeline's voter.
pub(crate) struct Verifier {
    votes: Queue<VoteRequest>,
}

impl Verifier {
    pub(crate) fn new(votes: Queue<VoteRequest>) -> Verifier {
        Verifier { votes }
    }
}

impl TransactionVerifier for Verifier {
    fn verify_batch(&self, block: &BlockRef, batch: &[&[u8]]) -> Result<(), ValidationError> {
        self.verify_and_vote_batch(block, batch).map(drop)
    }

    /// consensus-core calls this from blocking threads and, for its own
    /// blocks, inline in async code, where tokio's `blocking_recv` panics;
    /// `futures::executor::block_on` only parks the thread.
    fn verify_and_vote_batch(
        &self,
        block: &BlockRef,
        batch: &[&[u8]],
    ) -> Result<Vec<TransactionIndex>, ValidationError> {
        // Only when the epoch's pipeline is gone, after consensus stops.
        let stopping = || ValidationError::InvalidTransaction("the epoch has stopped".to_owned());
        let (reply, verdict) = oneshot::channel();
        let mut request = VoteRequest {
            block: Block {
                reference: block_ref(block),
                transactions: batch.iter().map(|t| t.to_vec()).collect(),
            },
            reply,
        };
        loop {
            match self.votes.try_push(request) {
                Ok(()) => break,
                Err(PushError::Full(refused)) => {
                    request = refused;
                    std::thread::sleep(RETRY);
                }
                Err(PushError::Closed(_)) => return Err(stopping()),
            }
        }
        #[allow(
            clippy::disallowed_methods,
            reason = "tokio's block_on panics inside a runtime"
        )]
        let verdict = futures::executor::block_on(verdict);
        match verdict {
            Ok(Ok(rejected)) => Ok(rejected),
            Ok(Err(invalid)) => Err(ValidationError::InvalidTransaction(format!(
                "transaction {}: {}",
                invalid.index, invalid.reason
            ))),
            Err(_) => Err(stopping()),
        }
    }
}
