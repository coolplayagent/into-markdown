//! Explicit, authority-sized model download transport used only by `models install`.

use into_markdown::{
    AcquiredModelArtifact, ConversionError, ExecutionContext, ModelAcquisition, ModelFetcher,
    ModelManagerError, ResourceReservation, RuntimeArtifact,
};
use into_markdown_http_transport::{
    FetchLimits, HttpClient, NetworkPolicy, RedirectHop, TransportError, TransportErrorKind,
};
use std::io::{Cursor, Read};
use std::sync::Arc;

const PADDLE_DETECTOR_URL: &str = "https://paddle-model-ecology.bj.bcebos.com/paddlex/official_inference_model/paddle3.0.0/PP-OCRv6_tiny_det_onnx_infer.tar";
const PADDLE_RECOGNIZER_URL: &str = "https://paddle-model-ecology.bj.bcebos.com/paddlex/official_inference_model/paddle3.0.0/PP-OCRv6_tiny_rec_onnx_infer.tar";
const PADDLE_DICTIONARY_URL: &str = "https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/2661c7c0ef5c613e8f93c6e93b2e052399f0f854/ppocr/utils/dict/ppocrv6_tiny_dict.txt";
const WHISPER_SMALL_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/c521a4b02f422512d734391fdf08bb08c0862f68/ggml-small.bin";
const WHISPER_SMALL_XET_HASH: &str =
    "edd29d67e70b000132af65205b99bb774b77abc13d10103e14f80ce2242913e1";
/// Audited Hugging Face xet bridge domains that may serve one pinned object.
const WHISPER_SMALL_XET_BRIDGES: &[&str] = &["cas-bridge.xethub.hf.co", "us.aws.cdn.hf.co"];

#[derive(Default)]
pub(crate) struct PinnedModelFetcher {
    client: HttpClient,
}

impl PinnedModelFetcher {
    /// Build the fetcher from the environment-derived download route.
    ///
    /// # Errors
    ///
    /// Returns the offending variable name and reason when a proxy variable
    /// is set but invalid; no download is attempted in that case.
    pub(crate) fn from_environment(insecure: bool) -> Result<Self, (&'static str, String)> {
        crate::proxy_env::model_fetch_client(insecure).map(|client| Self { client })
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
        let wire_limit = expected_bytes.saturating_mul(101).saturating_div(100);
        let fetched = self
            .client
            .get(
                &artifact.url,
                &network_policy(&artifact.url)?,
                FetchLimits { max_wire_bytes: wire_limit, max_decoded_bytes: expected_bytes },
                context,
            )
            .map_err(map_transport)?;
        let (bytes, lease, final_url, _, _, redirects) = fetched.into_parts();
        validate_fetch_identity(&artifact.url, &final_url, &redirects)?;
        Ok(AcquiredModelArtifact {
            acquisition,
            bytes: Box::new(LeasedReader { cursor: Cursor::new(bytes), _lease: lease }),
        })
    }
}

fn validate_fetch_identity(
    source: &str,
    final_url: &str,
    redirects: &[RedirectHop],
) -> Result<(), ModelManagerError> {
    if source != WHISPER_SMALL_URL {
        if redirects.is_empty() && final_url == source {
            return Ok(());
        }
        return Err(unexpected_redirect());
    }
    let [redirect] = redirects else {
        return Err(unexpected_redirect());
    };
    let final_url = url::Url::parse(final_url).map_err(|_| unexpected_redirect())?;
    let mut segments = final_url.path_segments().ok_or_else(unexpected_redirect)?;
    let Some(region) = segments.next() else {
        return Err(unexpected_redirect());
    };
    let Some(repository) = segments.next() else {
        return Err(unexpected_redirect());
    };
    let Some(object) = segments.next() else {
        return Err(unexpected_redirect());
    };
    if final_url.scheme() != "https"
        || !WHISPER_SMALL_XET_BRIDGES.contains(&final_url.host_str().unwrap_or_default())
        || final_url.port().is_some()
        || region != "xet-bridge-us"
        || repository.is_empty()
        || object != WHISPER_SMALL_XET_HASH
        || segments.next().is_some()
        || redirect.from != WHISPER_SMALL_URL
        || redirect.to != final_url.as_str()
        || !matches!(redirect.status, 301 | 302 | 303 | 307 | 308)
    {
        return Err(unexpected_redirect());
    }
    Ok(())
}

fn unexpected_redirect() -> ModelManagerError {
    ModelManagerError::Execution(ConversionError::Network {
        detail: "model download returned an unauthorized redirect identity".into(),
    })
}

