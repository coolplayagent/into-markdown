use super::{
    Arc, ExecutionContext, Instant, OnceLock, ServerName, TransportError, TransportErrorKind,
    check_operation,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, UnixTime};
use std::io;
use std::io::{Read, Write};

pub(super) fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots =
                webpki_roots::TLS_SERVER_ROOTS.iter().cloned().collect::<rustls::RootCertStore>();
            // Also trust certificates from the operating-system root store so
            // that corporate TLS-inspection proxies (whose MITM CA is installed
            // in the OS store but not in the Mozilla roots) are accepted.
            let native = rustls_native_certs::load_native_certs();
            for cert in native.certs {
                roots.add(cert).ok();
            }
            Arc::new(
                rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth(),
            )
        })
        .clone()
}

pub(super) fn tls_config_insecure() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            Arc::new(
                rustls::ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(NoCertificateVerification))
                    .with_no_client_auth(),
            )
        })
        .clone()
}

/// A `ServerCertVerifier` that accepts any certificate, disabling all TLS
/// certificate verification. Only used when the caller explicitly opts in
/// via `INTO_MD_INSECURE`.
#[derive(Debug)]
struct NoCertificateVerification;

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
        ]
    }
}

pub(super) fn tls_handshake<S: Read + Write>(
    mut stream: S,
    host: &str,
    context: &ExecutionContext,
    deadline: Instant,
    insecure: bool,
) -> Result<rustls::StreamOwned<rustls::ClientConnection, S>, TransportError> {
    let config = if insecure { tls_config_insecure() } else { tls_config() };
    let server_name = ServerName::try_from(host.to_owned())
        .map_err(|_| TransportError::new(TransportErrorKind::InvalidMessage))?;
    let mut connection = rustls::ClientConnection::new(config, server_name)
        .map_err(|_| TransportError::new(TransportErrorKind::Tls))?;
    while connection.is_handshaking() {
        check_operation(context, deadline)?;
        match connection.complete_io(&mut stream) {
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err(TransportError::new(TransportErrorKind::Tls)),
        }
    }
    Ok(rustls::StreamOwned::new(connection, stream))
}
