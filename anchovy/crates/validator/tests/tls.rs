// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The server over TLS, reached by a client configured as sui's is: TLS
//! 1.3, server name `sui`, the network key pinned.

use std::net::SocketAddr;
use std::sync::Arc;

use hyper_util::rt::TokioIo;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use tonic::transport::{Channel, Endpoint, Uri};
use validator::proto::{RawValidatorHealthRequest, RawValidatorHealthResponse};
use validator::service::validator_client::ValidatorClient;
use validator::tls::{NetworkKey, SERVER_NAME};

/// Accepts exactly the certificate whose key is `public_key`, as sui's
/// `ServerCertVerifier` does. The Ed25519 `SubjectPublicKeyInfo` is a fixed
/// prefix and the key, so finding it in the DER finds the certificate's
/// key.
#[derive(Debug)]
struct Pinned {
    public_key: [u8; 32],
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let mut spki = vec![
            0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
        ];
        spki.extend_from_slice(&self.public_key);
        if end_entity.windows(spki.len()).any(|w| w == spki) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("not the pinned key".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn client_config(public_key: [u8; 32]) -> ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned {
            public_key,
            provider,
        }))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    config
}

async fn serve(key: NetworkKey) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        validator::serve(listener, &key, std::future::pending())
            .await
            .unwrap();
    });
    addr
}

async fn connect(
    addr: SocketAddr,
    public_key: [u8; 32],
) -> Result<Channel, tonic::transport::Error> {
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config(public_key)));
    Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let connector = connector.clone();
            async move {
                let tcp = tokio::net::TcpStream::connect(addr).await?;
                let name = ServerName::try_from(SERVER_NAME).unwrap();
                let tls = connector.connect(name, tcp).await?;
                Ok::<_, std::io::Error>(TokioIo::new(tls))
            }
        }))
        .await
}

#[tokio::test]
async fn pinned_client_is_served() {
    let key = NetworkKey::from_bytes([7; 32]);
    let public_key = key.public_key();
    let addr = serve(key).await;
    let mut client = ValidatorClient::new(connect(addr, public_key).await.unwrap());
    let health = client
        .validator_health(RawValidatorHealthRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(health, RawValidatorHealthResponse::default());
}

#[tokio::test]
async fn wrong_key_is_refused() {
    let addr = serve(NetworkKey::from_bytes([7; 32])).await;
    let other = NetworkKey::from_bytes([8; 32]).public_key();
    assert!(connect(addr, other).await.is_err());
}

#[test]
fn reads_sui_key_files() {
    let dir = std::env::temp_dir().join(format!("anchovy-key-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("network.key");
    let write = |bytes: &[u8]| {
        use base64::Engine as _;
        let text = base64::engine::general_purpose::STANDARD.encode(bytes);
        std::fs::write(&path, format!("{text}\n")).unwrap();
    };

    let mut file = vec![0u8];
    file.extend_from_slice(&[7; 32]);
    write(&file);
    assert_eq!(
        NetworkKey::from_file(&path).unwrap().public_key(),
        NetworkKey::from_bytes([7; 32]).public_key()
    );

    // Secp256k1's flag.
    file[0] = 1;
    write(&file);
    assert!(NetworkKey::from_file(&path).is_err());
    write(&[0; 32]);
    assert!(NetworkKey::from_file(&path).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}
