use super::*;

pub(super) struct DirectConnectionFactory;

impl ConnectionFactory for DirectConnectionFactory {
    fn connect(
        &self,
        scheme: &str,
        host: &str,
        address: SocketAddr,
        context: &ExecutionContext,
        deadline: Instant,
    ) -> Result<Box<dyn Connection>, TransportError> {
        let stream = connect_exact(address, context, deadline)?;
        if scheme == "https" {
            tls_connect(stream, host, context, deadline)
                .map(|stream| Box::new(stream) as Box<dyn Connection>)
        } else {
            Ok(Box::new(stream))
        }
    }
}

pub(super) fn connect_exact(
    address: SocketAddr,
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<TcpStream, TransportError> {
    let socket = Socket::new(Domain::for_address(address), Type::STREAM, Some(Protocol::TCP))
        .map_err(|_| TransportError::new(TransportErrorKind::Connect))?;
    socket.set_nonblocking(true).map_err(|_| TransportError::new(TransportErrorKind::Connect))?;
    match socket.connect(&SockAddr::from(address)) {
        Ok(()) => {}
        Err(error) if connect_pending(&error) => loop {
            check_operation(context, deadline)?;
            if let Some(_) =
                socket.take_error().map_err(|_| TransportError::new(TransportErrorKind::Connect))?
            {
                return Err(TransportError::new(TransportErrorKind::Connect));
            }
            if socket.peer_addr().is_ok() {
                break;
            }
            thread::sleep(blocking_slice(deadline));
        },
        Err(_) => return Err(TransportError::new(TransportErrorKind::Connect)),
    }
    check_operation(context, deadline)?;
    socket.set_nonblocking(false).map_err(|_| TransportError::new(TransportErrorKind::Connect))?;
    let stream = TcpStream::from(socket);
    stream.set_nodelay(true).map_err(|_| TransportError::new(TransportErrorKind::Connect))?;
    stream
        .set_read_timeout(Some(IO_POLL_SLICE))
        .map_err(|_| TransportError::new(TransportErrorKind::Connect))?;
    stream
        .set_write_timeout(Some(IO_POLL_SLICE))
        .map_err(|_| TransportError::new(TransportErrorKind::Connect))?;
    Ok(stream)
}

pub(super) fn connect_pending(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted)
        || matches!(error.raw_os_error(), Some(36 | 115 | 10035))
}

pub(super) fn write_all_checked(
    writer: &mut dyn Write,
    mut bytes: &[u8],
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<(), TransportError> {
    while !bytes.is_empty() {
        check_operation(context, deadline)?;
        match writer.write(bytes) {
            Ok(0) => return Err(TransportError::new(TransportErrorKind::Connect)),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if retryable_io(&error) => {}
            Err(_) => return Err(TransportError::new(TransportErrorKind::Connect)),
        }
    }
    Ok(())
}

pub(super) fn read_checked(
    reader: &mut dyn Read,
    bytes: &mut [u8],
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<usize, TransportError> {
    loop {
        check_operation(context, deadline)?;
        match reader.read(bytes) {
            Ok(read) => return Ok(read),
            Err(error) if retryable_io(&error) => {}
            Err(_) => return Err(TransportError::new(TransportErrorKind::Connect)),
        }
    }
}

pub(super) fn retryable_io(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

pub(super) fn blocking_slice(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now()).min(IO_POLL_SLICE)
}

pub(super) fn check_context(context: &ExecutionContext) -> Result<(), TransportError> {
    context.checkpoint().map_err(map_context_error)
}

pub(super) fn check_operation(
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<(), TransportError> {
    check_context(context)?;
    if Instant::now() >= deadline {
        Err(TransportError::new(TransportErrorKind::Timeout))
    } else {
        Ok(())
    }
}
