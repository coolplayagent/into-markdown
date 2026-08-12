//! Safe ONNX Runtime policy, model validation, and bounded session caching.

use into_markdown_core::{BoxFuture, ConversionError, ExecutionContext, Tensor, TensorRuntime};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

const RUNTIME_ID: &str = "onnxruntime-cpu";
const MAX_THREADS: u16 = 64;

/// Tensor element types accepted at the OCR/runtime boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TensorElementType {
    /// IEEE-754 single precision values.
    Float32,
}

/// One validated model dimension.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Dimension {
    /// An exact positive dimension.
    Exact(usize),
    /// A bounded dynamic dimension.
    Dynamic { min: usize, max: usize },
}

/// One named model input or output contract.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TensorSpec {
    /// Exact UTF-8 ONNX graph name.
    pub name: String,
    /// Expected element type.
    pub element_type: TensorElementType,
    /// Expected rank and dimensions.
    pub dimensions: Vec<Dimension>,
}

/// Metadata read from a loaded ONNX session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelMetadata {
    /// ONNX operator-set version.
    pub opset: u32,
    /// Ordered input metadata.
    pub inputs: Vec<TensorSpec>,
    /// Ordered output metadata.
    pub outputs: Vec<TensorSpec>,
}

/// Audited metadata expected for one model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelContract {
    /// Exact supported operator-set version.
    pub opset: u32,
    /// Ordered input contract.
    pub inputs: Vec<TensorSpec>,
    /// Ordered output contract.
    pub outputs: Vec<TensorSpec>,
}

/// Canonical identity retained while a model is loaded.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelIdentity {
    /// Absolute canonical path below the configured model root.
    pub canonical_path: PathBuf,
    /// Lowercase SHA-256 recorded by the model authority.
    pub sha256: String,
    /// File size used for cache accounting.
    pub bytes: u64,
    /// Platform file identity captured from an opened no-follow handle.
    pub file_identity: String,
}

/// A model resolved through an opened-handle/no-follow implementation.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    /// Stable model identity.
    pub identity: ModelIdentity,
    /// Audited graph contract.
    pub contract: ModelContract,
    /// Hash-verified bytes read from the retained no-follow model handle.
    pub bytes: Arc<[u8]>,
}

/// Resolves only installed, hash-verified runtime ONNX models.
pub trait ModelResolver: Send + Sync {
    /// Resolve `model_id` without interpreting source archives as models.
    fn resolve(
        &self,
        model_id: &str,
        context: &ExecutionContext,
    ) -> Result<ResolvedModel, ConversionError>;
}

/// Product resolver backed by the embedded model authority.
///
/// Source archives are deliberately not interpreted as ONNX model files.
#[derive(Debug, Default)]
pub struct ManifestModelResolver;

impl ModelResolver for ManifestModelResolver {
    fn resolve(
        &self,
        model_id: &str,
        context: &ExecutionContext,
    ) -> Result<ResolvedModel, ConversionError> {
        context.checkpoint()?;
        let manifest = super::ModelManifest::embedded()?;
        let known = manifest.bundles.iter().any(|bundle| bundle.id == model_id);
        Err(ConversionError::ComponentUnavailable {
            component: "onnx-model".into(),
            detail: if known { "ModelUnavailable" } else { "UnknownModel" }.into(),
        })
    }
}

/// CPU session controls that participate in the cache key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionOptions {
    /// Intra-operator worker count.
    pub intra_op_threads: u16,
    /// Inter-operator worker count.
    pub inter_op_threads: u16,
    /// Whether the CPU arena is enabled.
    pub cpu_arena: bool,
    /// Maximum estimated session bytes.
    pub max_session_bytes: u64,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            intra_op_threads: 1,
            inter_op_threads: 1,
            cpu_arena: true,
            max_session_bytes: 256 * 1024 * 1024,
        }
    }
}

impl SessionOptions {
    fn validate(&self) -> Result<(), ConversionError> {
        if self.intra_op_threads == 0
            || self.inter_op_threads == 0
            || self.intra_op_threads > MAX_THREADS
            || self.inter_op_threads > MAX_THREADS
            || self.max_session_bytes == 0
        {
            return Err(runtime_error("invalidSessionOptions"));
        }
        Ok(())
    }
}

