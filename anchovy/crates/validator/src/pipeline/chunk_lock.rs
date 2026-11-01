// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Which front end runs each settlement chunk (`docs/state_sync_design.md`,
//! "The chunk lock"): the commit stage, for the chunks of the commits it
//! handles; the synced scheduler, for chunks of certified checkpoints past
//! them. The two never run one chunk at once: a chunk of the commit stage's
//! that the synced scheduler holds waits in the randomness gate until
//! released.

use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard, PoisonError};

pub struct ChunkLock {
    state: Mutex<State>,
}

struct State {
    /// The last chunk height the commit stage assigned.
    handled: u64,
    /// The last checkpoint it cut, and that checkpoint's last chunk height.
    cut: Cut,
    /// The chunk heights the synced scheduler is running.
    synced: BTreeSet<u64>,
}

/// The last checkpoint the commit stage cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cut {
    pub seq: u64,
    /// Its last chunk's height.
    pub height: u64,
}

impl ChunkLock {
    /// After the commit stage handled the chunks through `handled`, and cut
    /// the checkpoints through `cut`.
    pub fn new(handled: u64, cut: Cut) -> ChunkLock {
        ChunkLock {
            state: Mutex::new(State {
                handled,
                cut,
                synced: BTreeSet::new(),
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The commit stage claims the chunks through `handled`, having cut the
    /// checkpoints through `cut`, before emitting them: the synced
    /// scheduler takes none of them from now on.
    pub fn claim(&self, handled: u64, cut: Cut) {
        let mut state = self.state();
        debug_assert!(handled >= state.handled && cut.seq >= state.cut.seq);
        state.handled = handled;
        state.cut = cut;
    }

    /// Whether the synced scheduler is running the chunk at `height`.
    pub fn held(&self, height: u64) -> bool {
        self.state().synced.contains(&height)
    }

    /// The last checkpoint the commit stage cut: every chunk of it and of
    /// those before it is the commit stage's.
    pub fn cut(&self) -> Cut {
        self.state().cut
    }

    /// The synced scheduler takes the chunks `first..=last` of a certified
    /// checkpoint past the last cut: those above the commit stage's last
    /// height it runs, until it releases each. Returns that height: the
    /// chunks through it are the commit stage's.
    pub fn take(&self, first: u64, last: u64) -> u64 {
        let mut state = self.state();
        let handled = state.handled;
        state.synced.extend(first.max(handled + 1)..=last);
        handled
    }

    /// The synced scheduler has run the chunk at `height`. The randomness
    /// gate is told after: it may hold the chunk.
    pub fn release(&self, height: u64) {
        let removed = self.state().synced.remove(&height);
        debug_assert!(removed, "chunk {height} was taken");
    }
}
