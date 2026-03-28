// Called by the connection loop (implemented in the next task).
#![allow(dead_code)]

//! TCP and TLS connection factory for the IRC server link.
//!
//! Returns `LineReader`/`LineWriter` halves that abstract over both plain-TCP
//! and TLS transports. IRC servers commonly use self-signed certificates, so
//! the TLS path uses an accept-all certificate verifier — the link password
//! provides the real authentication.

use std::io;
use std::sync::Arc;

use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    ClientConfig, DigitallySignedStruct, Error as TlsError, SignatureScheme,
};

use super::framing::{LineReader, LineWriter};

/// Erased read half: works for both plain TCP and TLS.
pub type IrcReader = LineReader<Box<dyn tokio::io::AsyncRead + Unpin + Send>>;
/// Erased write half: works for both plain TCP and TLS.
pub type IrcWriter = LineWriter<Box<dyn tokio::io::AsyncWrite + Unpin + Send>>;

/// Open a TCP (or TLS) connection to an IRC server and return framed halves.
///
/// - If `tls` is `false`, connects with a plain TCP stream.
/// - If `tls` is `true`, wraps the TCP stream with TLS using an accept-all
///   certificate verifier (see [`AcceptAnyCert`]).
///
/// Returns `Err` if the TCP connection is refused or if the TLS handshake
/// fails.
pub async fn connect(host: &str, port: u16, tls: bool) -> io::Result<(IrcReader, IrcWriter)> {
    let tcp = TcpStream::connect((host, port)).await?;

    if tls {
        let config = Arc::new(make_accept_any_tls_config());
        let connector = TlsConnector::from(config);
        let server_name = ServerName::try_from(host.to_owned())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let tls_stream = connector.connect(server_name, tcp).await?;
        let (r, w) = tokio::io::split(tls_stream);
        Ok((
            LineReader::new(Box::new(r) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
            LineWriter::new(Box::new(w) as Box<dyn tokio::io::AsyncWrite + Unpin + Send>),
        ))
    } else {
        let (r, w) = tcp.into_split();
        Ok((
            LineReader::new(Box::new(r) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
            LineWriter::new(Box::new(w) as Box<dyn tokio::io::AsyncWrite + Unpin + Send>),
        ))
    }
}

fn make_accept_any_tls_config() -> ClientConfig {
    ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth()
}

/// A TLS server-certificate verifier that accepts any certificate.
///
/// IRC servers (including UnrealIRCd) commonly use self-signed certificates
/// for server links. The link password is the real authentication mechanism;
/// cert verification would add operator burden without a security benefit
/// in this context.
#[derive(Debug)]
struct AcceptAnyCert;

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA1,
            SignatureScheme::ECDSA_SHA1_Legacy,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::ED448,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The connect() function requires a live network endpoint, so real
    // connection tests are #[ignore]. The certificate verifier is a pass-through
    // with no branch logic to test. The only meaningful unit test here checks
    // that a refused connection surfaces as an error.

    /// AcceptAnyCert must declare at least one signature scheme so that TLS
    /// negotiation can succeed (an empty list prevents any cipher suite agreement).
    #[test]
    fn accept_any_cert_supports_at_least_one_scheme() {
        let v = AcceptAnyCert;
        assert!(
            !v.supported_verify_schemes().is_empty(),
            "supported_verify_schemes must not be empty"
        );
    }

    /// Bind a listener on an ephemeral port, drop it, then attempt to connect —
    /// the connection must be refused and surfaced as Err.
    #[tokio::test]
    async fn refused_connection_returns_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener); // nothing listening here now

        let result = connect("127.0.0.1", port, false).await;
        assert!(result.is_err(), "expected connection error, got Ok");
    }

    #[tokio::test]
    #[ignore = "requires a live plain-TCP IRC server"]
    async fn connects_plain() {
        let (_r, _w) = connect("irc.example.org", 6667, false).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires a live TLS IRC server"]
    async fn connects_tls() {
        let (_r, _w) = connect("irc.example.org", 6697, true).await.unwrap();
    }
}