/// Count and memory bounds for the process-local LRU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheLimits {
    /// Maximum ready sessions.
    pub max_sessions: usize,
    /// Maximum sum of ready-session estimates.
    pub max_bytes: u64,
}

impl Default for CacheLimits {
    fn default() -> Self {
        Self { max_sessions: 4, max_bytes: 512 * 1024 * 1024 }
    }
}

/// Runtime and cache policy.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Exact runtime version verified by the native loader.
    pub runtime_version: String,
    /// CPU session controls.
    pub session: SessionOptions,
    /// Bounded LRU controls.
    pub cache: CacheLimits,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            runtime_version: runtime_authority().map_or_else(|_| String::new(), |value| value.0),
            session: SessionOptions::default(),
            cache: CacheLimits::default(),
        }
    }
}

/// Safe session seam implemented by the native ORT boundary or a test fake.
pub trait SessionAdapter: Send + Sync {
    /// Metadata reported by the loaded graph.
    fn metadata(&self) -> Result<ModelMetadata, ConversionError>;
    /// Estimated long-lived memory charged to the cache.
    fn estimated_bytes(&self) -> u64;
    /// Execute ordered tensors. Implementations must connect cancellation to ORT run options.
    fn run(
        &self,
        inputs: &[Tensor],
        context: &ExecutionContext,
    ) -> Result<Vec<Tensor>, ConversionError>;
}

/// Creates native sessions after the model and runtime identities are verified.
pub trait SessionFactory: Send + Sync {
    /// Create a CPU-only session.
    fn create(
        &self,
        model: &ResolvedModel,
        options: &SessionOptions,
        context: &ExecutionContext,
    ) -> Result<Arc<dyn SessionAdapter>, ConversionError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    model: ModelIdentity,
    options: SessionOptions,
    runtime_version: String,
}

struct CachedSession {
    session: Arc<dyn SessionAdapter>,
    bytes: u64,
    last_used: u64,
}

enum CacheEntry {
    Loading,
    Ready(CachedSession),
}

#[derive(Default)]
struct CacheState {
    entries: HashMap<CacheKey, CacheEntry>,
    clock: u64,
    ready_bytes: u64,
}

struct SessionCache {
    state: Mutex<CacheState>,
    changed: Condvar,
    limits: CacheLimits,
}

impl SessionCache {
    fn new(limits: CacheLimits) -> Result<Self, ConversionError> {
        if limits.max_sessions == 0 || limits.max_bytes == 0 {
            return Err(runtime_error("invalidCacheLimits"));
        }
        Ok(Self { state: Mutex::new(CacheState::default()), changed: Condvar::new(), limits })
    }

    fn get_or_create(
        &self,
        key: &CacheKey,
        model: &ResolvedModel,
        options: &SessionOptions,
        factory: &dyn SessionFactory,
        context: &ExecutionContext,
    ) -> Result<Arc<dyn SessionAdapter>, ConversionError> {
        loop {
            context.checkpoint()?;
            let mut state = lock(&self.state);
            state.clock = state.clock.saturating_add(1);
            let now = state.clock;
            match state.entries.get_mut(key) {
                Some(CacheEntry::Ready(cached)) => {
                    cached.last_used = now;
                    return Ok(Arc::clone(&cached.session));
                }
                Some(CacheEntry::Loading) => {
                    let (next, _) = self
                        .changed
                        .wait_timeout(state, std::time::Duration::from_millis(10))
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    drop(next);
                }
                None => {
                    state.entries.insert(key.clone(), CacheEntry::Loading);
                    drop(state);
                    let created = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        factory.create(model, options, context).and_then(|session| {
                            context.checkpoint()?;
                            validate_metadata(&model.contract, &session.metadata()?)?;
                            let bytes = session.estimated_bytes();
                            if bytes == 0
                                || bytes > options.max_session_bytes
                                || bytes > self.limits.max_bytes
                            {
                                return Err(resource_error("sessionMemory"));
                            }
                            Ok((session, bytes))
                        })
                    }))
                    .unwrap_or_else(|_| Err(runtime_error("sessionFactoryPanicked")));
                    let mut state = lock(&self.state);
                    match created {
                        Ok((session, bytes)) => {
                            state.clock = state.clock.saturating_add(1);
                            let last_used = state.clock;
                            state.ready_bytes = state.ready_bytes.saturating_add(bytes);
                            state.entries.insert(
                                key.clone(),
                                CacheEntry::Ready(CachedSession {
                                    session: Arc::clone(&session),
                                    bytes,
                                    last_used,
                                }),
                            );
                            evict(&mut state, key, self.limits);
                            self.changed.notify_all();
                            return Ok(session);
                        }
                        Err(error) => {
                            state.entries.remove(key);
                            self.changed.notify_all();
                            return Err(error);
                        }
                    }
                }
            }
        }
    }
}

