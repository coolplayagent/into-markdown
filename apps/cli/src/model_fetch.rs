//! Explicit, authority-sized model download transport used only by `models install`.

use into_markdown::{
    AcquiredModelArtifact, ConversionError, ExecutionContext, ModelAcquisition, ModelFetcher,
    ModelManagerError, ResourceReservation, RuntimeArtifact,
};
use into_markdown_http_transport::{
    FetchLimits, HttpClient, NetworkPolicy, TransportError, TransportErrorKind,
};
use std::io::{Cursor, Read};
use std::sync::Arc;

pub(crate) struct PinnedModelFetcher {
    client: HttpClient,
}

impl Default for PinnedModelFetcher {
    fn default() -> Self {
        Self { client: HttpClient::default() }
    }
}

impl ModelFetcher for PinnedModelFetcher {
    fn open(
        &self,
        artifact: &RuntimeArtifact,
        context: &ExecutionContext,
    ) -> Result<AcquiredModelArtifact, ModelManagerError> {
        context.checkpoint()?;
        let (expected_bytes, acquisition) = match (
            artifact.archive_size,
            artifact.archive_sha256.as_ref(),
            artifact.archive_member.as_ref(),
        ) {
            (Some(size), Some(hash), Some(member)) => (
                size,
                ModelAcquisition::ArchiveMember {
                    archive_sha256: hash.clone(),
                    archive_size: size,
                    member: member.clone(),
                },
            ),
            (None, None, None) => (artifact.size, ModelAcquisition::Direct),
            _ => {
                return Err(ModelManagerError::Corrupt(
                    "runtime artifact has an incomplete acquisition authority".into(),
                ));
            }
        };
        let fetched = self
            .client
            .get(
                &artifact.url,
                &NetworkPolicy {
                    allow_network: true,
                    allow_private_network: false,
                    allowed_hosts: vec![
                        "paddle-model-ecology.bj.bcebos.com".into(),
                        "raw.githubusercontent.com".into(),
                    ],
                    max_redirects: 3,
                },
                FetchLimits { max_wire_bytes: expected_bytes, max_decoded_bytes: expected_bytes },
                context,
            )
            .map_err(map_transport)?;
        let (bytes, lease, _, _, _, _) = fetched.into_parts();
        Ok(AcquiredModelArtifact {
            acquisition,
            bytes: Box::new(LeasedReader { cursor: Cursor::new(bytes), _lease: lease }),
        })
    }
}

struct LeasedReader {
    cursor: Cursor<Arc<[u8]>>,
    _lease: ResourceReservation,
}

impl Read for LeasedReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        self.cursor.read(output)
    }
}

fn map_transport(error: TransportError) -> ModelManagerError {
    let conversion = match error.kind() {
        TransportErrorKind::Cancelled => ConversionError::Cancelled,
        TransportErrorKind::Timeout => ConversionError::Timeout,
        TransportErrorKind::ResourceLimit => ConversionError::ResourceLimit {
            limit: "max_input_bytes",
            detail: "model download exceeded its exact artifact authority".into(),
        },
        _ => ConversionError::Network { detail: error.to_string() },
    };
    ModelManagerError::Execution(conversion)
}
