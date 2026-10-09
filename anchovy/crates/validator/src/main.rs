// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, ValueEnum};
use messages::base::Digest;
use protocol_config::ProtocolVersion;
use validator::epoch::EpochState;
use validator::tls::NetworkKey;

#[derive(Clone, Copy, ValueEnum)]
enum Chain {
    Mainnet,
    Testnet,
    Unknown,
}

/// Serves the validator gRPC API.
#[derive(Parser)]
struct Args {
    /// Address to accept connections on.
    #[arg(long)]
    listen: SocketAddr,
    /// The validator's network key file, in sui's format.
    #[arg(long)]
    network_key: PathBuf,
    /// Until reconfiguration exists, the epoch is set here.
    #[arg(long, value_enum, default_value = "unknown")]
    chain: Chain,
    /// The protocol version in force; the latest by default.
    #[arg(long)]
    protocol_version: Option<u64>,
    #[arg(long, default_value_t = 0)]
    epoch: u64,
    /// The chain identifier (genesis checkpoint digest), in hex.
    #[arg(long, default_value = "00")]
    chain_id: String,
    #[arg(long, default_value_t = 1000)]
    reference_gas_price: u64,
    #[arg(long, default_value_t = 1)]
    committee_size: u32,
    /// The object database's directory, created if absent.
    #[arg(long)]
    db: PathBuf,
    /// A gas coin at genesis, as `0xADDRESS=MIST`; repeatable. Read only
    /// when the database has no genesis yet.
    #[arg(long = "genesis-coin", value_parser = allocation)]
    genesis_coins: Vec<([u8; 32], u64)>,
}

/// `0xADDRESS=MIST`.
fn allocation(s: &str) -> Result<([u8; 32], u64), String> {
    let (address, amount) = s
        .split_once('=')
        .ok_or("expected 0xADDRESS=MIST".to_owned())?;
    let amount = amount.parse().map_err(|e| format!("amount: {e}"))?;
    Ok((hex32(address)?.bytes, amount))
}

/// Up to 32 bytes of hex, left-padded.
fn hex32(hex: &str) -> Result<Digest, String> {
    let hex = hex.strip_prefix("0x").unwrap_or(hex);
    if hex.len() > 64 {
        return Err("over 32 bytes".to_owned());
    }
    let padded = format!("{hex:0>64}");
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&padded[2 * i..2 * i + 2], 16).map_err(|e| e.to_string())?;
    }
    Ok(Digest::new(bytes))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let key = NetworkKey::from_file(&args.network_key)
        .map_err(|e| format!("{}: {e}", args.network_key.display()))?;
    let chain = match args.chain {
        Chain::Mainnet => protocol_config::Chain::Mainnet,
        Chain::Testnet => protocol_config::Chain::Testnet,
        Chain::Unknown => protocol_config::Chain::Unknown,
    };
    let epoch = Arc::new(EpochState::new(
        chain,
        args.protocol_version
            .unwrap_or(ProtocolVersion::MAX.as_u64()),
        args.epoch,
        hex32(&args.chain_id).map_err(|e| format!("chain id: {e}"))?,
        args.reference_gas_price,
        args.committee_size,
        // JWKs are chain state, which does not exist yet: zkLogin fails.
        [],
    ));

    let store = Arc::new(
        store::Store::open(&args.db).map_err(|e| format!("{}: {e:?}", args.db.display()))?,
    );
    if execution::genesis::init(&epoch.execution, &store, &args.genesis_coins)
        .map_err(|e| format!("genesis: {e:?}"))?
    {
        eprintln!("wrote genesis to {}", args.db.display());
    }

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    eprintln!("listening on {}", listener.local_addr()?);
    let shutdown = async {
        // If the handler cannot be installed, run until killed.
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    validator::serve(listener, &key, epoch, store, shutdown).await?;
    Ok(())
}
