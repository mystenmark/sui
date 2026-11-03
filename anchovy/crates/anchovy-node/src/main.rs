// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use sui_config::{Config as _, NodeConfig};

/// Runs an anchovy validator from a sui-node config.
#[derive(Parser)]
struct Args {
    /// The sui-node config (`validator.yaml`).
    #[arg(long)]
    config_path: PathBuf,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    let config = NodeConfig::load(&args.config_path)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .thread_name("anchovy")
        .enable_all()
        .build()?;
    let running = anchovy_node::start(&config, &runtime)?;
    tracing::info!(address = %config.network_address, "anchovy is running");
    runtime.block_on(async {
        // The certified checkpoints this node's aggregator makes are a
        // probe for tests; serving peers reads the store. The highest is
        // logged for those that read the log.
        let mut drain = tokio::time::interval(Duration::from_secs(1));
        let mut node = running.node;
        loop {
            tokio::select! {
                _ = drain.tick() => {
                    let mut highest = None;
                    while let Some(certified) = running.certified.try_pop() {
                        highest = Some(certified.seq);
                    }
                    if let Some(seq) = highest {
                        tracing::info!("anchovy certified checkpoint {seq}");
                    }
                }
                result = &mut node => return result.map_err(anyhow::Error::from),
            }
        }
    })
}
