// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Checks the parse-time transaction index against the same data gathered
//! by walking the parsed transaction, the way the reference computes it.

use std::collections::BTreeSet;
use std::path::Path;

use anchovy_types::Message;
use anchovy_types::base::{ObjectId, ObjectRef};
use anchovy_types::checkpoint::CheckpointData;
use anchovy_types::transaction::{
    CallArg, Command, ObjectArg, ProgrammableTransaction, SharedObjectArg, TransactionData,
    TransactionKind,
};
use anchovy_types::type_tag::TypeInput;

fn type_packages(ty: &TypeInput<'_>, out: &mut BTreeSet<ObjectId>) {
    match ty {
        TypeInput::Vector(inner) => type_packages(inner, out),
        TypeInput::Struct(s) => {
            out.insert(ObjectId(s.address.0));
            for p in s.type_params {
                type_packages(p, out);
            }
        }
        _ => {}
    }
}

#[derive(Default, Debug, PartialEq)]
struct Walked {
    shared: Vec<SharedObjectArg>,
    owned: Vec<ObjectRef>,
    packages: Vec<ObjectId>,
    receiving: Vec<ObjectRef>,
    move_calls: Vec<u32>,
    funds_withdrawals: usize,
    coin_reservations: Vec<ObjectRef>,
}

fn walk_pt(pt: &ProgrammableTransaction<'_>, user: bool, out: &mut Walked) {
    for input in pt.inputs {
        match input {
            CallArg::Pure(_) => {}
            CallArg::Object(ObjectArg::ImmOrOwnedObject(o)) if o.is_coin_reservation() => {
                if user {
                    out.coin_reservations.push(**o);
                }
            }
            CallArg::Object(ObjectArg::ImmOrOwnedObject(o)) => out.owned.push(**o),
            CallArg::Object(ObjectArg::SharedObject(s)) => out.shared.push(**s),
            CallArg::Object(ObjectArg::Receiving(o)) => {
                if user {
                    out.receiving.push(**o);
                }
            }
            CallArg::FundsWithdrawal(_) => {
                if user {
                    out.funds_withdrawals += 1;
                }
            }
        }
    }
    let mut packages = BTreeSet::new();
    for (i, command) in pt.commands.iter().enumerate() {
        match command {
            Command::MoveCall(call) => {
                if user {
                    out.move_calls.push(i as u32);
                }
                packages.insert(*call.package);
                for ty in call.type_arguments {
                    type_packages(ty, &mut packages);
                }
            }
            Command::Publish(_, deps) => packages.extend(deps.iter().copied()),
            Command::Upgrade(_, deps, package, _) => {
                packages.extend(deps.iter().copied());
                packages.insert(**package);
            }
            Command::MakeMoveVec(Some(ty), _) => type_packages(ty, &mut packages),
            _ => {}
        }
    }
    out.packages = packages.into_iter().collect();
}

/// Returns false for kinds this walk does not cover.
fn walk(data: &TransactionData<'_>, out: &mut Walked) -> bool {
    match &data.kind {
        TransactionKind::ProgrammableTransaction(pt) => {
            walk_pt(pt, true, out);
            for o in data.gas_data.payment {
                if o.is_coin_reservation() {
                    out.coin_reservations.push(*o);
                } else {
                    out.owned.push(*o);
                }
            }
            true
        }
        TransactionKind::ProgrammableSystemTransaction(pt) => {
            walk_pt(pt, false, out);
            // Reservations in the gas payment count for every kind.
            for o in data.gas_data.payment {
                if o.is_coin_reservation() {
                    out.coin_reservations.push(*o);
                }
            }
            true
        }
        _ => false,
    }
}

fn check(path: &Path, programmable: &mut usize, system: &mut usize) {
    let mut bytes = std::fs::read(path).unwrap();
    bytes.remove(0);
    let checkpoint = Message::<CheckpointData>::parse(bytes).unwrap();
    for tx in checkpoint.get().transactions {
        let data = &tx.transaction.data;
        let index = &data.index;
        let mut walked = Walked::default();
        if walk(data, &mut walked) {
            *programmable += 1;
            let indexed = Walked {
                shared: index.shared_inputs.to_vec(),
                owned: index.owned_inputs.to_vec(),
                packages: index.packages.to_vec(),
                receiving: index.receiving.to_vec(),
                move_calls: index.move_calls.to_vec(),
                funds_withdrawals: index.funds_withdrawals.len(),
                coin_reservations: index.coin_reservations.to_vec(),
            };
            assert_eq!(indexed, walked, "{}", path.display());
            assert_eq!(data.move_calls().count(), walked.move_calls.len());
        } else {
            *system += 1;
            assert!(index.owned_inputs.is_empty() && index.packages.is_empty());
            if let TransactionKind::ConsensusCommitPrologueV4(_) = data.kind {
                let [clock] = index.shared_inputs else {
                    panic!("a prologue writes the clock and nothing else")
                };
                assert_eq!(clock.id.0[31], 6);
                assert_eq!(clock.initial_shared_version.get(), 1);
            }
        }
    }
}

#[test]
fn index_matches_walk() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (mut programmable, mut system) = (0, 0);
    check(
        &manifest.join("tests/data/mainnet-325300367.chk"),
        &mut programmable,
        &mut system,
    );
    // Under Miri the checked-in checkpoint is enough, and all there is time for.
    if cfg!(miri) {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(manifest.join("../../corpus/mainnet")) {
        for entry in entries {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "chk") {
                check(&path, &mut programmable, &mut system);
            }
        }
    }
    eprintln!("{programmable} programmable, {system} other");
    assert!(programmable > 0 && system > 0);
}
