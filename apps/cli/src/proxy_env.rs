//! Environment-derived model download routing.
//!
//! The library HTTP transport never reads ambient variables; this CLI layer
//! is the single reader. An explicit `INTO_MD_HTTPS_PROXY` overrides the
//! curl-style `HTTPS_PROXY`/`https_proxy` pair, and empty values mean unset.

use into_markdown_http_transport::{
    HttpClient, NoProxyList, ProxyConfig, RoutedConnectionFactory, SystemDnsResolver,
};
use std::sync::Arc;

/// Selected model download route from explicit environment variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DownloadRoute {
    /// No proxy variable is set; direct connections are used.
    Direct,
    /// A parsed CONNECT proxy with its exclusion list.
    Proxy {
        /// Parsed proxy endpoint.
        proxy: ProxyConfig,
        /// Hosts that bypass the proxy.
        no_proxy: NoProxyList,
        /// Name of the winning environment variable.
        source: &'static str,
    },
    /// A proxy variable is set but cannot be parsed.
    Invalid {
        /// Name of the offending environment variable.
        variable: &'static str,
        /// Stable parse failure.
        reason: String,
    },
}

/// Read and select the download route from the process environment.
pub(crate) fn download_route() -> DownloadRoute {
    route_from(
        non_empty_var("INTO_MD_HTTPS_PROXY"),
        non_empty_var("HTTPS_PROXY"),
        non_empty_var("https_proxy"),
        non_empty_var("INTO_MD_NO_PROXY")
            .or_else(|| non_empty_var("NO_PROXY"))
            .or_else(|| non_empty_var("no_proxy")),
    )
}

fn non_empty_var(name: &str) -> Option<String> {
    std::env::var_os(name)
        .and_then(|value| value.into_string().ok())
        .filter(|value| !value.is_empty())
}

fn route_from(
    override_value: Option<String>,
    upper: Option<String>,
    lower: Option<String>,
    no_proxy: Option<String>,
) -> DownloadRoute {
    let (value, variable) = match (override_value, upper, lower) {
        (Some(value), _, _) => (value, "INTO_MD_HTTPS_PROXY"),
        (None, Some(value), _) => (value, "HTTPS_PROXY"),
        (None, None, Some(value)) => (value, "https_proxy"),
        (None, None, None) => return DownloadRoute::Direct,
    };
    match ProxyConfig::parse(&value) {
        Ok(proxy) => DownloadRoute::Proxy {
            proxy,
            no_proxy: NoProxyList::parse(&no_proxy.unwrap_or_default()),
            source: variable,
        },
        Err(error) => DownloadRoute::Invalid { variable, reason: error.to_string() },
    }
}

/// Build the pinned model fetch client from the environment download route.
///
/// # Errors
///
/// Returns the offending variable name and reason when a proxy variable is
/// set but invalid; no network access happens in that case.
pub(crate) fn model_fetch_client(cli_insecure: bool) -> Result<HttpClient, (&'static str, String)> {
    let env_insecure = parse_insecure(
        std::env::var_os("INTO_MD_INSECURE").and_then(|value| value.into_string().ok()),
    )
    .map_err(|reason| ("INTO_MD_INSECURE", reason))?;
    let insecure = cli_insecure || env_insecure;
    match download_route() {
        DownloadRoute::Direct => {
            Ok(HttpClient::with_insecure(Arc::new(SystemDnsResolver), insecure))
        }
        DownloadRoute::Proxy { proxy, no_proxy, .. } => Ok(HttpClient::with_components(
            Arc::new(SystemDnsResolver),
            Arc::new(RoutedConnectionFactory::new(
                proxy,
                no_proxy,
                Arc::new(SystemDnsResolver),
                insecure,
            )),
        )),
        DownloadRoute::Invalid { variable, reason } => Err((variable, reason)),
    }
}

fn parse_insecure(value: Option<String>) -> Result<bool, String> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(false);
    };
    if value == "0" || value.eq_ignore_ascii_case("false") {
        return Ok(false);
    }
    if value == "1" || value.eq_ignore_ascii_case("true") {
        return Ok(true);
    }
    Err("expected one of 0, 1, false, or true".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_priority_prefers_the_explicit_override_then_upper_then_lower() {
        let DownloadRoute::Proxy { source, .. } = route_from(
            Some("http://override.test:1".into()),
            Some("http://upper.test:2".into()),
            Some("http://lower.test:3".into()),
            None,
        ) else {
            panic!("override must win");
        };
        assert_eq!(source, "INTO_MD_HTTPS_PROXY");
        let DownloadRoute::Proxy { proxy, source, .. } =
            route_from(None, Some("http://upper.test:2".into()), None, None)
        else {
            panic!("upper must win");
        };
        assert_eq!((source, proxy.redacted_endpoint().as_str()), ("HTTPS_PROXY", "upper.test:2"));
        let DownloadRoute::Proxy { source, .. } =
            route_from(None, None, Some("http://lower.test:3".into()), None)
        else {
            panic!("lower must win");
        };
        assert_eq!(source, "https_proxy");
        assert!(matches!(route_from(None, None, None, Some("*".into())), DownloadRoute::Direct));
    }

    #[test]
    fn invalid_proxy_variable_reports_the_offending_name_and_reason() {
        let DownloadRoute::Invalid { variable, reason } =
            route_from(Some("socks5://proxy.test:1080".into()), None, None, None)
        else {
            panic!("socks must be rejected");
        };
        assert_eq!(variable, "INTO_MD_HTTPS_PROXY");
        assert_eq!(reason, "only http:// proxy endpoints are supported");
    }

    #[test]
    fn insecure_environment_value_is_strictly_parsed() {
        for value in [None, Some(""), Some("0"), Some("false"), Some("FALSE")] {
            assert!(!parse_insecure(value.map(str::to_owned)).unwrap());
        }
        for value in ["1", "true", "TRUE"] {
            assert!(parse_insecure(Some(value.into())).unwrap());
        }
        assert_eq!(
            parse_insecure(Some("no".into())).unwrap_err(),
            "expected one of 0, 1, false, or true"
        );
    }
}
