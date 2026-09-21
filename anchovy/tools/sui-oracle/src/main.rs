// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Writes what sui-types derives from each transaction and effects in a
//! checkpoint, as text, for anchovy's tests to compare against. Usage:
//! `sui-oracle FILE.chk...` writes `FILE.oracle` next to each input.
//!
//! A `summary <checkpoint digest>` line, then one record per transaction,
//! in checkpoint order:
//!
//! ```text
//! tx <index>
//! shared <id> <initial version> <mutability>
//! owned <id> <version> <digest>
//! package <id>
//! receiving <id> <version> <digest>
//! movecall <command index> <package> <module> <function>
//! reservation <id> <version> <digest>
//! withdrawals <count>
//! input_objects error
//! fx <created|mutated|unwrapped|deleted|unwrapped_then_deleted|wrapped> <id>
//! end
//! ```
//!
//! Hex has no prefix. Lines within a record keep the reference's order.

use std::fmt::Write as _;

use sui_types::effects::TransactionEffectsAPI;
use sui_types::full_checkpoint_content::CheckpointData;
use sui_types::transaction::{InputObjectKind, SharedObjectMutability, TransactionDataAPI};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn main() {
    for path in std::env::args().skip(1) {
        let mut bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.remove(0), 1, "{path}: not a BCS blob");
        let checkpoint: CheckpointData = bcs::from_bytes(&bytes).unwrap();

        let mut out = String::new();
        writeln!(
            out,
            "summary {}",
            hex(checkpoint.checkpoint_summary.digest().inner())
        )
        .unwrap();
        for (i, tx) in checkpoint.transactions.iter().enumerate() {
            let data = tx.transaction.transaction_data();
            writeln!(out, "tx {i}").unwrap();
            for s in data.shared_input_objects() {
                let mutability = match s.mutability {
                    SharedObjectMutability::Immutable => 0,
                    SharedObjectMutability::Mutable => 1,
                    SharedObjectMutability::NonExclusiveWrite => 2,
                };
                writeln!(
                    out,
                    "shared {} {} {mutability}",
                    hex(&s.id.into_bytes()),
                    s.initial_shared_version.value()
                )
                .unwrap();
            }
            match data.input_objects() {
                Ok(inputs) => {
                    for input in inputs {
                        match input {
                            InputObjectKind::ImmOrOwnedMoveObject((id, v, d)) => writeln!(
                                out,
                                "owned {} {} {}",
                                hex(&id.into_bytes()),
                                v.value(),
                                hex(d.inner())
                            )
                            .unwrap(),
                            InputObjectKind::MovePackage(id) => {
                                writeln!(out, "package {}", hex(&id.into_bytes())).unwrap();
                            }
                            InputObjectKind::SharedMoveObject { .. } => {}
                        }
                    }
                }
                Err(_) => writeln!(out, "input_objects error").unwrap(),
            }
            for (id, v, d) in data.receiving_objects() {
                writeln!(
                    out,
                    "receiving {} {} {}",
                    hex(&id.into_bytes()),
                    v.value(),
                    hex(d.inner())
                )
                .unwrap();
            }
            for (i, package, module, function) in data.move_calls() {
                writeln!(
                    out,
                    "movecall {i} {} {module} {function}",
                    hex(&package.into_bytes())
                )
                .unwrap();
            }
            for (id, v, d) in data.kind().get_coin_reservation_obj_refs() {
                writeln!(
                    out,
                    "reservation {} {} {}",
                    hex(&id.into_bytes()),
                    v.value(),
                    hex(d.inner())
                )
                .unwrap();
            }
            let withdrawals = data.kind().get_funds_withdrawals().count();
            writeln!(out, "withdrawals {withdrawals}").unwrap();

            let fx = &tx.effects;
            let classes: [(&str, Vec<sui_types::base_types::ObjectID>); 6] = [
                ("created", fx.created().iter().map(|(r, _)| r.0).collect()),
                ("mutated", fx.mutated().iter().map(|(r, _)| r.0).collect()),
                (
                    "unwrapped",
                    fx.unwrapped().iter().map(|(r, _)| r.0).collect(),
                ),
                ("deleted", fx.deleted().iter().map(|r| r.0).collect()),
                (
                    "unwrapped_then_deleted",
                    fx.unwrapped_then_deleted().iter().map(|r| r.0).collect(),
                ),
                ("wrapped", fx.wrapped().iter().map(|r| r.0).collect()),
            ];
            for (class, ids) in classes {
                for id in ids {
                    writeln!(out, "fx {class} {}", hex(&id.into_bytes())).unwrap();
                }
            }
            writeln!(out, "end").unwrap();
        }
        let out_path = path.replace(".chk", ".oracle");
        std::fs::write(&out_path, out).unwrap();
        println!("{out_path}");
    }
}
