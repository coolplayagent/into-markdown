use super::{
    Arc, AtomicUsize, DNS_QUEUE_PER_WORKER, DNS_WORKERS, DnsResolver, ExecutionContext, Instant,
    MAX_DNS_ADDRESSES, OnceLock, Ordering, SocketAddr, SyncSender, ToSocketAddrs, TransportError,
    TransportErrorKind, TrySendError, blocking_slice, check_operation, mpsc,
};
use std::io;
use std::thread;

/// Bounded system resolver backed by the platform `getaddrinfo` workflow.
pub struct SystemDnsResolver;

impl DnsResolver for SystemDnsResolver {
    fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        let mut output = Vec::with_capacity(MAX_DNS_ADDRESSES);
        for address in (host, port).to_socket_addrs()? {
            if output.len() == MAX_DNS_ADDRESSES {
                return Err(io::Error::other("DNS address limit exceeded"));
            }
            output.push(address);
        }
        Ok(output)
    }
}

pub(super) struct DnsJob {
    resolver: Arc<dyn DnsResolver>,
    host: String,
    port: u16,
    result: SyncSender<io::Result<Vec<SocketAddr>>>,
}

pub(super) struct DnsPool {
    workers: Vec<SyncSender<DnsJob>>,
    next: AtomicUsize,
}

impl DnsPool {
    fn start() -> Option<Self> {
        let mut workers = Vec::with_capacity(DNS_WORKERS);
        for index in 0..DNS_WORKERS {
            let (sender, receiver) = mpsc::sync_channel::<DnsJob>(DNS_QUEUE_PER_WORKER);
            if thread::Builder::new()
                .name(format!("into-md-http-dns-{index}"))
                .spawn(move || {
                    while let Ok(job) = receiver.recv() {
                        let result = job.resolver.resolve(&job.host, job.port);
                        let _ = job.result.send(result);
                    }
                })
                .is_ok()
            {
                workers.push(sender);
            }
        }
        (!workers.is_empty()).then(|| Self { workers, next: AtomicUsize::new(0) })
    }

    fn submit(&self, mut job: DnsJob) -> Result<(), TransportError> {
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        for offset in 0..self.workers.len() {
            let index = (start + offset) % self.workers.len();
            match self.workers[index].try_send(job) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(returned) | TrySendError::Disconnected(returned)) => {
                    job = returned;
                }
            }
        }
        Err(TransportError::new(TransportErrorKind::Dns))
    }
}

pub(super) fn resolve_checked(
    resolver: Arc<dyn DnsResolver>,
    host: String,
    port: u16,
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<Vec<SocketAddr>, TransportError> {
    static POOL: OnceLock<Option<DnsPool>> = OnceLock::new();
    let pool = POOL
        .get_or_init(DnsPool::start)
        .as_ref()
        .ok_or_else(|| TransportError::new(TransportErrorKind::Dns))?;
    let (sender, receiver) = mpsc::sync_channel(1);
    pool.submit(DnsJob { resolver, host, port, result: sender })?;
    loop {
        check_operation(context, deadline)?;
        match receiver.recv_timeout(blocking_slice(deadline)) {
            Ok(Ok(mut addresses)) => {
                if addresses.is_empty()
                    || addresses.len() > MAX_DNS_ADDRESSES
                    || addresses.capacity() > MAX_DNS_ADDRESSES
                    || addresses.iter().any(|address| address.port() != port)
                {
                    return Err(TransportError::new(TransportErrorKind::Dns));
                }
                addresses.sort_unstable();
                addresses.dedup();
                return Ok(addresses);
            }
            Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(TransportError::new(TransportErrorKind::Dns));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}
