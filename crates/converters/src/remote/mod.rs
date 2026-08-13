//! HTTP(S) source resolver assembly.

mod options;
mod redirect;
mod source_metadata;

use self::options::{fetch_limits, network_policy};
use self::source_metadata::resolved_source;
use super::BlockingPool;
use into_markdown_core::{
    BoxFuture, ConversionError, ConversionOptions, ExecutionContext, InputRef, ResolvedInput,
    ResolvedSource, SourceResolver,
};
use into_markdown_http_transport::{HttpClient, TransportError, TransportErrorKind};
use std::sync::{Arc, OnceLock};

const HTTP_WORKER_COUNT: usize = 4;
const HTTP_QUEUE_CAPACITY: usize = 16;

fn http_pool() -> Result<&'static BlockingPool, ConversionError> {
    static POOL: OnceLock<Result<BlockingPool, String>> = OnceLock::new();
    POOL.get_or_init(|| {
        BlockingPool::new(
            "into-md-http-source",
            HTTP_WORKER_COUNT,
            HTTP_QUEUE_CAPACITY,
            "blocking_http_source_queue",
        )
    })
    .as_ref()
    .map_err(|detail| ConversionError::ComponentUnavailable {
        component: "blocking-http-source-workers".into(),
        detail: detail.clone(),
    })
}

/// Audited, explicitly authorized HTTP(S) source resolver.
#[derive(Clone)]
pub struct HttpSourceResolver {
    client: Arc<HttpClient>,
}

impl std::fmt::Debug for HttpSourceResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("HttpSourceResolver").finish_non_exhaustive()
    }
}

impl Default for HttpSourceResolver {
    fn default() -> Self {
        Self { client: Arc::new(HttpClient::default()) }
    }
}

impl HttpSourceResolver {
    /// Construct a resolver over an injected audited client.
    #[must_use]
    pub fn with_client(client: Arc<HttpClient>) -> Self {
        Self { client }
    }

    fn resolve_accounted_owned(
        &self,
        input: &InputRef,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<super::BlockingFuture<ResolvedSource>, ConversionError> {
        context.checkpoint()?;
        let InputRef::Uri(source) = input else {
            return Err(ConversionError::Unsupported {
                detail: "HTTP source resolver accepts only URI inputs".into(),
            });
        };
        if !options.network.enabled {
            return Err(ConversionError::Network {
                detail: "network resolution is disabled by default".into(),
            });
        }
        let source = source.clone();
        let policy = network_policy(options);
        let limits = fetch_limits(options);
        let client = Arc::clone(&self.client);
        let worker_context = context.clone();
        http_pool()?.submit(move || {
            let resource = client
                .get(&source, &policy, limits, &worker_context)
                .map_err(map_transport_error)?;
            resolved_source(resource)
        })
    }
}

impl SourceResolver for HttpSourceResolver {
    fn id(&self) -> &'static str {
        "builtin.source.http"
    }

    fn supports(&self, input: &InputRef) -> bool {
        matches!(input, InputRef::Uri(_))
    }

    fn resolve<'a>(
        &'a self,
        input: &'a InputRef,
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ResolvedInput, ConversionError>> {
        Box::pin(async move {
            self.resolve_accounted_owned(input, options, context)?
                .await
                .map(ResolvedSource::into_input)
        })
    }

    fn resolve_accounted<'a>(
        &'a self,
        input: &'a InputRef,
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ResolvedSource, ConversionError>> {
        Box::pin(async move { self.resolve_accounted_owned(input, options, context)?.await })
    }
}

fn map_transport_error(error: TransportError) -> ConversionError {
    match error.kind() {
        TransportErrorKind::Cancelled => ConversionError::Cancelled,
        TransportErrorKind::Timeout => ConversionError::Timeout,
        TransportErrorKind::ResourceLimit => ConversionError::ResourceLimit {
            limit: "max_input_bytes",
            detail: "remote source exceeded an HTTP or decoded-source budget".into(),
        },
        TransportErrorKind::Unavailable => ConversionError::ComponentUnavailable {
            component: "builtin.source.http".into(),
            detail: "audited HTTP transport is unavailable".into(),
        },
        TransportErrorKind::NetworkDenied
        | TransportErrorKind::HostDenied
        | TransportErrorKind::PrivateNetworkDenied
        | TransportErrorKind::Dns
        | TransportErrorKind::Connect
        | TransportErrorKind::Tls
        | TransportErrorKind::Http
        | TransportErrorKind::InvalidMessage => {
            ConversionError::Network { detail: error.to_string() }
        }
        _ => ConversionError::Network { detail: "HTTP source resolution failed".into() },
    }
}
