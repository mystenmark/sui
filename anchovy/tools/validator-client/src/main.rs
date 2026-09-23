// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Runs sui's own validator client against `anchovy-validator`. Usage:
//! `validator-client PATH/TO/anchovy-validator`.
//!
//! Generates a network key with sui-types, writes it in sui's key file
//! format, starts the server with it, and connects as
//! `NetworkAuthorityClient::connect` does (`sui-tls` pinning the key,
//! `mysten-network`'s channel). Then: health must answer; every other
//! route must reach its handler, which the server's `todo!()` message on
//! stderr shows; a client pinning another key must be refused.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use mysten_network::Multiaddr;
use rand::SeedableRng;
use rand::rngs::StdRng;
use sui_network::api::ValidatorClient;
use sui_types::base_types::{ObjectID, SequenceNumber};
use sui_types::crypto::{
    EncodeDecodeBase64, KeypairTraits, NetworkKeyPair, NetworkPublicKey, SuiKeyPair,
    get_key_pair_from_rng,
};
use sui_types::digests::TransactionDigest;
use sui_types::messages_checkpoint::{CheckpointRequest, CheckpointRequestV2};
use sui_types::messages_grpc::{
    LayoutGenerationOption, ObjectInfoRequest, ObjectInfoRequestKind, PingType, RawSubmitTxRequest,
    RawValidatorHealthRequest, RawWaitForEffectsRequest, SubmitTxType, SystemStateRequest,
    TransactionInfoRequest,
};
use tonic::Code;
use tonic::transport::Channel;

/// The server, killed on drop, and the stderr lines it has written.
struct Server {
    child: Child,
    stderr: Arc<Mutex<Vec<String>>>,
    address: Multiaddr,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn start(binary: &str, key: &NetworkKeyPair) -> Result<Server> {
        let dir = std::env::temp_dir().join(format!("validator-client-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let key_path = dir.join("network.key");
        std::fs::write(&key_path, SuiKeyPair::Ed25519(key.copy()).encode_base64())?;

        let mut child = Command::new(binary)
            .args(["--listen", "127.0.0.1:0", "--network-key"])
            .arg(&key_path)
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting {binary}"))?;
        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        let first = lines.next().context("server exited")??;
        let addr = first
            .strip_prefix("listening on ")
            .with_context(|| format!("unexpected first line: {first}"))?;
        let (ip, port) = addr.rsplit_once(':').context("no port")?;
        // `mysten-network::client::connect` builds an https-only connector,
        // which refuses the `http://` URI an `/http` multiaddr becomes.
        let address: Multiaddr = format!("/ip4/{ip}/tcp/{port}/https").parse()?;

        let stderr = Arc::new(Mutex::new(Vec::new()));
        let sink = stderr.clone();
        std::thread::spawn(move || {
            for line in lines.map_while(Result::ok) {
                sink.lock().unwrap().push(line);
            }
        });
        Ok(Server {
            child,
            stderr,
            address,
        })
    }

    /// Whether the server has reported reaching `handler`'s `todo!()`.
    async fn reached(&self, handler: &str) -> bool {
        let needle = format!("not yet implemented: {handler}");
        for _ in 0..50 {
            if self
                .stderr
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains(&needle))
            {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        false
    }
}

async fn connect(address: &Multiaddr, key: NetworkPublicKey) -> Result<ValidatorClient<Channel>> {
    let tls = sui_tls::create_rustls_client_config(
        key,
        sui_tls::SUI_VALIDATOR_SERVER_NAME.to_string(),
        None,
    );
    let channel = mysten_network::client::connect(address, tls)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(ValidatorClient::new(channel))
}

/// A route that decoded its request and reached a `todo!()` handler.
async fn expect_todo<T>(
    server: &Server,
    handler: &str,
    result: Result<tonic::Response<T>, tonic::Status>,
) -> Result<()> {
    match result {
        Ok(_) => bail!("{handler}: answered, expected todo!()"),
        Err(status) if matches!(status.code(), Code::Unimplemented | Code::InvalidArgument) => {
            bail!("{handler}: {status:?}")
        }
        Err(_) => {}
    }
    ensure!(
        server.reached(handler).await,
        "{handler}: no todo!() on the server"
    );
    println!("ok  {handler} reached its handler");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let binary = std::env::args()
        .nth(1)
        .context("usage: validator-client PATH/TO/anchovy-validator")?;
    let (_, key): (_, NetworkKeyPair) = get_key_pair_from_rng(&mut StdRng::from_seed([7; 32]));
    let server = Server::start(&binary, &key)?;
    let mut client = connect(&server.address, key.public().clone()).await?;

    let health = client
        .validator_health(RawValidatorHealthRequest {})
        .await?
        .into_inner();
    ensure!(
        health.pending_certificates.is_none()
            && health.inflight_consensus_messages.is_none()
            && health.consensus_round.is_none()
            && health.checkpoint_sequence.is_none(),
        "health: {health:?}"
    );
    println!("ok  validator_health answered");

    let ping = RawSubmitTxRequest {
        transactions: vec![],
        submit_type: SubmitTxType::Ping as i32,
    };
    let result = client.submit_transaction(ping).await;
    expect_todo(&server, "submit_transaction", result).await?;

    let wait = RawWaitForEffectsRequest {
        transaction_digest: None,
        consensus_position: Some(Bytes::from_static(&[0; 16])),
        include_details: false,
        ping_type: Some(PingType::Consensus as i32),
    };
    let result = client.wait_for_effects(wait).await;
    expect_todo(&server, "wait_for_effects", result).await?;

    let object = ObjectInfoRequest {
        object_id: ObjectID::new([1; 32]),
        generate_layout: LayoutGenerationOption::Generate,
        request_kind: ObjectInfoRequestKind::PastObjectInfoDebug(SequenceNumber::from_u64(9)),
    };
    let result = client.object_info(object).await;
    expect_todo(&server, "object_info", result).await?;

    let transaction = TransactionInfoRequest {
        transaction_digest: TransactionDigest::new([2; 32]),
    };
    let result = client.transaction_info(transaction).await;
    expect_todo(&server, "transaction_info", result).await?;

    let checkpoint = CheckpointRequest {
        sequence_number: Some(3),
        request_content: true,
    };
    let result = client.checkpoint(checkpoint).await;
    expect_todo(&server, "checkpoint", result).await?;

    let checkpoint = CheckpointRequestV2 {
        sequence_number: None,
        request_content: false,
        certified: true,
    };
    let result = client.checkpoint_v2(checkpoint).await;
    expect_todo(&server, "checkpoint_v2", result).await?;

    let result = client
        .get_system_state_object(SystemStateRequest { _unused: false })
        .await;
    expect_todo(&server, "get_system_state_object", result).await?;

    // The panics took down their streams only.
    client
        .validator_health(RawValidatorHealthRequest {})
        .await
        .context("health after the todo!() routes")?;
    println!("ok  server still serving");

    let (_, other): (_, NetworkKeyPair) = get_key_pair_from_rng(&mut StdRng::from_seed([8; 32]));
    let refused = match connect(&server.address, other.public().clone()).await {
        Err(_) => true,
        Ok(mut c) => c
            .validator_health(RawValidatorHealthRequest {})
            .await
            .is_err(),
    };
    ensure!(refused, "a client pinning another key was served");
    println!("ok  client pinning another key refused");
    Ok(())
}
