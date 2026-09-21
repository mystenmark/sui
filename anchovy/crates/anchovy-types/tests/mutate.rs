// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A deterministic mutation fuzzer that runs as a test. Seeds are real
//! mainnet messages; each is damaged a little and parsed. Parsing must not
//! panic, must not allocate more than a fixed multiple of the input, and
//! whatever it accepts must be self-consistent.

use std::path::Path;

use anchovy_types::checkpoint::{CertifiedCheckpointSummary, CheckpointContents, CheckpointData};
use anchovy_types::effects::{TransactionEffects, TransactionEvents};
use anchovy_types::message::{MAX_ARENA_PER_WIRE_BYTE, MIN_ARENA_GUESS};
use anchovy_types::object::Object;
use anchovy_types::transaction::TransactionData;
use anchovy_types::{Message, Wire};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Bytes that matter to a BCS parser: tags, small lengths, and the edges of
/// the uleb128 encoding.
const INTERESTING: [u8; 12] = [0, 1, 2, 3, 7, 32, 33, 0x7f, 0x80, 0x81, 0xfe, 0xff];

fn mutate(seed: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut out = seed.to_vec();
    for _ in 0..=rng.below(3) {
        if out.is_empty() {
            break;
        }
        let at = rng.below(out.len());
        match rng.below(6) {
            0 => out[at] ^= 1 << rng.below(8),
            1 => out[at] = INTERESTING[rng.below(INTERESTING.len())],
            2 => out[at] = rng.next() as u8,
            3 => out.truncate(at),
            4 => {
                out.remove(at);
            }
            _ => out.insert(at, INTERESTING[rng.below(INTERESTING.len())]),
        }
    }
    out
}

/// Parses `bytes` as `T` and checks what must hold whether or not it parses.
fn parse_checked<T: Wire>(bytes: Vec<u8>) -> Option<Message<T>> {
    let len = bytes.len();
    match Message::<T>::parse(bytes) {
        Ok(m) => {
            assert!(m.arena_used() <= len * MAX_ARENA_PER_WIRE_BYTE);
            // A guessed arena, or a fallback arena of exactly what is used.
            let guess = (len * T::ARENA_GUESS_SIXTEENTHS / 16).max(MIN_ARENA_GUESS);
            assert!(m.arena_size() == guess || m.arena_size() == m.arena_used());
            Some(m)
        }
        Err((_, returned)) => {
            assert_eq!(returned.as_slice().len(), len);
            None
        }
    }
}

struct Seeds {
    checkpoint: Vec<u8>,
    summary: Vec<u8>,
    contents: Vec<u8>,
    transactions: Vec<Vec<u8>>,
    effects: Vec<Vec<u8>>,
    events: Vec<Vec<u8>>,
    objects: Vec<Vec<u8>>,
}

fn seeds() -> Seeds {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/mainnet-325300367.chk");
    let mut checkpoint = std::fs::read(path).unwrap();
    checkpoint.remove(0);
    let parsed = Message::<CheckpointData>::parse(checkpoint.clone()).unwrap();
    let mut seeds = Seeds {
        checkpoint,
        summary: parsed.get().checkpoint_summary.bytes.to_vec(),
        contents: parsed.get().checkpoint_contents.bytes.to_vec(),
        transactions: Vec::new(),
        effects: Vec::new(),
        events: Vec::new(),
        objects: Vec::new(),
    };
    for tx in parsed.get().transactions {
        seeds.transactions.push(tx.transaction.data.bytes.to_vec());
        seeds.effects.push(tx.effects.bytes.to_vec());
        if let Some(events) = tx.events {
            seeds.events.push(events.bytes.to_vec());
        }
        for object in tx.input_objects.iter().chain(tx.output_objects) {
            seeds.objects.push(object.bytes.to_vec());
        }
    }
    seeds
}

fn iterations() -> usize {
    std::env::var("ANCHOVY_MUTATE_ITERATIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000)
}

#[test]
fn transactions() {
    let seeds = seeds();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut accepted = 0;
    for _ in 0..iterations() {
        let seed = &seeds.transactions[rng.below(seeds.transactions.len())];
        let Some(m) = parse_checked::<TransactionData>(mutate(seed, &mut rng)) else {
            continue;
        };
        accepted += 1;
        let data = m.get();
        assert_eq!(data.bytes, m.wire_bytes());
        assert_eq!(data.move_calls().count(), data.index.move_calls.len());
    }
    eprintln!("{accepted} mutated transactions accepted");
    assert!(accepted > 0);
}

#[test]
fn effects_events_objects() {
    let seeds = seeds();
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for _ in 0..iterations() {
        let seed = &seeds.effects[rng.below(seeds.effects.len())];
        if let Some(m) = parse_checked::<TransactionEffects>(mutate(seed, &mut rng)) {
            assert_eq!(m.get().bytes, m.wire_bytes());
        }
        let seed = &seeds.events[rng.below(seeds.events.len())];
        if let Some(m) = parse_checked::<TransactionEvents>(mutate(seed, &mut rng)) {
            assert_eq!(m.get().bytes, m.wire_bytes());
        }
        let seed = &seeds.objects[rng.below(seeds.objects.len())];
        if let Some(m) = parse_checked::<Object>(mutate(seed, &mut rng)) {
            assert_eq!(m.get().bytes, m.wire_bytes());
        }
    }
}

#[test]
fn checkpoints() {
    let seeds = seeds();
    let mut rng = Rng(0xda94_2042_e4dd_58b5);
    let mut accepted = 0;
    for _ in 0..iterations() / 100 {
        if parse_checked::<CheckpointContents>(mutate(&seeds.contents, &mut rng)).is_some() {
            accepted += 1;
        }
        if parse_checked::<CertifiedCheckpointSummary>(mutate(&seeds.summary, &mut rng)).is_some() {
            accepted += 1;
        }
        let Some(m) = parse_checked::<CheckpointData>(mutate(&seeds.checkpoint, &mut rng)) else {
            continue;
        };
        accepted += 1;
        for tx in m.get().transactions {
            let alone = parse_checked::<TransactionData>(tx.transaction.data.bytes.to_vec())
                .expect("a span the checkpoint parser accepted");
            assert_eq!(*alone.get(), tx.transaction.data);
        }
    }
    assert!(accepted > 0);
}

/// Any byte string at all, as every root type.
#[test]
fn noise() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for _ in 0..iterations() {
        let len = rng.below(200);
        let bytes: Vec<u8> = (0..len)
            .map(|_| {
                if rng.below(3) == 0 {
                    INTERESTING[rng.below(INTERESTING.len())]
                } else {
                    rng.next() as u8
                }
            })
            .collect();
        parse_checked::<TransactionData>(bytes.clone());
        parse_checked::<TransactionEffects>(bytes.clone());
        parse_checked::<TransactionEvents>(bytes.clone());
        parse_checked::<Object>(bytes.clone());
        parse_checked::<CheckpointData>(bytes);
    }
}
