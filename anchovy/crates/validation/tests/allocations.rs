// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Heap allocations made while validating, counted with a global
//! allocator. The arena is created before counting starts, as a caller
//! would reuse one.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::Path;

use containers::Bump;
use messages::Message;
use messages::base::Digest;
use messages::checkpoint::CheckpointData;
use protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use validation::Context;
use validation::verify::Verifier;

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.with(|c| c.set(c.get() + 1));
        // SAFETY: the caller's contract, passed on.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

fn count<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let before = ALLOCATIONS.with(Cell::get);
    let out = f();
    (out, ALLOCATIONS.with(Cell::get) - before)
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn allocations_per_transaction() {
    let data = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut chks = vec![data.join("../messages/tests/data/mainnet-325300367.chk")];
    if let Ok(dir) = std::fs::read_dir(data.join("../../corpus/mainnet")) {
        chks.extend(
            dir.map(|e| e.unwrap().path())
                .filter(|p| p.extension().is_some_and(|e| e == "chk")),
        );
    }
    let config = ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Mainnet);
    let verifier = Verifier::new(&config, Chain::Mainnet, []);
    let chain = Digest::new(
        unhex("35834a8ac17ca48fb14ac8f99c17c98747e95dd07294ae41a46b382246a4499b")
            .try_into()
            .unwrap(),
    );

    // (step, outcome, signature scheme) -> allocation counts
    let mut seen: Seen = BTreeMap::new();
    for chk in &chks {
        let mut bytes = std::fs::read(chk).unwrap();
        bytes.remove(0);
        let checkpoint = Message::<CheckpointData>::parse(bytes)
            .map_err(|(e, _)| e)
            .unwrap();
        let checkpoint = checkpoint.get();
        let ctx = Context {
            config: &config,
            epoch: checkpoint.checkpoint_summary.data.epoch,
            chain_identifier: chain,
            reference_gas_price: 1,
            committee_size: 100,
        };
        for tx in checkpoint.transactions {
            let signed = &tx.transaction;
            let scheme = match signed.tx_signatures.first().map(|s| s.0[0]) {
                Some(0) => "ed25519",
                Some(1) => "secp256k1",
                Some(2) => "secp256r1",
                Some(3) => "multisig",
                Some(5) => "zklogin",
                Some(6) => "passkey",
                _ => "none",
            }
            .to_owned();
            // Once uncounted, so one-time initialization (fastcrypto's
            // secp256k1 context) is not charged to a transaction.
            let warm = Bump::with_capacity(1 << 16);
            let _ = validation::check(signed, &ctx, &verifier, &[], &warm);
            let bump = Bump::with_capacity(1 << 16);
            let (result, n) = count(|| {
                validation::sender_signed::validity_check(signed, &ctx, &bump).map(|_| ())
            });
            let outcome = result.map_or_else(|e| format!("{:?}", e.kind), |()| "ok".into());
            if n > 0 && outcome == "ok" && scheme != "zklogin" {
                eprintln!(
                    "{n} allocations: arena chunks {} bytes {}, price {}",
                    bump.chunks(),
                    bump.allocated(),
                    signed.data.gas_data.price
                );
            }
            seen.entry(("validity_check", outcome, scheme.clone()))
                .or_default()
                .push(n);

            let bump = Bump::with_capacity(1 << 16);
            let (result, n) =
                count(|| validation::check(signed, &ctx, &verifier, &[], &bump).map(|_| ()));
            let outcome = result.map_or_else(|e| format!("{:?}", e.kind), |()| "ok".into());
            seen.entry(("check", outcome, scheme)).or_default().push(n);
        }
    }
    measure_vectors(&mut seen);

    // Accepting a transaction allocates nothing on the heap, except where a
    // dependency does: zkLogin (fastcrypto-zkp's types and proof check),
    // Secp256r1 (fastcrypto's verification builds tables on the heap),
    // passkey (both of those and serde_json) and legacy multisig (roaring).
    for ((step, outcome, scheme), counts) in &seen {
        let heap_by_dependency =
            ["zklogin", "passkey", "multisig", "r1", "secp256r1"].contains(&scheme.as_str());
        if outcome == "ok" && !heap_by_dependency {
            let most = counts.iter().max().unwrap();
            assert_eq!(
                *most, 0,
                "{step} {scheme}: accepting allocated up to {most}"
            );
        }
    }
    for ((step, outcome, scheme), counts) in &seen {
        let min = counts.iter().min().unwrap();
        let max = counts.iter().max().unwrap();
        let mean = counts.iter().sum::<usize>() as f64 / counts.len() as f64;
        eprintln!(
            "{step:15} {outcome:28} {scheme:10} n={:5} allocations min {min} mean {mean:.1} max {max}",
            counts.len()
        );
    }
}

type Seen = BTreeMap<(&'static str, String, String), Vec<usize>>;

/// Every scheme, from the verification vectors (Unknown chain, latest
/// version, epoch 4, the vectors' JWKs).
fn measure_vectors(seen: &mut Seen) {
    let vectors = include_str!("data/validity.vectors");
    let unknown = ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Unknown);
    let jwks: Vec<(
        fastcrypto_zkp::bn254::zk_login::JwkId,
        fastcrypto_zkp::bn254::zk_login::JWK,
    )> = serde_json::from_str(
        vectors
            .lines()
            .next()
            .unwrap()
            .strip_prefix("jwks ")
            .unwrap(),
    )
    .unwrap();
    let verifier = Verifier::new(&unknown, Chain::Unknown, jwks);
    let ctx = Context {
        config: &unknown,
        epoch: 4,
        chain_identifier: Digest::new([0x11; 32]),
        reference_gas_price: 1000,
        committee_size: 4,
    };
    for line in vectors.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let ["tx", _, "verify", label, hex] = fields[..] else {
            continue;
        };
        let Ok(message) = Message::<messages::transaction::SenderSignedData>::parse(unhex(hex))
        else {
            continue;
        };
        let warm = Bump::with_capacity(1 << 16);
        let _ = validation::check(message.get(), &ctx, &verifier, &[], &warm);
        let bump = Bump::with_capacity(1 << 16);
        let (result, n) =
            count(|| validation::check(message.get(), &ctx, &verifier, &[], &bump).map(|_| ()));
        let outcome = result.map_or_else(|e| format!("{:?}", e.kind), |()| "ok".into());
        let scheme = label
            .trim_start_matches("verify_")
            .split('_')
            .next()
            .unwrap()
            .to_owned();
        seen.entry(("check (vectors)", outcome, scheme))
            .or_default()
            .push(n);
    }
}
