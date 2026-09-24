// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Writes what sui-types derives from each transaction and effects in a
//! checkpoint, as text, for the messages tests to compare against. Usage:
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
//!
//! `sui-oracle --grpc-requests FILE` instead writes sample validator gRPC
//! requests, one `<type> <bcs hex>` line each.
//!
//! `sui-oracle --validity-vectors FILE` writes validity-check vectors; see
//! `validity.rs`. `sui-oracle --validity-corpus FILE.chk...` writes the
//! validity verdicts of real transactions; see `validity_corpus.rs`.
//! `sui-oracle --mutation-vectors FILE.gz` writes randomly mutated
//! transactions with the reference's verdicts; see `mutations.rs`.

use std::fmt::Write as _;

mod depth;
mod mutations;
mod signatures;
mod validity;
mod validity_corpus;
mod validity_kind;
mod validity_signed;
mod validity_verify;

use sui_types::effects::TransactionEffectsAPI;
use sui_types::full_checkpoint_content::CheckpointData;
use sui_types::transaction::{InputObjectKind, SharedObjectMutability, TransactionDataAPI};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn grpc_requests() -> String {
    use sui_types::base_types::{ObjectID, SequenceNumber};
    use sui_types::digests::TransactionDigest;
    use sui_types::messages_checkpoint::{CheckpointRequest, CheckpointRequestV2};
    use sui_types::messages_grpc::{
        LayoutGenerationOption, ObjectInfoRequest, ObjectInfoRequestKind, SystemStateRequest,
        TransactionInfoRequest,
    };

    let id = ObjectID::new(std::array::from_fn(|i| i as u8));
    let mut out = String::new();
    let mut line = |ty: &str, bytes: Vec<u8>| writeln!(out, "{ty} {}", hex(&bytes)).unwrap();
    for (generate_layout, request_kind) in [
        (
            LayoutGenerationOption::Generate,
            ObjectInfoRequestKind::LatestObjectInfo,
        ),
        (
            LayoutGenerationOption::None,
            ObjectInfoRequestKind::PastObjectInfoDebug(SequenceNumber::from_u64(u64::MAX - 1)),
        ),
    ] {
        let request = ObjectInfoRequest {
            object_id: id,
            generate_layout,
            request_kind,
        };
        line("ObjectInfoRequest", bcs::to_bytes(&request).unwrap());
    }
    let request = TransactionInfoRequest {
        transaction_digest: TransactionDigest::new([0xab; 32]),
    };
    line("TransactionInfoRequest", bcs::to_bytes(&request).unwrap());
    for sequence_number in [None, Some(0), Some(325_300_367)] {
        for request_content in [false, true] {
            let v1 = CheckpointRequest {
                sequence_number,
                request_content,
            };
            line("CheckpointRequest", bcs::to_bytes(&v1).unwrap());
            for certified in [false, true] {
                let v2 = CheckpointRequestV2 {
                    sequence_number,
                    request_content,
                    certified,
                };
                line("CheckpointRequestV2", bcs::to_bytes(&v2).unwrap());
            }
        }
    }
    for unused in [false, true] {
        let request = SystemStateRequest { _unused: unused };
        line("SystemStateRequest", bcs::to_bytes(&request).unwrap());
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [flag, path] = args.as_slice() {
        let out = match flag.as_str() {
            "--grpc-requests" => Some(grpc_requests()),
            "--validity-vectors" => Some(validity::vectors()),
            "--signature-vectors" => Some(signatures::vectors()),
            _ => None,
        };
        if let Some(out) = out {
            std::fs::write(path, out).unwrap();
            println!("{path}");
            return;
        }
    }
    if let [flag, path] = args.as_slice()
        && flag == "--depth-vectors"
    {
        std::fs::write(path, depth::vectors()).unwrap();
        println!("{path}");
        return;
    }
    if let [flag, path] = args.as_slice()
        && flag == "--mutation-vectors"
    {
        std::fs::write(path, mutations::vectors()).unwrap();
        println!("{path}");
        return;
    }
    if args.first().map(String::as_str) == Some("--validity-corpus") {
        for path in &args[1..] {
            let mut bytes = std::fs::read(path).unwrap();
            assert_eq!(bytes.remove(0), 1, "{path}: not a BCS blob");
            let checkpoint: CheckpointData = bcs::from_bytes(&bytes).unwrap();
            let out_path = path.replace(".chk", ".validity");
            std::fs::write(&out_path, validity_corpus::verdicts(&checkpoint)).unwrap();
            println!("{out_path}");
        }
        return;
    }
    for path in args {
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
