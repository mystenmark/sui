// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Views lent by `Kept` stay valid as more messages are kept.

use std::path::Path;

use messages::checkpoint::CheckpointData;
use messages::object::Object;
use messages::{Kept, Message};

#[test]
fn kept_views_outlive_later_keeps() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/mainnet-325300367.chk");
    let mut bytes = std::fs::read(path).unwrap();
    bytes.remove(0);
    let checkpoint = Message::<CheckpointData>::parse(bytes).unwrap();
    let originals: Vec<&[u8]> = checkpoint
        .get()
        .transactions
        .iter()
        .flat_map(|tx| tx.input_objects.iter().chain(tx.output_objects))
        .map(|o| o.bytes)
        .collect();

    let kept = Kept::<Object<'static>>::new();
    // Each keep may move the earlier messages as the keeper grows.
    let views: Vec<Object<'_>> = originals
        .iter()
        .map(|b| kept.keep(Message::parse(b.to_vec()).unwrap()))
        .collect();
    assert!(kept.len() > 16, "enough objects to grow the keeper");
    assert_eq!(views.len(), originals.len());
    for (i, view) in views.iter().enumerate() {
        assert_eq!(view.bytes, originals[i]);
    }
}
