use super::{
    Arc, ExecutionContext, Instant, OnceLock, ServerName, TransportError, TransportErrorKind,
    check_operation,
};
use std::io;
use std::io::{Read, Write};

pub(super) fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots =
                webpki_roots::TLS_SERVER_ROOTS.iter().cloned().collect::<rustls::RootCertStore>();
            Arc::new(
                rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth(),
            )
        })
        .clone()
}

pub(super) fn tls_handshake<S: Read + Write>(
    mut stream: S,
    host: &str,
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<rustls::StreamOwned<rustls::ClientConnection, S>, TransportError> {
    let server_name = ServerName::try_from(host.to_owned())
        .map_err(|_| TransportError::new(TransportErrorKind::InvalidMessage))?;
    let mut connection = rustls::ClientConnection::new(tls_config(), server_name)
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
