// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use super::{Built, Bump, Writer};
use crate::base::Digest;
use crate::effects::Event;

/// Builds `TransactionEvents`. Events are borrowed until `finish`, which
/// writes them all with their count.
pub struct EventsBuilder<'a> {
    bump: &'a Bump,
    events: containers::Vec<'a, Event<'a>>,
    bytes: usize,
}

impl<'a> EventsBuilder<'a> {
    pub fn new_in(bump: &'a Bump, expected: usize) -> EventsBuilder<'a> {
        EventsBuilder {
            bump,
            events: containers::Vec::with_capacity_in(expected, bump),
            bytes: 1,
        }
    }

    pub fn push(&mut self, event: Event<'a>) {
        // An id, a name, a sender, a type and the contents, roughly.
        self.bytes += 32 + 1 + event.transaction_module.len() + 32 + 64 + 5 + event.contents.len();
        self.events.push(event);
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn finish(self) -> Built<'a> {
        let mut w = Writer::new_in(self.bump, self.bytes);
        w.len_prefix(self.events.len());
        for e in &self.events {
            w.event(e);
        }
        w.finish("TransactionEvents")
    }
}

impl Writer<'_> {
    pub fn event(&mut self, e: &Event<'_>) {
        self.raw(&e.package_id.0);
        self.str(e.transaction_module);
        self.address(e.sender);
        self.struct_tag(&e.type_);
        self.bytes(e.contents);
    }
}

/// `Event::digest`: the hash of the event's encoding, without a type name.
pub fn event_digest(bump: &Bump, e: &Event<'_>) -> Digest {
    use blake2::Digest as _;
    let len = 32 + 1 + e.transaction_module.len() + 32 + 64 + 5 + e.contents.len();
    let mut w = Writer::new_in(bump, len);
    w.event(e);
    let mut hasher = blake2::Blake2b::<blake2::digest::consts::U32>::new();
    hasher.update(w.finish_bytes());
    Digest::new(hasher.finalize().into())
}
