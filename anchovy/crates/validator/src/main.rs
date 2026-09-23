// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use validator::tls::NetworkKey;

/// Serves the validator gRPC API.
#[derive(Parser)]
struct Args {
    /// Address to accept connections on.
    #[arg(long)]
    listen: SocketAddr,
    /// The validator's network key file, in sui's format.
    #[arg(long)]
    network_key: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let key = NetworkKey::from_file(&args.network_key)
        .map_err(|e| format!("{}: {e}", args.network_key.display()))?;
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    eprintln!("listening on {}", listener.local_addr()?);
    let shutdown = async {
        // If the handler cannot be installed, run until killed.
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    validator::serve(listener, &key, shutdown).await?;
    Ok(())
}
