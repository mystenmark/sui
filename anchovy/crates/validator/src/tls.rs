// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! TLS as `sui-tls` speaks it: TLS 1.3 only, ALPN `h2`, no client auth, and
//! a self-signed certificate over the validator's Ed25519 network key
//! naming `sui`. Clients pin the network key rather than trusting a CA.

use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use rcgen::{CertificateParams, KeyPair, PKCS_ED25519};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tonic::transport::server::Connected;

/// The name every validator's certificate carries; clients check it.
pub const SERVER_NAME: &str = "sui";

/// A handshake that has not finished by then is dropped, so that idle
/// connections cannot hold accept slots.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// An Ed25519 secret key, as a sui network key file holds it.
pub struct NetworkKey([u8; 32]);

impl NetworkKey {
    pub fn from_bytes(secret: [u8; 32]) -> NetworkKey {
        NetworkKey(secret)
    }

    /// Reads a key file in sui's format: base64 of the scheme flag, 0 for
    /// Ed25519, then the 32-byte secret.
    pub fn from_file(path: &Path) -> io::Result<NetworkKey> {
        let invalid = |msg: &str| io::Error::new(io::ErrorKind::InvalidData, msg.to_owned());
        let text = std::fs::read_to_string(path)?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(text.trim())
            .map_err(|_| invalid("network key: not base64"))?;
        match bytes.split_first() {
            Some((0, secret)) => Ok(NetworkKey(
                secret
                    .try_into()
                    .map_err(|_| invalid("network key: not 32 bytes"))?,
            )),
            _ => Err(invalid("network key: not Ed25519")),
        }
    }

    /// PKCS#8 v1, the only form ring accepts: a fixed prefix, then the
    /// secret (RFC 8410).
    fn pkcs8(&self) -> PrivatePkcs8KeyDer<'static> {
        const PREFIX: [u8; 16] = [
            0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ];
        let mut der = Vec::with_capacity(48);
        der.extend_from_slice(&PREFIX);
        der.extend_from_slice(&self.0);
        PrivatePkcs8KeyDer::from(der)
    }

    fn key_pair(&self) -> KeyPair {
        KeyPair::from_pkcs8_der_and_sign_algo(&self.pkcs8(), &PKCS_ED25519)
            .expect("any 32 bytes are an Ed25519 secret")
    }

    /// The public key clients pin.
    pub fn public_key(&self) -> [u8; 32] {
        self.key_pair()
            .public_key_raw()
            .try_into()
            .expect("an Ed25519 public key is 32 bytes")
    }

    /// The self-signed certificate naming [`SERVER_NAME`].
    pub fn certificate(&self) -> CertificateDer<'static> {
        CertificateParams::new(vec![SERVER_NAME.to_owned()])
            .expect("a DNS name")
            .self_signed(&self.key_pair())
            .expect("Ed25519 signs any certificate")
            .into()
    }
}

pub fn server_config(key: &NetworkKey) -> ServerConfig {
    let mut config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("ring supports TLS 1.3")
            .with_no_client_auth()
            .with_single_cert(vec![key.certificate()], PrivateKeyDer::Pkcs8(key.pkcs8()))
            .expect("the certificate is over this key");
    config.alpn_protocols = vec![b"h2".to_vec()];
    config
}

/// A TLS connection handed to tonic. The newtype exists to implement
/// `Connected`, which tonic requires of every transport.
pub struct TlsConnection(TlsStream<TcpStream>);

#[derive(Clone, Debug)]
pub struct TlsConnectInfo {
    pub remote_addr: Option<SocketAddr>,
}

impl Connected for TlsConnection {
    type ConnectInfo = TlsConnectInfo;

    fn connect_info(&self) -> TlsConnectInfo {
        TlsConnectInfo {
            remote_addr: self.0.get_ref().0.peer_addr().ok(),
        }
    }
}

impl AsyncRead for TlsConnection {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for TlsConnection {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// Accepts TCP connections and completes their TLS handshakes, each in its
/// own task so a slow client does not hold up the rest. Failed handshakes
/// are dropped; tonic sees only established connections.
pub fn incoming(
    listener: TcpListener,
    config: ServerConfig,
) -> tokio_stream::wrappers::ReceiverStream<io::Result<TlsConnection>> {
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                // The server has shut down.
                () = tx.closed() => return,
                accepted = listener.accept() => accepted,
            };
            let Ok((tcp, _)) = accepted else {
                // A reset before accept, or out of file descriptors; the
                // pause keeps the latter from spinning.
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            let acceptor = acceptor.clone();
            let sender = tx.clone();
            tokio::spawn(async move {
                if let Ok(Ok(tls)) =
                    tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await
                {
                    // Fails only if the server has shut down meanwhile.
                    let _ = sender.send(Ok(TlsConnection(tls))).await;
                }
            });
        }
    });
    tokio_stream::wrappers::ReceiverStream::new(rx)
}
