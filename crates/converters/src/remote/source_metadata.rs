use super::redirect::source_redirects;
use into_markdown_core::{ConversionError, ResolvedInput, ResolvedSource, SourceMetadata};
use into_markdown_http_transport::FetchedResource;

pub(super) fn resolved_source(
    resource: FetchedResource,
) -> Result<ResolvedSource, ConversionError> {
    let (bytes, reservation, final_url, media_type, filename, redirects) = resource.into_parts();
    let size = u64::try_from(bytes.len()).map_err(|_| ConversionError::ResourceLimit {
        limit: "max_input_bytes",
        detail: "remote source size cannot be represented as u64".into(),
    })?;
    Ok(ResolvedSource::with_memory_reservation(
        ResolvedInput {
            bytes,
            metadata: SourceMetadata {
                name: filename,
                media_type,
                uri: Some(final_url),
                size,
                redirects: source_redirects(redirects),
            },
        },
        reservation,
    ))
}