fn evict(state: &mut CacheState, protected: &CacheKey, limits: CacheLimits) {
    loop {
        let ready_count =
            state.entries.values().filter(|entry| matches!(entry, CacheEntry::Ready(_))).count();
        if ready_count <= limits.max_sessions && state.ready_bytes <= limits.max_bytes {
            return;
        }
        let victim = state
            .entries
            .iter()
            .filter_map(|(key, entry)| match entry {
                CacheEntry::Ready(cached) if key != protected => {
                    Some((key.clone(), cached.last_used))
                }
                _ => None,
            })
            .min_by_key(|(_, used)| *used)
            .map(|(key, _)| key);
        let Some(victim) = victim else {
            return;
        };
        if let Some(CacheEntry::Ready(removed)) = state.entries.remove(&victim) {
            state.ready_bytes = state.ready_bytes.saturating_sub(removed.bytes);
        }
    }
}

/// Object-safe CPU ONNX runtime with validated sessions and single-flight caching.
pub struct OnnxRuntime {
    resolver: Arc<dyn ModelResolver>,
    factory: Arc<dyn SessionFactory>,
    config: RuntimeConfig,
    cache: SessionCache,
}

impl fmt::Debug for OnnxRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OnnxRuntime").field("config", &self.config).finish_non_exhaustive()
    }
}

impl OnnxRuntime {
    /// Construct after a native loader has verified the exact ORT version and ABI.
    pub fn new(
        resolver: Arc<dyn ModelResolver>,
        factory: Arc<dyn SessionFactory>,
        config: RuntimeConfig,
    ) -> Result<Self, ConversionError> {
        let (expected_version, _) = runtime_authority()?;
        if config.runtime_version != expected_version {
            return Err(runtime_error("runtimeVersionMismatch"));
        }
        config.session.validate()?;
        let cache = SessionCache::new(config.cache)?;
        Ok(Self { resolver, factory, config, cache })
    }
}

fn runtime_authority() -> Result<(String, u32), ConversionError> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Authority {
        version: String,
        api_version: u32,
        source: String,
        license: String,
        targets: std::collections::BTreeMap<String, serde_json::Value>,
    }
    let authority: Authority =
        serde_json::from_str(include_str!("../../../third_party/onnxruntime/manifest.json"))
            .map_err(|_| runtime_error("invalidRuntimeAuthority"))?;
    if authority.version.is_empty()
        || authority.api_version == 0
        || authority.source.is_empty()
        || authority.license != "MIT"
        || authority.targets.len() != 4
    {
        return Err(runtime_error("invalidRuntimeAuthority"));
    }
    Ok((authority.version, authority.api_version))
}

impl TensorRuntime for OnnxRuntime {
    fn id(&self) -> &'static str {
        RUNTIME_ID
    }

    fn run<'a>(
        &'a self,
        model_id: &'a str,
        inputs: &'a [Tensor],
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<Vec<Tensor>, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            let model = self.resolver.resolve(model_id, context)?;
            validate_resolved_model(&model)?;
            validate_tensors(&model.contract.inputs, inputs, "input")?;
            let key = CacheKey {
                model: model.identity.clone(),
                options: self.config.session.clone(),
                runtime_version: self.config.runtime_version.clone(),
            };
            let session = self.cache.get_or_create(
                &key,
                &model,
                &self.config.session,
                self.factory.as_ref(),
                context,
            )?;
            context.checkpoint()?;
            let input_bytes = tensor_bytes(inputs)?;
            let _reservation = context.reserve_memory(input_bytes)?;
            let outputs = session.run(inputs, context)?;
            context.checkpoint()?;
            let _output_reservation = context.reserve_memory(tensor_bytes(&outputs)?)?;
            validate_tensors(&model.contract.outputs, &outputs, "output")?;
            Ok(outputs)
        })
    }
}

