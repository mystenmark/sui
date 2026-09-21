// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Writes seeds for the `parse` fuzz target into `fuzz/corpus/parse/`, cut
//! from the checked-in mainnet checkpoint. Each seed is the target's type
//! selector byte followed by one message.

use std::path::Path;

use anchovy_types::Message;
use anchovy_types::checkpoint::CheckpointData;

fn main() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut checkpoint = std::fs::read(manifest.join("tests/data/mainnet-325300367.chk")).unwrap();
    checkpoint.remove(0);
    let parsed = Message::<CheckpointData>::parse(checkpoint.clone()).unwrap();
    let view = parsed.get();

    let mut seeds: Vec<(u8, &[u8])> = vec![
        (5, view.checkpoint_contents.bytes),
        (6, view.checkpoint_summary.bytes),
        (8, &checkpoint),
    ];
    for tx in view.transactions {
        seeds.push((0, tx.transaction.bytes));
        seeds.push((1, tx.transaction.data.bytes));
        seeds.push((2, tx.effects.bytes));
        if let Some(events) = &tx.events {
            seeds.push((3, events.bytes));
        }
        for object in tx.input_objects.iter().chain(tx.output_objects) {
            seeds.push((4, object.bytes));
        }
    }

    let dir = manifest.join("../../fuzz/corpus/parse");
    std::fs::create_dir_all(&dir).unwrap();
    for (i, (selector, bytes)) in seeds.iter().enumerate() {
        let mut seed = vec![*selector];
        seed.extend_from_slice(bytes);
        std::fs::write(dir.join(format!("seed-{selector}-{i}")), seed).unwrap();
    }
    println!("{} seeds in {}", seeds.len(), dir.display());
}
