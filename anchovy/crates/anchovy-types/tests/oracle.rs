// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Compares the transaction index and the effects change classes with what
//! sui-types derives, as written by `tools/sui-oracle` next to each corpus
//! checkpoint. The oracle's record format is documented in that tool.

use std::fmt::Write as _;
use std::path::Path;

use anchovy_types::Message;
use anchovy_types::checkpoint::CheckpointData;
use anchovy_types::effects::{ChangeKind, VersionedEffects};
use anchovy_types::transaction::TransactionKind;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        write!(s, "{b:02x}").unwrap();
        s
    })
}

/// Renders a checkpoint's transactions the way the oracle does.
fn render(checkpoint: &CheckpointData<'_>) -> String {
    let mut out = String::new();
    for (i, tx) in checkpoint.transactions.iter().enumerate() {
        let data = &tx.transaction.data;
        let index = &data.index;
        writeln!(out, "tx {i}").unwrap();
        for s in index.shared_inputs {
            writeln!(
                out,
                "shared {} {} {}",
                hex(&s.id.0),
                s.initial_shared_version.get(),
                s.mutability() as u8
            )
            .unwrap();
        }
        // The reference lists owned inputs, then packages; it errors instead
        // when an object is named twice, which the index leaves to validation.
        let no_duplicate = match data.kind {
            TransactionKind::ProgrammableTransaction(_)
            | TransactionKind::ProgrammableSystemTransaction(_) => {
                // The check covers the input arguments: owned inputs before
                // the gas payment, and shared inputs.
                let arguments = index.owned_inputs.len() - gas_count(data);
                let mut ids: Vec<_> = index.owned_inputs[..arguments]
                    .iter()
                    .map(|o| o.id)
                    .chain(index.shared_inputs.iter().map(|s| s.id))
                    .collect();
                ids.sort_unstable();
                ids.windows(2).all(|w| w[0] != w[1])
            }
            _ => true,
        };
        if no_duplicate {
            // The reference lists the gas payment after the packages.
            let arguments = index.owned_inputs.len() - gas_count(data);
            let (inputs, gas) = index.owned_inputs.split_at(arguments);
            for o in inputs {
                writeln!(
                    out,
                    "owned {} {} {}",
                    hex(&o.id.0),
                    o.version.get(),
                    hex(&o.digest.bytes)
                )
                .unwrap();
            }
            for p in index.packages {
                writeln!(out, "package {}", hex(&p.0)).unwrap();
            }
            for o in gas {
                writeln!(
                    out,
                    "owned {} {} {}",
                    hex(&o.id.0),
                    o.version.get(),
                    hex(&o.digest.bytes)
                )
                .unwrap();
            }
        } else {
            writeln!(out, "input_objects error").unwrap();
        }
        for o in index.receiving {
            writeln!(
                out,
                "receiving {} {} {}",
                hex(&o.id.0),
                o.version.get(),
                hex(&o.digest.bytes)
            )
            .unwrap();
        }
        for (i, call) in data.move_calls() {
            writeln!(
                out,
                "movecall {i} {} {} {}",
                hex(&call.package.0),
                call.module,
                call.function
            )
            .unwrap();
        }
        // The oracle can only reach the reference's reservations among the
        // inputs; those in the gas payment come last in the index.
        let gas_reservations = match data.kind {
            TransactionKind::ProgrammableTransaction(_) => {
                data.gas_data.payment.len() - gas_count(data)
            }
            _ => 0,
        };
        let inputs = index.coin_reservations.len() - gas_reservations;
        for o in &index.coin_reservations[..inputs] {
            writeln!(
                out,
                "reservation {} {} {}",
                hex(&o.id.0),
                o.version.get(),
                hex(&o.digest.bytes)
            )
            .unwrap();
        }
        writeln!(out, "withdrawals {}", index.funds_withdrawals.len()).unwrap();
        render_effects(&tx.effects.version, &mut out);
        writeln!(out, "end").unwrap();
    }
    out
}

fn render_effects(effects: &VersionedEffects<'_>, out: &mut String) {
    {
        match effects {
            VersionedEffects::V2(v2) => {
                let classes = [
                    ("created", ChangeKind::Created),
                    ("mutated", ChangeKind::Mutated),
                    ("unwrapped", ChangeKind::Unwrapped),
                    ("deleted", ChangeKind::Deleted),
                    ("unwrapped_then_deleted", ChangeKind::UnwrappedThenDeleted),
                    ("wrapped", ChangeKind::Wrapped),
                ];
                for (name, kind) in classes {
                    for c in v2.changes(kind) {
                        writeln!(out, "fx {name} {}", hex(&c.id.0)).unwrap();
                    }
                }
            }
            VersionedEffects::V1(v1) => {
                for (r, _) in v1.created {
                    writeln!(out, "fx created {}", hex(&r.id.0)).unwrap();
                }
                for (r, _) in v1.mutated {
                    writeln!(out, "fx mutated {}", hex(&r.id.0)).unwrap();
                }
                for (r, _) in v1.unwrapped {
                    writeln!(out, "fx unwrapped {}", hex(&r.id.0)).unwrap();
                }
                for r in v1.deleted {
                    writeln!(out, "fx deleted {}", hex(&r.id.0)).unwrap();
                }
                for r in v1.unwrapped_then_deleted {
                    writeln!(out, "fx unwrapped_then_deleted {}", hex(&r.id.0)).unwrap();
                }
                for r in v1.wrapped {
                    writeln!(out, "fx wrapped {}", hex(&r.id.0)).unwrap();
                }
            }
        }
    }
}

fn gas_count(data: &anchovy_types::transaction::TransactionData<'_>) -> usize {
    match data.kind {
        TransactionKind::ProgrammableTransaction(_) => data
            .gas_data
            .payment
            .iter()
            .filter(|o| !o.is_coin_reservation())
            .count(),
        _ => 0,
    }
}

#[test]
fn index_matches_sui() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/mainnet");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("no corpus at {}", dir.display());
        return;
    };
    let mut compared = 0;
    for entry in entries {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "chk") {
            continue;
        }
        let oracle = path.with_extension("oracle");
        let Ok(expected) = std::fs::read_to_string(&oracle) else {
            continue;
        };
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.remove(0);
        let checkpoint = Message::<CheckpointData<'static>>::parse(bytes).unwrap();
        let actual = render(checkpoint.get());
        if actual != expected {
            let expected_lines: Vec<&str> = expected.lines().collect();
            let mismatch = actual
                .lines()
                .enumerate()
                .position(|(i, a)| expected_lines.get(i) != Some(&a))
                .unwrap_or(0);
            let context = |s: &str| {
                s.lines()
                    .skip(mismatch.saturating_sub(3))
                    .take(8)
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            panic!(
                "{}: first difference at line {}\n--- anchovy\n{}\n--- sui\n{}",
                path.display(),
                mismatch + 1,
                context(&actual),
                context(&expected)
            );
        }
        compared += 1;
    }
    if compared == 0 {
        eprintln!("no oracle files; run tools/sui-oracle over the corpus first");
    } else {
        eprintln!("{compared} checkpoints compared with the oracle");
    }
}