fn network_policy(url: &str) -> Result<NetworkPolicy, ModelManagerError> {
    let (allowed_hosts, max_redirects) = match url {
        PADDLE_DETECTOR_URL | PADDLE_RECOGNIZER_URL => {
            (vec!["paddle-model-ecology.bj.bcebos.com".into()], 0)
        }
        PADDLE_DICTIONARY_URL => (vec!["raw.githubusercontent.com".into()], 0),
        WHISPER_SMALL_URL => (
            vec![
                "huggingface.co".into(),
                "cas-bridge.xethub.hf.co".into(),
                "us.aws.cdn.hf.co".into(),
            ],
            1,
        ),
        _ => {
            return Err(ModelManagerError::Corrupt(
                "runtime artifact URL is not a pinned download authority".into(),
            ));
        }
    };
    Ok(NetworkPolicy {
        allow_network: true,
        allow_private_network: false,
        allowed_hosts,
        max_redirects,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_http_transport::{Connection, ConnectionFactory, DnsResolver};
    use std::collections::VecDeque;
    use std::io::{self, Write};
    use std::net::SocketAddr;
    use std::sync::Mutex;
    use std::time::Instant;

    struct PublicDns;

    impl DnsResolver for PublicDns {
        fn resolve(&self, _: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
            Ok(vec![SocketAddr::from(([8, 8, 8, 8], port))])
        }
    }

    struct ScriptedConnection {
        response: Cursor<Vec<u8>>,
    }

    impl Read for ScriptedConnection {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.response.read(output)
        }
    }

    impl Write for ScriptedConnection {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            Ok(input.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct ScriptedConnector {
        responses: Mutex<VecDeque<Vec<u8>>>,
        hosts: Mutex<Vec<String>>,
    }

    impl ScriptedConnector {
        fn new(responses: impl IntoIterator<Item = Vec<u8>>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                hosts: Mutex::new(Vec::new()),
            }
        }
    }

    impl ConnectionFactory for ScriptedConnector {
        fn connect(
            &self,
            _: &str,
            host: &str,
            _: SocketAddr,
            _: &ExecutionContext,
            _: Instant,
        ) -> Result<Box<dyn Connection>, TransportError> {
            self.hosts.lock().unwrap().push(host.into());
            let response = self.responses.lock().unwrap().pop_front().unwrap();
            Ok(Box::new(ScriptedConnection { response: Cursor::new(response) }))
        }
    }

    fn artifact(url: &str, size: u64) -> RuntimeArtifact {
        RuntimeArtifact {
            id: "test-model".into(),
            role: "model".into(),
            file_name: "model.bin".into(),
            url: url.into(),
            archive_sha256: None,
            archive_size: None,
            archive_member: None,
            archive_members: None,
            sha256: "00".repeat(32),
            size,
            platforms: vec![],
            license: "MIT".into(),
        }
    }

    fn context() -> ExecutionContext {
        ExecutionContext::new(Default::default(), Default::default())
    }

    fn scripted_fetcher(
        responses: impl IntoIterator<Item = Vec<u8>>,
    ) -> (PinnedModelFetcher, Arc<ScriptedConnector>) {
        let connector = Arc::new(ScriptedConnector::new(responses));
        let client = HttpClient::with_components(Arc::new(PublicDns), connector.clone());
        (PinnedModelFetcher { client }, connector)
    }

    #[test]
    fn pinned_authorities_reject_lookalike_and_mutated_whisper_urls() {
        let policy = network_policy(WHISPER_SMALL_URL).unwrap();
        assert_eq!(
            policy.allowed_hosts,
            ["huggingface.co", "cas-bridge.xethub.hf.co", "us.aws.cdn.hf.co"]
        );
        assert_eq!(policy.max_redirects, 1);
        assert!(
            network_policy(
                &WHISPER_SMALL_URL.replace("huggingface.co", "huggingface.co.evil.test")
            )
            .is_err()
        );
        assert!(network_policy(&WHISPER_SMALL_URL.replace("ggml-small.bin", "other.bin")).is_err());
    }

    #[test]
    fn whisper_redirect_reauthorizes_only_the_xet_bridge() {
        for status in [301, 302, 303, 307, 308] {
            let redirect = format!(
                "HTTP/1.1 {status} Redirect\r\nLocation: https://us.aws.cdn.hf.co/xet-bridge-us/repository/{WHISPER_SMALL_XET_HASH}?signature=secret\r\nContent-Length: 0\r\n\r\n"
            )
            .into_bytes();
            let body = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ndata".to_vec();
            let (fetcher, connector) = scripted_fetcher([redirect, body]);
            let mut acquired = fetcher.open(&artifact(WHISPER_SMALL_URL, 4), &context()).unwrap();
            let mut bytes = Vec::new();
            acquired.bytes.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"data");
            assert_eq!(
                *connector.hosts.lock().unwrap(),
                ["huggingface.co", "us.aws.cdn.hf.co"]
            );
        }

        let legacy_bridge = format!(
            "HTTP/1.1 302 Found\r\nLocation: https://cas-bridge.xethub.hf.co/xet-bridge-us/repository/{WHISPER_SMALL_XET_HASH}\r\nContent-Length: 0\r\n\r\n"
        )
        .into_bytes();
        let body = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ndata".to_vec();
        let (fetcher, _) = scripted_fetcher([legacy_bridge, body]);
        let mut acquired = fetcher.open(&artifact(WHISPER_SMALL_URL, 4), &context()).unwrap();
        let mut bytes = Vec::new();
        acquired.bytes.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"data");

        let mutated_object = format!(
            "HTTP/1.1 302 Found\r\nLocation: https://us.aws.cdn.hf.co/xet-bridge-us/repository/{}0\r\nContent-Length: 0\r\n\r\n",
            WHISPER_SMALL_XET_HASH
        )
        .into_bytes();
        let body = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ndata".to_vec();
        let (fetcher, _) = scripted_fetcher([mutated_object, body]);
        assert!(matches!(
            fetcher.open(&artifact(WHISPER_SMALL_URL, 4), &context()),
            Err(ModelManagerError::Execution(ConversionError::Network { .. }))
        ));

        let evil = b"HTTP/1.1 302 Found\r\nLocation: https://cas-bridge.xethub.hf.co.evil.test/model\r\nContent-Length: 0\r\n\r\n".to_vec();
        let (fetcher, connector) = scripted_fetcher([evil]);
        assert!(matches!(
            fetcher.open(&artifact(WHISPER_SMALL_URL, 4), &context()),
            Err(ModelManagerError::Execution(ConversionError::Network { .. }))
        ));
        assert_eq!(*connector.hosts.lock().unwrap(), ["huggingface.co"]);
    }

    #[test]
    fn whisper_download_fails_closed_on_missing_or_mutated_redirects() {
        let direct = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ndata".to_vec();
        let (fetcher, _) = scripted_fetcher([direct]);
        assert!(matches!(
            fetcher.open(&artifact(WHISPER_SMALL_URL, 4), &context()),
            Err(ModelManagerError::Execution(ConversionError::Network { .. }))
        ));

        let wrong_status = b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n".to_vec();
        let (fetcher, connector) = scripted_fetcher([wrong_status]);
        assert!(matches!(
            fetcher.open(&artifact(WHISPER_SMALL_URL, 4), &context()),
            Err(ModelManagerError::Execution(ConversionError::Network { .. }))
        ));
        assert_eq!(*connector.hosts.lock().unwrap(), ["huggingface.co"]);

        let extra_path = format!(
            "HTTP/1.1 302 Found\r\nLocation: https://cas-bridge.xethub.hf.co/xet-bridge-us/repository/{WHISPER_SMALL_XET_HASH}/extra?signature=secret\r\nContent-Length: 0\r\n\r\n"
        )
        .into_bytes();
        let body = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ndata".to_vec();
        let (fetcher, connector) = scripted_fetcher([extra_path, body]);
        assert!(matches!(
            fetcher.open(&artifact(WHISPER_SMALL_URL, 4), &context()),
            Err(ModelManagerError::Execution(ConversionError::Network { .. }))
        ));
        assert_eq!(*connector.hosts.lock().unwrap(), ["huggingface.co", "cas-bridge.xethub.hf.co"]);

        let first = format!(
            "HTTP/1.1 302 Found\r\nLocation: https://cas-bridge.xethub.hf.co/xet-bridge-us/repository/{WHISPER_SMALL_XET_HASH}?signature=first\r\nContent-Length: 0\r\n\r\n"
        )
        .into_bytes();
        let second = format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: https://cas-bridge.xethub.hf.co/xet-bridge-us/repository/{WHISPER_SMALL_XET_HASH}?signature=second\r\nContent-Length: 0\r\n\r\n"
        )
        .into_bytes();
        let (fetcher, connector) = scripted_fetcher([first, second]);
        assert!(matches!(
            fetcher.open(&artifact(WHISPER_SMALL_URL, 4), &context()),
            Err(ModelManagerError::Execution(ConversionError::Network { .. }))
        ));
        assert_eq!(*connector.hosts.lock().unwrap(), ["huggingface.co", "cas-bridge.xethub.hf.co"]);
    }

    #[test]
    fn model_download_enforces_the_exact_size_boundary() {
        let oversized = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n12345".to_vec();
        let (fetcher, _) = scripted_fetcher([oversized]);
        assert!(matches!(
            fetcher.open(&artifact(WHISPER_SMALL_URL, 4), &context()),
            Err(ModelManagerError::Execution(ConversionError::ResourceLimit {
                limit: "max_input_bytes",
                ..
            }))
        ));
    }
}