fn validate_identity(identity: &ModelIdentity) -> Result<(), ConversionError> {
    if !identity.canonical_path.is_absolute()
        || identity.bytes == 0
        || identity.file_identity.is_empty()
        || identity.sha256.len() != 64
        || !identity
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(runtime_error("invalidModelIdentity"));
    }
    Ok(())
}

fn validate_resolved_model(model: &ResolvedModel) -> Result<(), ConversionError> {
    validate_identity(&model.identity)?;
    if usize::try_from(model.identity.bytes) != Ok(model.bytes.len())
        || format!("{:x}", Sha256::digest(&model.bytes)) != model.identity.sha256
    {
        return Err(runtime_error("modelHashMismatch"));
    }
    Ok(())
}

fn validate_metadata(
    expected: &ModelContract,
    actual: &ModelMetadata,
) -> Result<(), ConversionError> {
    validate_specs(&expected.inputs)?;
    validate_specs(&expected.outputs)?;
    validate_specs(&actual.inputs)?;
    validate_specs(&actual.outputs)?;
    if expected.opset != actual.opset {
        return Err(runtime_error("opsetMismatch"));
    }
    if expected.inputs != actual.inputs || expected.outputs != actual.outputs {
        return Err(runtime_error("modelIoMismatch"));
    }
    Ok(())
}

fn validate_specs(specs: &[TensorSpec]) -> Result<(), ConversionError> {
    if specs.is_empty() {
        return Err(runtime_error("emptyTensorContract"));
    }
    for spec in specs {
        if spec.name.is_empty()
            || spec.name.len() > 1024
            || spec.name.as_bytes().contains(&0)
            || spec.dimensions.is_empty()
            || spec.dimensions.iter().any(|dimension| match dimension {
                Dimension::Exact(value) => *value == 0,
                Dimension::Dynamic { min, max } => *min == 0 || min > max,
            })
        {
            return Err(runtime_error("invalidTensorContract"));
        }
    }
    Ok(())
}

fn validate_tensors(
    specs: &[TensorSpec],
    tensors: &[Tensor],
    direction: &'static str,
) -> Result<(), ConversionError> {
    if specs.len() != tensors.len() {
        return Err(runtime_error(if direction == "input" {
            "inputCountMismatch"
        } else {
            "outputCountMismatch"
        }));
    }
    for (spec, tensor) in specs.iter().zip(tensors) {
        if spec.dimensions.len() != tensor.shape.len()
            || !spec.dimensions.iter().zip(&tensor.shape).all(|(expected, actual)| match expected {
                Dimension::Exact(value) => value == actual,
                Dimension::Dynamic { min, max } => actual >= min && actual <= max,
            })
        {
            return Err(runtime_error(if direction == "input" {
                "inputShapeMismatch"
            } else {
                "outputShapeMismatch"
            }));
        }
        let elements =
            tensor.shape.iter().try_fold(1_usize, |total, value| total.checked_mul(*value));
        if elements != Some(tensor.values.len()) {
            return Err(runtime_error("tensorElementCountMismatch"));
        }
    }
    Ok(())
}

fn tensor_bytes(tensors: &[Tensor]) -> Result<u64, ConversionError> {
    tensors.iter().try_fold(0_u64, |total, tensor| {
        let values =
            u64::try_from(tensor.values.len()).map_err(|_| resource_error("tensorMemory"))?;
        let bytes = values.checked_mul(4).ok_or_else(|| resource_error("tensorMemory"))?;
        total.checked_add(bytes).ok_or_else(|| resource_error("tensorMemory"))
    })
}

fn runtime_error(detail: &'static str) -> ConversionError {
    ConversionError::Ocr { provider: RUNTIME_ID.into(), detail: detail.into() }
}

