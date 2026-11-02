// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `SenderSignedData::validity_check`, preceded by the checks the
//! reference makes while deserializing a transaction, which our parser
//! defers.

use containers::Bump;
use messages::signature::{CompressedSignature, MultiSig, PublicKey};
use messages::transaction::{
    CallArg, SenderSignedData, TransactionExpiration, TransactionKind, TxState, WithdrawalTypeArg,
};
use messages::type_tag::TypeTag;

use crate::kind::is_valid_identifier;
use crate::signature::{self, ParsedSignature, uleb_len};
use crate::{Context, Error, ErrorKind, transaction_data};

/// A transaction that passed: its signatures parsed, and the size the
/// reference counts against per-block byte limits.
#[derive(Clone, Copy, Debug)]
pub struct Checked<'a> {
    pub signatures: &'a [ParsedSignature<'a>],
    pub tx_size: usize,
}

fn malformed(what: &str) -> Error {
    Error::new(ErrorKind::TransactionDeserializationError, what)
}

pub fn validity_check<'a>(
    tx: &SenderSignedData<'a, impl TxState>,
    ctx: &Context<'_>,
    bump: &'a Bump,
) -> Result<Checked<'a>, Error> {
    let (signatures, tx_size) = deserialization_checks(tx, bump)?;
    let config = ctx.config;

    for sig in signatures {
        // Only ML-DSA-65 is gated among the simple schemes. An ML-DSA-65
        // multisig member, signing or not, is a BCS variant older binaries
        // cannot decode.
        let enabled = match sig {
            ParsedSignature::MultiSig(m) => {
                config.upgraded_multisig_supported() && (config.mldsa65_auth() || !uses_mldsa65(m))
            }
            ParsedSignature::MultiSigLegacy { multisig, .. } => {
                config.mldsa65_auth() || !uses_mldsa65(multisig)
            }
            ParsedSignature::ZkLogin(_) => config.zklogin_auth(),
            ParsedSignature::Passkey(_) => config.passkey_auth(),
            ParsedSignature::Simple(bytes) => bytes[0] != 7 || config.mldsa65_auth(),
        };
        if !enabled {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!("{} signatures are not enabled", sig.variant()),
            ));
        }
    }

    // Users cannot send system transactions.
    if !matches!(
        tx.data().kind(),
        TransactionKind::ProgrammableTransaction(_)
    ) {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "a user transaction cannot be a system transaction",
        ));
    }

    let max = config.max_tx_size_bytes();
    if tx_size as u64 > max {
        return Err(Error::new(
            ErrorKind::SizeLimitExceeded,
            format!("transaction of {tx_size} bytes, limit {max}"),
        ));
    }
    if transaction_data::is_gasless(tx.data(), ctx) {
        let max = config.get_gasless_max_tx_size_bytes();
        if tx_size as u64 > max {
            return Err(Error::new(
                ErrorKind::SizeLimitExceeded,
                format!("gasless transaction of {tx_size} bytes, limit {max}"),
            ));
        }
    }

    transaction_data::validity_check(tx.data(), ctx, bump)?;
    Ok(Checked {
        signatures,
        tx_size,
    })
}

/// `MultiSig::uses_mldsa65`: an ML-DSA-65 key or signature.
fn uses_mldsa65(m: &MultiSig<'_>) -> bool {
    m.multisig_pk
        .pk_map
        .iter()
        .any(|(k, _)| matches!(k, PublicKey::MLDSA65(_)))
        || m.sigs
            .iter()
            .any(|s| matches!(s, CompressedSignature::MLDSA65(_)))
}

/// What the reference rejects while deserializing a user transaction that
/// our parser accepted: the intent, an empty proposer set, identifiers in
/// withdrawal types, and signature contents. Returns the parsed signatures
/// and the size the reference's re-serialization has.
///
/// System kinds carry more deserialization rules (genesis objects,
/// durations) that are not checked here; such a transaction is rejected as
/// a system transaction instead, with a different error.
pub fn deserialization_checks<'a>(
    tx: &SenderSignedData<'a, impl TxState>,
    bump: &'a Bump,
) -> Result<(&'a [ParsedSignature<'a>], usize), Error> {
    let intent = tx.intent();
    if (intent.scope, intent.version, intent.app_id) != (0, 0, 0) {
        return Err(malformed("not a transaction intent"));
    }

    if let TransactionExpiration::Validity(_, Some(allowed)) = tx.data().expiration()
        && allowed.proposers.is_empty()
    {
        return Err(malformed("empty allowed proposers"));
    }

    if let TransactionKind::ProgrammableTransaction(pt) = &tx.data().kind() {
        for input in pt.inputs {
            if let CallArg::FundsWithdrawal(w) = input {
                let WithdrawalTypeArg::Balance(ty) = &w.get().type_arg;
                if !type_tag_identifiers_valid(ty) {
                    return Err(malformed("invalid identifier in a type"));
                }
            }
        }
    }

    // Signatures re-serialize to their wire bytes, except legacy multisig.
    let mut size = tx.bytes().len();
    let mut parsed = containers::Vec::with_capacity_in(tx.tx_signatures().len(), bump);
    for sig in tx.tx_signatures() {
        let (sig_parsed, len) = signature::parse(sig.0, bump)?;
        size = size - uleb_len(sig.0.len()) - sig.0.len() + uleb_len(len) + len;
        parsed.push(sig_parsed);
    }
    Ok((parsed.leak(), size))
}

/// `TypeTag` identifiers are checked as they are deserialized.
fn type_tag_identifiers_valid(ty: &TypeTag<'_>) -> bool {
    match ty {
        TypeTag::Vector(inner) => type_tag_identifiers_valid(inner.get()),
        TypeTag::Struct(s) => {
            let s = s.get();
            is_valid_identifier(s.module)
                && is_valid_identifier(s.name)
                && s.type_params.iter().all(type_tag_identifiers_valid)
        }
        _ => true,
    }
}
