use into_markdown_core::SourceRedirect;
use into_markdown_http_transport::RedirectHop;

pub(super) fn source_redirects(hops: Vec<RedirectHop>) -> Vec<SourceRedirect> {
    hops.into_iter()
        .map(|hop| SourceRedirect { from: hop.from, to: hop.to, status: hop.status })
        .collect()
}