fn resource_error(limit: &'static str) -> ConversionError {
    ConversionError::ResourceLimit { limit, detail: "ONNX Runtime budget exceeded".into() }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use into_markdown_core::{CancellationToken, ExecutionOptions, ResourceLimits};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }

    fn spec(name: &str, dimensions: Vec<Dimension>) -> TensorSpec {
        TensorSpec { name: name.into(), element_type: TensorElementType::Float32, dimensions }
    }

    fn contract() -> ModelContract {
        ModelContract {
            opset: 18,
            inputs: vec![spec("image", vec![Dimension::Exact(1), Dimension::Exact(2)])],
            outputs: vec![spec("score", vec![Dimension::Exact(1)])],
        }
    }

    fn model(name: &str) -> ResolvedModel {
        let bytes: Arc<[u8]> = Arc::from([0_u8; 16]);
        ResolvedModel {
            identity: ModelIdentity {
                canonical_path: PathBuf::from(format!("/trusted/{name}.onnx")),
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                bytes: 16,
                file_identity: format!("device:inode:{name}"),
            },
            contract: contract(),
            bytes,
        }
    }

    struct FakeSession {
        metadata: ModelMetadata,
        bytes: u64,
    }

    impl SessionAdapter for FakeSession {
        fn metadata(&self) -> Result<ModelMetadata, ConversionError> {
            Ok(self.metadata.clone())
        }

        fn estimated_bytes(&self) -> u64 {
            self.bytes
        }

        fn run(
            &self,
            _inputs: &[Tensor],
            context: &ExecutionContext,
        ) -> Result<Vec<Tensor>, ConversionError> {
            context.checkpoint()?;
            Ok(vec![Tensor { shape: vec![1], values: vec![0.5] }])
        }
    }

    struct FakeFactory {
        calls: AtomicUsize,
        fail_until: usize,
        bytes: u64,
        delay: Duration,
    }

    impl FakeFactory {
        fn new(fail_until: usize, bytes: u64) -> Self {
            Self { calls: AtomicUsize::new(0), fail_until, bytes, delay: Duration::ZERO }
        }
    }

    impl SessionFactory for FakeFactory {
        fn create(
            &self,
            model: &ResolvedModel,
            _options: &SessionOptions,
            context: &ExecutionContext,
        ) -> Result<Arc<dyn SessionAdapter>, ConversionError> {
            context.checkpoint()?;
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            thread::sleep(self.delay);
            if call <= self.fail_until {
                return Err(runtime_error("sessionLoadFailed"));
            }
            Ok(Arc::new(FakeSession {
                metadata: ModelMetadata {
                    opset: model.contract.opset,
                    inputs: model.contract.inputs.clone(),
                    outputs: model.contract.outputs.clone(),
                },
                bytes: self.bytes,
            }))
        }
    }

    struct FakeResolver(ResolvedModel);

    impl ModelResolver for FakeResolver {
        fn resolve(
            &self,
            _model_id: &str,
            context: &ExecutionContext,
        ) -> Result<ResolvedModel, ConversionError> {
            context.checkpoint()?;
            Ok(self.0.clone())
        }
    }

    #[test]
    fn authority_drives_version_and_rejects_mismatch() {
        let (version, api) = runtime_authority().unwrap();
        assert!(!version.is_empty());
        assert!(api > 0);
        let mut config = RuntimeConfig::default();
        config.runtime_version.push_str("-wrong");
        assert!(
            OnnxRuntime::new(
                Arc::new(FakeResolver(model("a"))),
                Arc::new(FakeFactory::new(0, 8)),
                config,
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_names_opset_and_shapes_fail_stably() {
        let expected = contract();
        let mut actual = ModelMetadata {
            opset: expected.opset + 1,
            inputs: expected.inputs.clone(),
            outputs: expected.outputs.clone(),
        };
        assert!(
            format!("{}", validate_metadata(&expected, &actual).unwrap_err())
                .contains("opsetMismatch")
        );
        actual.opset = expected.opset;
        actual.inputs[0].name.push('\0');
        assert!(
            format!("{}", validate_metadata(&expected, &actual).unwrap_err())
                .contains("invalidTensorContract")
        );
        let error = validate_tensors(
            &expected.inputs,
            &[Tensor { shape: vec![1, 3], values: vec![0.0; 3] }],
            "input",
        )
        .unwrap_err();
        assert!(format!("{error}").contains("inputShapeMismatch"));
    }

    #[test]
    fn concurrent_load_is_single_flight() {
        let factory = Arc::new(FakeFactory {
            calls: AtomicUsize::new(0),
            fail_until: 0,
            bytes: 8,
            delay: Duration::from_millis(40),
        });
        let runtime = Arc::new(
            OnnxRuntime::new(
                Arc::new(FakeResolver(model("single"))),
                factory.clone(),
                RuntimeConfig::default(),
            )
            .unwrap(),
        );
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let runtime = Arc::clone(&runtime);
                thread::spawn(move || {
                    block_on(runtime.run(
                        "single",
                        &[Tensor { shape: vec![1, 2], values: vec![1.0, 2.0] }],
                        &context(),
                    ))
                    .unwrap();
                })
            })
            .collect();
        for handle in threads {
            handle.join().unwrap();
        }
        assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_load_does_not_poison_cache() {
        let factory = Arc::new(FakeFactory::new(1, 8));
        let runtime = OnnxRuntime::new(
            Arc::new(FakeResolver(model("retry"))),
            factory.clone(),
            RuntimeConfig::default(),
        )
        .unwrap();
        let input = [Tensor { shape: vec![1, 2], values: vec![1.0, 2.0] }];
        assert!(block_on(runtime.run("retry", &input, &context())).is_err());
        assert!(block_on(runtime.run("retry", &input, &context())).is_ok());
        assert_eq!(factory.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn panicking_factory_does_not_leave_a_loading_entry() {
        struct PanicFactory;
        impl SessionFactory for PanicFactory {
            fn create(
                &self,
                _model: &ResolvedModel,
                _options: &SessionOptions,
                _context: &ExecutionContext,
            ) -> Result<Arc<dyn SessionAdapter>, ConversionError> {
                panic!("fake native panic");
            }
        }
        let resolved = model("panic");
        let cache = SessionCache::new(CacheLimits::default()).unwrap();
        let options = SessionOptions::default();
        let key = CacheKey {
            model: resolved.identity.clone(),
            options: options.clone(),
            runtime_version: runtime_authority().unwrap().0,
        };
        for _ in 0..2 {
            assert!(
                cache.get_or_create(&key, &resolved, &options, &PanicFactory, &context()).is_err()
            );
            assert!(lock(&cache.state).entries.is_empty());
        }
    }

    #[test]
    fn lru_is_bounded_by_count_and_bytes() {
        let cache = SessionCache::new(CacheLimits { max_sessions: 1, max_bytes: 8 }).unwrap();
        let factory = FakeFactory::new(0, 8);
        let options = SessionOptions { max_session_bytes: 8, ..SessionOptions::default() };
        for name in ["a", "b", "a"] {
            let resolved = model(name);
            let key = CacheKey {
                model: resolved.identity.clone(),
                options: options.clone(),
                runtime_version: runtime_authority().unwrap().0,
            };
            cache.get_or_create(&key, &resolved, &options, &factory, &context()).unwrap();
        }
        assert_eq!(factory.calls.load(Ordering::SeqCst), 3);
        let state = lock(&cache.state);
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.ready_bytes, 8);
    }

    #[test]
    fn current_manifest_is_stably_unavailable() {
        let error = ManifestModelResolver.resolve("pp-ocrv6-tiny-zh-en", &context()).unwrap_err();
        assert_eq!(error.code().as_str(), "componentUnavailable");
        assert!(format!("{error}").contains("ModelUnavailable"));
    }

    #[test]
    fn cancellation_and_request_memory_budget_are_enforced() {
        let factory = Arc::new(FakeFactory::new(0, 8));
        let runtime = OnnxRuntime::new(
            Arc::new(FakeResolver(model("budget"))),
            factory.clone(),
            RuntimeConfig::default(),
        )
        .unwrap();
        let input = [Tensor { shape: vec![1, 2], values: vec![1.0, 2.0] }];
        let token = CancellationToken::new();
        token.cancel();
        let cancelled = ExecutionContext::new(
            ExecutionOptions { cancellation: token, ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        assert_eq!(
            block_on(runtime.run("budget", &input, &cancelled)).unwrap_err().code().as_str(),
            "cancelled"
        );
        assert_eq!(factory.calls.load(Ordering::SeqCst), 0);

        let limited = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 4, ..ResourceLimits::default() },
        );
        assert_eq!(
            block_on(runtime.run("budget", &input, &limited)).unwrap_err().code().as_str(),
            "resourceLimit"
        );
    }
}
