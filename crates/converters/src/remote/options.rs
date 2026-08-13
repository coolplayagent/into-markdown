use into_markdown_core::ConversionOptions;
use into_markdown_http_transport::{FetchLimits, NetworkPolicy};

pub(super) fn network_policy(options: &ConversionOptions) -> NetworkPolicy {
    NetworkPolicy {
        allow_network: options.network.enabled,
        allow_private_network: !options.network.deny_private_networks,
        allowed_hosts: options.network.allowed_hosts.clone(),
        max_redirects: options.network.max_redirects,
    }
}

pub(super) fn fetch_limits(options: &ConversionOptions) -> FetchLimits {
    FetchLimits {
        max_wire_bytes: options.limits.max_input_bytes,
        max_decoded_bytes: options.limits.max_input_bytes,
    }
}
