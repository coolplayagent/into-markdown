//! Safe ONNX Runtime policy, model validation, and bounded session caching.

use into_markdown_core::{
    BoxFuture, ConversionError, ExecutionContext, ResourceReservation, Tensor, TensorRuntime,
};
use prost::Message;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

const RUNTIME_ID: &str = "onnxruntime-cpu";
const MAX_THREADS: u16 = 64;
const MAX_PROTO_FIELDS: usize = 1_000_000;
const MAX_OPSET_IMPORTS: usize = 64;
const MAX_DOMAIN_BYTES: usize = 256;
const MAX_GRAPH_INITIALIZERS: usize = 65_536;
const MAX_PROTO_DEPTH: usize = 8;
/// Maximum rank accepted for any runtime tensor or contract entry.
pub const MAX_TENSOR_RANK: usize = 16;
/// Maximum number of inputs or outputs accepted by one runtime session.
pub const MAX_TENSORS: usize = 64;
/// Maximum UTF-8 byte length of an ONNX input or output name.
pub const MAX_TENSOR_NAME_BYTES: usize = 256;

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
    /// Exact ONNX IR version parsed from `ModelProto`.
    pub ir_version: u64,
    /// Exact canonical operator-set imports parsed from `ModelProto`.
    pub opsets: BTreeMap<String, u64>,
    /// Ordered input metadata.
    pub inputs: Vec<TensorSpec>,
    /// Ordered graph inputs that are also initializers and may be overridden.
    pub overridable_inputs: Vec<TensorSpec>,
    /// Ordered output metadata.
    pub outputs: Vec<TensorSpec>,
}

/// Audited metadata expected for one model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelContract {
    /// Exact ONNX IR version bound by the model authority.
    pub ir_version: u64,
    /// Exact canonical operator-set imports bound by the model authority.
    pub opsets: BTreeMap<String, u64>,
    /// Ordered input contract.
    pub inputs: Vec<TensorSpec>,
    /// Ordered graph inputs that are also initializers and may be overridden.
    pub overridable_inputs: Vec<TensorSpec>,
    /// Ordered output contract.
    pub outputs: Vec<TensorSpec>,
    /// Conservative upper bound for one live native session.
    pub session_memory_bytes: u64,
    /// Conservative upper bound for native scratch allocation during one run.
    pub run_memory_bytes: u64,
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
    /// Request accounting retained while the model bytes are live.
    pub memory_reservation: Option<Arc<ResourceReservation>>,
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
            cpu_arena: false,
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
    /// Return the conservative session allocation charged before loading starts.
    fn estimate_bytes(
        &self,
        model: &ResolvedModel,
        options: &SessionOptions,
    ) -> Result<u64, ConversionError>;
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
    contract_sha256: String,
    options: SessionOptions,
    runtime_version: String,
}

struct CachedSession {
    session: Arc<dyn SessionAdapter>,
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
}

#[derive(Default)]
struct LiveBudgetState {
    sessions: usize,
    bytes: u64,
}

struct LiveBudget {
    state: Mutex<LiveBudgetState>,
    limits: CacheLimits,
}

impl LiveBudget {
    fn try_reserve(self: &Arc<Self>, bytes: u64) -> Result<PendingLease, ConversionError> {
        let mut state = lock(&self.state);
        let next_sessions =
            state.sessions.checked_add(1).ok_or_else(|| resource_error("sessionCount"))?;
        let next_bytes =
            state.bytes.checked_add(bytes).ok_or_else(|| resource_error("sessionMemory"))?;
        if next_sessions > self.limits.max_sessions {
            return Err(resource_error("sessionCount"));
        }
        if next_bytes > self.limits.max_bytes {
            return Err(resource_error("sessionMemory"));
        }
        state.sessions = next_sessions;
        state.bytes = next_bytes;
        Ok(PendingLease { budget: Arc::clone(self), bytes, active: true })
    }

    fn can_reserve(&self, bytes: u64) -> bool {
        let state = lock(&self.state);
        state.sessions < self.limits.max_sessions
            && state.bytes.checked_add(bytes).is_some_and(|sum| sum <= self.limits.max_bytes)
    }

    fn release(&self, bytes: u64) {
        let mut state = lock(&self.state);
        state.sessions = state.sessions.checked_sub(1).expect("live session count underflow");
        state.bytes = state.bytes.checked_sub(bytes).expect("live session bytes underflow");
    }
}

struct PendingLease {
    budget: Arc<LiveBudget>,
    bytes: u64,
    active: bool,
}

impl PendingLease {
    fn into_session(mut self, session: Arc<dyn SessionAdapter>) -> Arc<dyn SessionAdapter> {
        self.active = false;
        Arc::new(LeasedSession {
            session: Some(session),
            budget: Arc::clone(&self.budget),
            reserved_bytes: self.bytes,
        })
    }
}

impl Drop for PendingLease {
    fn drop(&mut self) {
        if self.active {
            self.budget.release(self.bytes);
        }
    }
}

struct LeasedSession {
    session: Option<Arc<dyn SessionAdapter>>,
    budget: Arc<LiveBudget>,
    reserved_bytes: u64,
}

impl SessionAdapter for LeasedSession {
    fn metadata(&self) -> Result<ModelMetadata, ConversionError> {
        self.session.as_ref().expect("live leased session").metadata()
    }

    fn estimated_bytes(&self) -> u64 {
        self.reserved_bytes
    }

    fn run(
        &self,
        inputs: &[Tensor],
        context: &ExecutionContext,
    ) -> Result<Vec<Tensor>, ConversionError> {
        self.session.as_ref().expect("live leased session").run(inputs, context)
    }
}

impl Drop for LeasedSession {
    fn drop(&mut self) {
        // Native session destruction may itself touch allocator/runtime state;
        // keep it charged until that destructor has completed.
        drop(self.session.take());
        self.budget.release(self.reserved_bytes);
    }
}

struct SessionCache {
    state: Mutex<CacheState>,
    changed: Condvar,
    live: Arc<LiveBudget>,
}

impl SessionCache {
    fn new(limits: CacheLimits) -> Result<Self, ConversionError> {
        if limits.max_sessions == 0 || limits.max_bytes == 0 {
            return Err(runtime_error("invalidCacheLimits"));
        }
        Ok(Self {
            state: Mutex::new(CacheState::default()),
            changed: Condvar::new(),
            live: Arc::new(LiveBudget { state: Mutex::new(LiveBudgetState::default()), limits }),
        })
    }

    fn get_or_create(
        &self,
        key: &CacheKey,
        model: &ResolvedModel,
        options: &SessionOptions,
        factory: &dyn SessionFactory,
        context: &ExecutionContext,
    ) -> Result<Arc<dyn SessionAdapter>, ConversionError> {
        let factory_estimate = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            factory.estimate_bytes(model, options)
        }))
        .unwrap_or_else(|_| Err(runtime_error("sessionFactoryPanicked")))?;
        let estimate = model.contract.session_memory_bytes;
        if factory_estimate != estimate
            || estimate == 0
            || estimate > options.max_session_bytes
            || estimate > self.live.limits.max_bytes
        {
            return Err(resource_error("sessionMemory"));
        }
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
                    evict_releasable(&mut state, key, estimate, &self.live);
                    let lease = self.live.try_reserve(estimate)?;
                    state.entries.insert(key.clone(), CacheEntry::Loading);
                    drop(state);
                    let created = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        factory.create(model, options, context).and_then(|session| {
                            context.checkpoint()?;
                            validate_metadata(&model.contract, &session.metadata()?)?;
                            let actual = session.estimated_bytes();
                            if actual == 0 || actual > estimate {
                                return Err(resource_error("sessionMemory"));
                            }
                            Ok(session)
                        })
                    }))
                    .unwrap_or_else(|_| Err(runtime_error("sessionFactoryPanicked")));
                    let mut state = lock(&self.state);
                    match created {
                        Ok(session) => {
                            let session = lease.into_session(session);
                            state.clock = state.clock.saturating_add(1);
                            let last_used = state.clock;
                            state.entries.insert(
                                key.clone(),
                                CacheEntry::Ready(CachedSession {
                                    session: Arc::clone(&session),
                                    last_used,
                                }),
                            );
                            self.changed.notify_all();
                            return Ok(session);
                        }
                        Err(error) => {
                            state.entries.remove(key);
                            drop(state);
                            drop(lease);
                            self.changed.notify_all();
                            return Err(error);
                        }
                    }
                }
            }
        }
    }
}

fn evict_releasable(
    state: &mut CacheState,
    protected: &CacheKey,
    required_bytes: u64,
    live: &LiveBudget,
) {
    while !live.can_reserve(required_bytes) {
        let victim = state
            .entries
            .iter()
            .filter_map(|(key, entry)| match entry {
                CacheEntry::Ready(cached)
                    if key != protected && Arc::strong_count(&cached.session) == 1 =>
                {
                    Some((key.clone(), cached.last_used))
                }
                _ => None,
            })
            .min_by_key(|(_, used)| *used)
            .map(|(key, _)| key);
        let Some(victim) = victim else {
            return;
        };
        state.entries.remove(&victim);
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
                contract_sha256: contract_sha256(&model.contract)?,
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
            let run_peak = run_memory_peak(inputs, &model.contract)?;
            // Held before adapters clone input backing, ORT executes, or output
            // values are copied out of native storage.
            let _run_reservation = context.reserve_memory(run_peak)?;
            let outputs = session.run(inputs, context)?;
            context.checkpoint()?;
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
    validate_specs(&model.contract.inputs)?;
    validate_specs_allow_empty(&model.contract.overridable_inputs)?;
    validate_specs(&model.contract.outputs)?;
    validate_distinct_spec_names(&model.contract.inputs, &model.contract.overridable_inputs)?;
    if model.contract.ir_version == 0
        || i64::try_from(model.contract.ir_version).is_err()
        || model.contract.opsets.is_empty()
        || model.contract.opsets.iter().any(|(domain, version)| {
            domain == "ai.onnx"
                || domain.len() > MAX_DOMAIN_BYTES
                || domain.as_bytes().contains(&0)
                || *version == 0
                || i64::try_from(*version).is_err()
        })
        || model.contract.session_memory_bytes == 0
        || model.contract.session_memory_bytes
            < model
                .identity
                .bytes
                .checked_add(
                    contract_metadata_bytes(&model.contract)?
                        .checked_mul(2)
                        .ok_or_else(|| resource_error("tensorMemory"))?,
                )
                .ok_or_else(|| resource_error("tensorMemory"))?
        || model.contract.run_memory_bytes == 0
    {
        return Err(runtime_error("invalidModelContract"));
    }
    let graph = parse_model_proto(&model.bytes)?;
    if graph.ir_version != model.contract.ir_version {
        return Err(runtime_error("modelIrVersionMismatch"));
    }
    if graph.opsets != model.contract.opsets {
        return Err(runtime_error("opsetMismatch"));
    }
    validate_graph_specs(&graph.inputs, &model.contract.inputs)?;
    validate_graph_specs(&graph.overridable_inputs, &model.contract.overridable_inputs)?;
    validate_graph_specs(&graph.outputs, &model.contract.outputs)?;
    Ok(())
}

fn validate_metadata(
    expected: &ModelContract,
    actual: &ModelMetadata,
) -> Result<(), ConversionError> {
    validate_specs(&expected.inputs)?;
    validate_specs_allow_empty(&expected.overridable_inputs)?;
    validate_specs(&expected.outputs)?;
    validate_specs(&actual.inputs)?;
    validate_specs_allow_empty(&actual.overridable_inputs)?;
    validate_specs(&actual.outputs)?;
    if expected.ir_version != actual.ir_version || expected.opsets != actual.opsets {
        return Err(runtime_error("opsetMismatch"));
    }
    if expected.inputs != actual.inputs
        || expected.overridable_inputs != actual.overridable_inputs
        || expected.outputs != actual.outputs
    {
        return Err(runtime_error("modelIoMismatch"));
    }
    Ok(())
}

fn validate_specs(specs: &[TensorSpec]) -> Result<(), ConversionError> {
    if specs.is_empty() {
        return Err(runtime_error("emptyTensorContract"));
    }
    validate_specs_allow_empty(specs)
}

fn validate_specs_allow_empty(specs: &[TensorSpec]) -> Result<(), ConversionError> {
    if specs.len() > MAX_TENSORS {
        return Err(runtime_error("invalidTensorContract"));
    }
    let mut names = std::collections::BTreeSet::new();
    for spec in specs {
        if spec.name.is_empty()
            || spec.name.len() > MAX_TENSOR_NAME_BYTES
            || spec.name.as_bytes().contains(&0)
            || spec.dimensions.is_empty()
            || spec.dimensions.len() > MAX_TENSOR_RANK
            || !names.insert(spec.name.as_str())
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

fn validate_distinct_spec_names(
    left: &[TensorSpec],
    right: &[TensorSpec],
) -> Result<(), ConversionError> {
    let left =
        left.iter().map(|spec| spec.name.as_str()).collect::<std::collections::BTreeSet<_>>();
    if right.iter().any(|spec| left.contains(spec.name.as_str())) {
        return Err(runtime_error("invalidTensorContract"));
    }
    Ok(())
}

fn validate_tensors(
    specs: &[TensorSpec],
    tensors: &[Tensor],
    direction: &'static str,
) -> Result<(), ConversionError> {
    if specs.len() > MAX_TENSORS
        || tensors.len() > MAX_TENSORS
        || tensors.iter().any(|tensor| tensor.shape.len() > MAX_TENSOR_RANK)
    {
        return Err(runtime_error("invalidTensorContract"));
    }
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

fn tensor_storage_bytes(tensors: &[Tensor]) -> Result<u64, ConversionError> {
    tensors.iter().try_fold(0_u64, |total, tensor| {
        let values =
            u64::try_from(tensor.values.len()).map_err(|_| resource_error("tensorMemory"))?;
        let shape =
            u64::try_from(tensor.shape.len()).map_err(|_| resource_error("tensorMemory"))?;
        let bytes = values
            .checked_mul(u64::try_from(std::mem::size_of::<f32>()).unwrap())
            .and_then(|bytes| {
                shape
                    .checked_mul(u64::try_from(std::mem::size_of::<usize>()).unwrap())
                    .and_then(|shape_bytes| bytes.checked_add(shape_bytes))
            })
            .ok_or_else(|| resource_error("tensorMemory"))?;
        total.checked_add(bytes).ok_or_else(|| resource_error("tensorMemory"))
    })
}

fn max_tensor_storage_bytes(specs: &[TensorSpec]) -> Result<u64, ConversionError> {
    specs.iter().try_fold(0_u64, |total, spec| {
        let elements = spec.dimensions.iter().try_fold(1_u64, |count, dimension| {
            let maximum = match dimension {
                Dimension::Exact(value) => *value,
                Dimension::Dynamic { max, .. } => *max,
            };
            count
                .checked_mul(u64::try_from(maximum).map_err(|_| resource_error("tensorMemory"))?)
                .ok_or_else(|| resource_error("tensorMemory"))
        })?;
        let shape_bytes = u64::try_from(spec.dimensions.len())
            .map_err(|_| resource_error("tensorMemory"))?
            .checked_mul(u64::try_from(std::mem::size_of::<usize>()).unwrap())
            .ok_or_else(|| resource_error("tensorMemory"))?;
        total
            .checked_add(
                elements
                    .checked_mul(u64::try_from(std::mem::size_of::<f32>()).unwrap())
                    .and_then(|bytes| bytes.checked_add(shape_bytes))
                    .ok_or_else(|| resource_error("tensorMemory"))?,
            )
            .ok_or_else(|| resource_error("tensorMemory"))
    })
}

fn contract_metadata_bytes(contract: &ModelContract) -> Result<u64, ConversionError> {
    fn specs_bytes(specs: &[TensorSpec]) -> Result<u64, ConversionError> {
        specs.iter().try_fold(0_u64, |total, spec| {
            let name =
                u64::try_from(spec.name.len()).map_err(|_| resource_error("tensorMemory"))?;
            let dimensions = u64::try_from(spec.dimensions.len())
                .map_err(|_| resource_error("tensorMemory"))?
                .checked_mul(u64::try_from(std::mem::size_of::<Dimension>()).unwrap())
                .ok_or_else(|| resource_error("tensorMemory"))?;
            let structure = u64::try_from(std::mem::size_of::<TensorSpec>()).unwrap();
            total
                .checked_add(name)
                .and_then(|bytes| bytes.checked_add(dimensions))
                .and_then(|bytes| bytes.checked_add(structure))
                .ok_or_else(|| resource_error("tensorMemory"))
        })
    }
    let opsets = contract.opsets.iter().try_fold(0_u64, |total, (domain, _)| {
        let domain = u64::try_from(domain.len()).map_err(|_| resource_error("tensorMemory"))?;
        total
            .checked_add(domain)
            .and_then(|bytes| {
                bytes.checked_add(u64::try_from(std::mem::size_of::<(String, u64)>()).unwrap())
            })
            .ok_or_else(|| resource_error("tensorMemory"))
    })?;
    let specs = specs_bytes(&contract.inputs)?
        .checked_add(specs_bytes(&contract.overridable_inputs)?)
        .and_then(|bytes| bytes.checked_add(specs_bytes(&contract.outputs).ok()?))
        .ok_or_else(|| resource_error("tensorMemory"))?;
    specs
        .checked_add(opsets)
        .and_then(|bytes| {
            bytes.checked_add(u64::try_from(std::mem::size_of::<ModelMetadata>()).unwrap())
        })
        .ok_or_else(|| resource_error("tensorMemory"))
}

fn run_memory_peak(inputs: &[Tensor], contract: &ModelContract) -> Result<u64, ConversionError> {
    let input_clone = tensor_storage_bytes(inputs)?;
    let input_entries = u64::try_from(inputs.len())
        .map_err(|_| resource_error("tensorMemory"))?
        .checked_mul(u64::try_from(std::mem::size_of::<(String, Tensor)>()).unwrap())
        .ok_or_else(|| resource_error("tensorMemory"))?;
    let output_entries = u64::try_from(contract.outputs.len())
        .map_err(|_| resource_error("tensorMemory"))?
        .checked_mul(u64::try_from(std::mem::size_of::<Tensor>()).unwrap())
        .ok_or_else(|| resource_error("tensorMemory"))?;
    let output_storage = max_tensor_storage_bytes(&contract.outputs)?;
    // Output storage is charged twice: once for ORT-owned tensor backing and
    // once for the checked Rust copy returned across the runtime boundary.
    input_clone
        .checked_add(input_entries)
        .and_then(|bytes| bytes.checked_add(output_entries))
        .and_then(|bytes| output_storage.checked_mul(2).and_then(|peak| bytes.checked_add(peak)))
        .and_then(|bytes| bytes.checked_add(contract.run_memory_bytes))
        .ok_or_else(|| resource_error("tensorMemory"))
}

fn contract_sha256(contract: &ModelContract) -> Result<String, ConversionError> {
    fn push_u64(digest: &mut Sha256, value: u64) {
        digest.update(value.to_le_bytes());
    }
    fn push_text(digest: &mut Sha256, value: &str) -> Result<(), ConversionError> {
        push_u64(
            digest,
            u64::try_from(value.len()).map_err(|_| runtime_error("invalidModelContract"))?,
        );
        digest.update(value.as_bytes());
        Ok(())
    }
    fn push_specs(digest: &mut Sha256, specs: &[TensorSpec]) -> Result<(), ConversionError> {
        push_u64(
            digest,
            u64::try_from(specs.len()).map_err(|_| runtime_error("invalidModelContract"))?,
        );
        for spec in specs {
            push_text(digest, &spec.name)?;
            digest.update([match spec.element_type {
                TensorElementType::Float32 => 1,
            }]);
            push_u64(
                digest,
                u64::try_from(spec.dimensions.len())
                    .map_err(|_| runtime_error("invalidModelContract"))?,
            );
            for dimension in &spec.dimensions {
                match dimension {
                    Dimension::Exact(value) => {
                        digest.update([1]);
                        push_u64(
                            digest,
                            u64::try_from(*value)
                                .map_err(|_| runtime_error("invalidModelContract"))?,
                        );
                    }
                    Dimension::Dynamic { min, max } => {
                        digest.update([2]);
                        push_u64(
                            digest,
                            u64::try_from(*min)
                                .map_err(|_| runtime_error("invalidModelContract"))?,
                        );
                        push_u64(
                            digest,
                            u64::try_from(*max)
                                .map_err(|_| runtime_error("invalidModelContract"))?,
                        );
                    }
                }
            }
        }
        Ok(())
    }
    let mut digest = Sha256::new();
    digest.update(b"into-markdown:model-contract\0");
    push_u64(&mut digest, contract.ir_version);
    push_u64(
        &mut digest,
        u64::try_from(contract.opsets.len()).map_err(|_| runtime_error("invalidModelContract"))?,
    );
    for (domain, version) in &contract.opsets {
        push_text(&mut digest, domain)?;
        push_u64(&mut digest, *version);
    }
    push_specs(&mut digest, &contract.inputs)?;
    push_specs(&mut digest, &contract.overridable_inputs)?;
    push_specs(&mut digest, &contract.outputs)?;
    push_u64(&mut digest, contract.session_memory_bytes);
    push_u64(&mut digest, contract.run_memory_bytes);
    Ok(format!("{:x}", digest.finalize()))
}

#[derive(Debug)]
struct ParsedModelProto {
    ir_version: u64,
    opsets: BTreeMap<String, u64>,
    inputs: Vec<ParsedTensorSpec>,
    overridable_inputs: Vec<ParsedTensorSpec>,
    outputs: Vec<ParsedTensorSpec>,
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedTensorSpec {
    name: String,
    dimensions: Vec<ParsedDimension>,
}

#[derive(Debug, PartialEq, Eq)]
enum ParsedDimension {
    Exact(usize),
    Symbolic(String),
}

fn parse_model_proto(bytes: &[u8]) -> Result<ParsedModelProto, ConversionError> {
    let mut field_budget = MAX_PROTO_FIELDS;
    preflight_model(bytes, &mut field_budget, 0)?;
    let model = crate::onnx_proto::ModelProto::decode(bytes)
        .map_err(|_| runtime_error("invalidOnnxProtobuf"))?;
    let ir_version = u64::try_from(model.ir_version)
        .ok()
        .filter(|version| *version > 0)
        .ok_or_else(|| runtime_error("invalidOnnxProtobuf"))?;
    let mut opsets = BTreeMap::new();
    for import in model.opset_import {
        if import.domain.len() > MAX_DOMAIN_BYTES || import.domain.as_bytes().contains(&0) {
            return Err(runtime_error("invalidOnnxProtobuf"));
        }
        let version = u64::try_from(import.version)
            .ok()
            .filter(|version| *version > 0)
            .ok_or_else(|| runtime_error("invalidOnnxProtobuf"))?;
        let domain = if import.domain.is_empty() || import.domain == "ai.onnx" {
            String::new()
        } else {
            import.domain
        };
        if opsets.insert(domain, version).is_some() {
            return Err(runtime_error("duplicateOpsetDomain"));
        }
    }
    if opsets.is_empty() {
        return Err(runtime_error("invalidOnnxProtobuf"));
    }
    let graph = model.graph.ok_or_else(|| runtime_error("invalidOnnxProtobuf"))?;
    let mut initializer_names = std::collections::BTreeSet::new();
    for initializer in graph.initializer {
        insert_initializer(&mut initializer_names, initializer.name)?;
    }
    for initializer in graph.sparse_initializer {
        let values = initializer.values.ok_or_else(|| runtime_error("invalidOnnxProtobuf"))?;
        insert_initializer(&mut initializer_names, values.name)?;
    }
    let mut inputs = Vec::new();
    let mut overridable_inputs = Vec::new();
    let mut input_names = std::collections::BTreeSet::new();
    for input in graph.input {
        let spec = parsed_tensor_spec(input)?;
        if !input_names.insert(spec.name.clone()) {
            return Err(runtime_error("invalidOnnxProtobuf"));
        }
        if initializer_names.contains(&spec.name) {
            if ir_version >= 4 {
                overridable_inputs.push(spec);
            }
        } else {
            inputs.push(spec);
        }
    }
    let mut outputs = Vec::new();
    let mut output_names = std::collections::BTreeSet::new();
    for output in graph.output {
        let spec = parsed_tensor_spec(output)?;
        if !output_names.insert(spec.name.clone()) {
            return Err(runtime_error("invalidOnnxProtobuf"));
        }
        outputs.push(spec);
    }
    if inputs.is_empty() || outputs.is_empty() {
        return Err(runtime_error("invalidOnnxProtobuf"));
    }
    Ok(ParsedModelProto { ir_version, opsets, inputs, overridable_inputs, outputs })
}

fn insert_initializer(
    names: &mut std::collections::BTreeSet<String>,
    name: String,
) -> Result<(), ConversionError> {
    if name.is_empty()
        || name.len() > MAX_TENSOR_NAME_BYTES
        || name.as_bytes().contains(&0)
        || !names.insert(name)
    {
        return Err(runtime_error("invalidOnnxProtobuf"));
    }
    Ok(())
}

fn parsed_tensor_spec(
    value: crate::onnx_proto::ValueInfoProto,
) -> Result<ParsedTensorSpec, ConversionError> {
    if value.name.is_empty()
        || value.name.len() > MAX_TENSOR_NAME_BYTES
        || value.name.as_bytes().contains(&0)
    {
        return Err(runtime_error("invalidOnnxProtobuf"));
    }
    let r#type = value.r#type.ok_or_else(|| runtime_error("invalidOnnxProtobuf"))?;
    let crate::onnx_proto::type_proto::Value::TensorType(tensor) =
        r#type.value.ok_or_else(|| runtime_error("invalidOnnxProtobuf"))?
    else {
        return Err(runtime_error("modelDtypeMismatch"));
    };
    if tensor.elem_type != 1 {
        return Err(runtime_error("modelDtypeMismatch"));
    }
    let shape = tensor.shape.ok_or_else(|| runtime_error("invalidOnnxProtobuf"))?;
    if shape.dim.is_empty() || shape.dim.len() > MAX_TENSOR_RANK {
        return Err(runtime_error("invalidOnnxProtobuf"));
    }
    let mut dimensions = Vec::new();
    dimensions.try_reserve_exact(shape.dim.len()).map_err(|_| resource_error("tensorMemory"))?;
    for dimension in shape.dim {
        let parsed = match dimension.value {
            Some(crate::onnx_proto::tensor_shape_proto::dimension::Value::DimValue(value)) => {
                ParsedDimension::Exact(
                    usize::try_from(value)
                        .ok()
                        .filter(|value| *value > 0)
                        .ok_or_else(|| runtime_error("invalidOnnxProtobuf"))?,
                )
            }
            Some(crate::onnx_proto::tensor_shape_proto::dimension::Value::DimParam(value))
                if !value.is_empty()
                    && value.len() <= MAX_TENSOR_NAME_BYTES
                    && !value.as_bytes().contains(&0) =>
            {
                ParsedDimension::Symbolic(value)
            }
            _ => return Err(runtime_error("invalidOnnxProtobuf")),
        };
        dimensions.push(parsed);
    }
    Ok(ParsedTensorSpec { name: value.name, dimensions })
}

fn validate_graph_specs(
    parsed: &[ParsedTensorSpec],
    expected: &[TensorSpec],
) -> Result<(), ConversionError> {
    if parsed.len() != expected.len() {
        return Err(runtime_error("modelIoMismatch"));
    }
    for (parsed, expected) in parsed.iter().zip(expected) {
        if parsed.name != expected.name
            || expected.element_type != TensorElementType::Float32
            || parsed.dimensions.len() != expected.dimensions.len()
        {
            return Err(runtime_error("modelIoMismatch"));
        }
        for (parsed, expected) in parsed.dimensions.iter().zip(&expected.dimensions) {
            let compatible = matches!((parsed, expected),
                (ParsedDimension::Exact(left), Dimension::Exact(right)) if left == right)
                || matches!((parsed, expected),
                    (ParsedDimension::Symbolic(_), Dimension::Dynamic { min, max })
                        if *min > 0 && min <= max);
            if !compatible {
                return Err(runtime_error("modelShapeMismatch"));
            }
        }
    }
    Ok(())
}

fn preflight_model(bytes: &[u8], budget: &mut usize, depth: usize) -> Result<(), ConversionError> {
    checked_depth(depth)?;
    let mut cursor = 0;
    let mut ir = false;
    let mut graph = false;
    let mut opsets = 0;
    while cursor < bytes.len() {
        let (field, wire) = read_key(bytes, &mut cursor, budget)?;
        match (field, wire) {
            (1, 0) => {
                if ir {
                    return Err(runtime_error("invalidOnnxProtobuf"));
                }
                ir = true;
                read_varint(bytes, &mut cursor)?;
            }
            (7, 2) => {
                if graph {
                    return Err(runtime_error("invalidOnnxProtobuf"));
                }
                graph = true;
                preflight_graph(read_length_delimited(bytes, &mut cursor)?, budget, depth + 1)?;
            }
            (8, 2) => {
                opsets += 1;
                if opsets > MAX_OPSET_IMPORTS {
                    return Err(runtime_error("invalidOnnxProtobuf"));
                }
                preflight_opset(read_length_delimited(bytes, &mut cursor)?, budget, depth + 1)?;
            }
            (2 | 3 | 4 | 6 | 14 | 20 | 25 | 26, 2) | (5, 0) => {
                skip_field(bytes, &mut cursor, wire)?;
            }
            _ => return Err(runtime_error("invalidOnnxProtobuf")),
        }
    }
    if !ir || !graph || opsets == 0 {
        return Err(runtime_error("invalidOnnxProtobuf"));
    }
    Ok(())
}

fn preflight_graph(bytes: &[u8], budget: &mut usize, depth: usize) -> Result<(), ConversionError> {
    checked_depth(depth)?;
    let mut cursor = 0;
    let mut inputs = 0;
    let mut outputs = 0;
    let mut initializers = 0;
    while cursor < bytes.len() {
        let (field, wire) = read_key(bytes, &mut cursor, budget)?;
        match (field, wire) {
            (5, 2) => {
                initializers += 1;
                if initializers > MAX_GRAPH_INITIALIZERS {
                    return Err(runtime_error("invalidOnnxProtobuf"));
                }
                preflight_tensor(read_length_delimited(bytes, &mut cursor)?, budget, depth + 1)?;
            }
            (15, 2) => {
                initializers += 1;
                if initializers > MAX_GRAPH_INITIALIZERS {
                    return Err(runtime_error("invalidOnnxProtobuf"));
                }
                preflight_sparse_tensor(
                    read_length_delimited(bytes, &mut cursor)?,
                    budget,
                    depth + 1,
                )?;
            }
            (11, 2) => {
                inputs += 1;
                if inputs > MAX_TENSORS * 2 {
                    return Err(runtime_error("invalidOnnxProtobuf"));
                }
                preflight_value_info(
                    read_length_delimited(bytes, &mut cursor)?,
                    budget,
                    depth + 1,
                )?;
            }
            (12, 2) => {
                outputs += 1;
                if outputs > MAX_TENSORS {
                    return Err(runtime_error("invalidOnnxProtobuf"));
                }
                preflight_value_info(
                    read_length_delimited(bytes, &mut cursor)?,
                    budget,
                    depth + 1,
                )?;
            }
            (1 | 2 | 10 | 13 | 14 | 16, 2) => skip_field(bytes, &mut cursor, wire)?,
            _ => return Err(runtime_error("invalidOnnxProtobuf")),
        }
    }
    if inputs == 0 || outputs == 0 {
        return Err(runtime_error("invalidOnnxProtobuf"));
    }
    Ok(())
}

fn preflight_opset(bytes: &[u8], budget: &mut usize, depth: usize) -> Result<(), ConversionError> {
    checked_depth(depth)?;
    let mut cursor = 0;
    let mut domain = false;
    let mut version = false;
    while cursor < bytes.len() {
        let (field, wire) = read_key(bytes, &mut cursor, budget)?;
        match (field, wire) {
            (1, 2) if !domain => {
                domain = true;
                if read_length_delimited(bytes, &mut cursor)?.len() > MAX_DOMAIN_BYTES {
                    return Err(runtime_error("invalidOnnxProtobuf"));
                }
            }
            (2, 0) if !version => {
                version = true;
                read_varint(bytes, &mut cursor)?;
            }
            _ => return Err(runtime_error("invalidOnnxProtobuf")),
        }
    }
    if !version {
        return Err(runtime_error("invalidOnnxProtobuf"));
    }
    Ok(())
}

fn preflight_value_info(
    bytes: &[u8],
    budget: &mut usize,
    depth: usize,
) -> Result<(), ConversionError> {
    checked_depth(depth)?;
    let mut cursor = 0;
    let mut name = false;
    let mut r#type = false;
    while cursor < bytes.len() {
        let (field, wire) = read_key(bytes, &mut cursor, budget)?;
        match (field, wire) {
            (1, 2) if !name => {
                name = true;
                if read_length_delimited(bytes, &mut cursor)?.len() > MAX_TENSOR_NAME_BYTES {
                    return Err(runtime_error("invalidOnnxProtobuf"));
                }
            }
            (2, 2) if !r#type => {
                r#type = true;
                preflight_type(read_length_delimited(bytes, &mut cursor)?, budget, depth + 1)?;
            }
            (3 | 4, 2) => skip_field(bytes, &mut cursor, wire)?,
            _ => return Err(runtime_error("invalidOnnxProtobuf")),
        }
    }
    if !name || !r#type {
        return Err(runtime_error("invalidOnnxProtobuf"));
    }
    Ok(())
}

fn preflight_type(bytes: &[u8], budget: &mut usize, depth: usize) -> Result<(), ConversionError> {
    checked_depth(depth)?;
    let mut cursor = 0;
    let mut tensor = false;
    while cursor < bytes.len() {
        let (field, wire) = read_key(bytes, &mut cursor, budget)?;
        match (field, wire) {
            (1, 2) if !tensor => {
                tensor = true;
                preflight_tensor_type(
                    read_length_delimited(bytes, &mut cursor)?,
                    budget,
                    depth + 1,
                )?;
            }
            (6, 2) => skip_field(bytes, &mut cursor, wire)?,
            (4 | 5 | 8 | 9, 2) => return Err(runtime_error("modelDtypeMismatch")),
            _ => return Err(runtime_error("invalidOnnxProtobuf")),
        }
    }
    if !tensor {
        return Err(runtime_error("invalidOnnxProtobuf"));
    }
    Ok(())
}

fn preflight_tensor_type(
    bytes: &[u8],
    budget: &mut usize,
    depth: usize,
) -> Result<(), ConversionError> {
    checked_depth(depth)?;
    let mut cursor = 0;
    let mut element = false;
    let mut shape = false;
    while cursor < bytes.len() {
        let (field, wire) = read_key(bytes, &mut cursor, budget)?;
        match (field, wire) {
            (1, 0) if !element => {
                element = true;
                read_varint(bytes, &mut cursor)?;
            }
            (2, 2) if !shape => {
                shape = true;
                preflight_shape(read_length_delimited(bytes, &mut cursor)?, budget, depth + 1)?;
            }
            _ => return Err(runtime_error("invalidOnnxProtobuf")),
        }
    }
    if !element || !shape {
        return Err(runtime_error("invalidOnnxProtobuf"));
    }
    Ok(())
}

fn preflight_shape(bytes: &[u8], budget: &mut usize, depth: usize) -> Result<(), ConversionError> {
    checked_depth(depth)?;
    let mut cursor = 0;
    let mut rank = 0;
    while cursor < bytes.len() {
        let (field, wire) = read_key(bytes, &mut cursor, budget)?;
        if (field, wire) != (1, 2) {
            return Err(runtime_error("invalidOnnxProtobuf"));
        }
        rank += 1;
        if rank > MAX_TENSOR_RANK {
            return Err(runtime_error("invalidOnnxProtobuf"));
        }
        preflight_dimension(read_length_delimited(bytes, &mut cursor)?, budget, depth + 1)?;
    }
    if rank == 0 {
        return Err(runtime_error("invalidOnnxProtobuf"));
    }
    Ok(())
}

fn preflight_dimension(
    bytes: &[u8],
    budget: &mut usize,
    depth: usize,
) -> Result<(), ConversionError> {
    checked_depth(depth)?;
    let mut cursor = 0;
    let mut value = false;
    while cursor < bytes.len() {
        let (field, wire) = read_key(bytes, &mut cursor, budget)?;
        match (field, wire) {
            (1, 0) if !value => {
                value = true;
                read_varint(bytes, &mut cursor)?;
            }
            (2, 2) if !value => {
                value = true;
                if read_length_delimited(bytes, &mut cursor)?.len() > MAX_TENSOR_NAME_BYTES {
                    return Err(runtime_error("invalidOnnxProtobuf"));
                }
            }
            (3, 2) => skip_field(bytes, &mut cursor, wire)?,
            _ => return Err(runtime_error("invalidOnnxProtobuf")),
        }
    }
    if !value {
        return Err(runtime_error("invalidOnnxProtobuf"));
    }
    Ok(())
}

fn preflight_tensor(bytes: &[u8], budget: &mut usize, depth: usize) -> Result<(), ConversionError> {
    checked_depth(depth)?;
    let mut cursor = 0;
    let mut name = false;
    while cursor < bytes.len() {
        let (field, wire) = read_key(bytes, &mut cursor, budget)?;
        match (field, wire) {
            (8, 2) if !name => {
                name = true;
                if read_length_delimited(bytes, &mut cursor)?.len() > MAX_TENSOR_NAME_BYTES {
                    return Err(runtime_error("invalidOnnxProtobuf"));
                }
            }
            (1, 0 | 2)
            | (2 | 5 | 7 | 11 | 14, 0)
            | (3 | 4 | 5 | 6 | 7 | 9 | 10 | 11 | 12 | 13 | 16, 2)
            | (4 | 10, 1 | 5) => {
                skip_field(bytes, &mut cursor, wire)?;
            }
            _ => return Err(runtime_error("invalidOnnxProtobuf")),
        }
    }
    if !name {
        return Err(runtime_error("invalidOnnxProtobuf"));
    }
    Ok(())
}

fn preflight_sparse_tensor(
    bytes: &[u8],
    budget: &mut usize,
    depth: usize,
) -> Result<(), ConversionError> {
    checked_depth(depth)?;
    let mut cursor = 0;
    let mut values = false;
    while cursor < bytes.len() {
        let (field, wire) = read_key(bytes, &mut cursor, budget)?;
        match (field, wire) {
            (1, 2) if !values => {
                values = true;
                preflight_tensor(read_length_delimited(bytes, &mut cursor)?, budget, depth + 1)?;
            }
            (2 | 3, 2) | (3, 0) => skip_field(bytes, &mut cursor, wire)?,
            _ => return Err(runtime_error("invalidOnnxProtobuf")),
        }
    }
    if !values {
        return Err(runtime_error("invalidOnnxProtobuf"));
    }
    Ok(())
}

fn read_key(
    bytes: &[u8],
    cursor: &mut usize,
    budget: &mut usize,
) -> Result<(u64, u8), ConversionError> {
    *budget = budget.checked_sub(1).ok_or_else(|| runtime_error("invalidOnnxProtobuf"))?;
    let key = read_varint(bytes, cursor)?;
    let field = key >> 3;
    let wire = u8::try_from(key & 7).map_err(|_| runtime_error("invalidOnnxProtobuf"))?;
    if field == 0 {
        return Err(runtime_error("invalidOnnxProtobuf"));
    }
    Ok((field, wire))
}

fn checked_depth(depth: usize) -> Result<(), ConversionError> {
    if depth > MAX_PROTO_DEPTH {
        return Err(runtime_error("invalidOnnxProtobuf"));
    }
    Ok(())
}

fn read_varint(bytes: &[u8], cursor: &mut usize) -> Result<u64, ConversionError> {
    let mut value = 0_u64;
    for shift in (0..70).step_by(7) {
        let byte = *bytes.get(*cursor).ok_or_else(|| runtime_error("invalidOnnxProtobuf"))?;
        *cursor += 1;
        if shift == 63 && byte > 1 {
            return Err(runtime_error("invalidOnnxProtobuf"));
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(runtime_error("invalidOnnxProtobuf"))
}

fn read_length_delimited<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
) -> Result<&'a [u8], ConversionError> {
    let length = usize::try_from(read_varint(bytes, cursor)?)
        .map_err(|_| runtime_error("invalidOnnxProtobuf"))?;
    let end = cursor.checked_add(length).ok_or_else(|| runtime_error("invalidOnnxProtobuf"))?;
    let value = bytes.get(*cursor..end).ok_or_else(|| runtime_error("invalidOnnxProtobuf"))?;
    *cursor = end;
    Ok(value)
}

fn skip_field(bytes: &[u8], cursor: &mut usize, wire: u8) -> Result<(), ConversionError> {
    match wire {
        0 => {
            read_varint(bytes, cursor)?;
        }
        1 => {
            *cursor = cursor
                .checked_add(8)
                .filter(|end| *end <= bytes.len())
                .ok_or_else(|| runtime_error("invalidOnnxProtobuf"))?;
        }
        2 => {
            read_length_delimited(bytes, cursor)?;
        }
        5 => {
            *cursor = cursor
                .checked_add(4)
                .filter(|end| *end <= bytes.len())
                .ok_or_else(|| runtime_error("invalidOnnxProtobuf"))?;
        }
        _ => return Err(runtime_error("invalidOnnxProtobuf")),
    }
    Ok(())
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
    use crate::ManifestModelResolver;
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
            ir_version: 9,
            opsets: BTreeMap::from([(String::new(), 18)]),
            inputs: vec![spec("image", vec![Dimension::Exact(1), Dimension::Exact(2)])],
            overridable_inputs: Vec::new(),
            outputs: vec![spec("score", vec![Dimension::Exact(1)])],
            session_memory_bytes: 1024,
            run_memory_bytes: 8,
        }
    }

    fn encode_varint(mut value: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        loop {
            let mut byte = u8::try_from(value & 0x7f).unwrap();
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            bytes.push(byte);
            if value == 0 {
                return bytes;
            }
        }
    }

    fn field_bytes(field: u64, value: &[u8]) -> Vec<u8> {
        let mut bytes = encode_varint((field << 3) | 2);
        bytes.extend(encode_varint(u64::try_from(value.len()).unwrap()));
        bytes.extend(value);
        bytes
    }

    fn value_info(name: &str, dimensions: &[u64]) -> Vec<u8> {
        let mut shape = Vec::new();
        for dimension in dimensions {
            let mut encoded = vec![0x08];
            encoded.extend(encode_varint(*dimension));
            shape.extend(field_bytes(1, &encoded));
        }
        let mut tensor_type = vec![0x08, 0x01]; // FLOAT
        tensor_type.extend(field_bytes(2, &shape));
        let type_proto = field_bytes(1, &tensor_type);
        let mut info = field_bytes(1, name.as_bytes());
        info.extend(field_bytes(2, &type_proto));
        info
    }

    fn symbolic_value_info(name: &str, dimensions: usize) -> Vec<u8> {
        let mut shape = Vec::new();
        for index in 0..dimensions {
            let dimension = field_bytes(2, format!("d{index}").as_bytes());
            shape.extend(field_bytes(1, &dimension));
        }
        let mut tensor_type = vec![0x08, 0x01];
        tensor_type.extend(field_bytes(2, &shape));
        let type_proto = field_bytes(1, &tensor_type);
        let mut info = field_bytes(1, name.as_bytes());
        info.extend(field_bytes(2, &type_proto));
        info
    }

    fn tiny_identity_model() -> Arc<[u8]> {
        let mut node = field_bytes(1, b"image");
        node.extend(field_bytes(2, b"score"));
        node.extend(field_bytes(4, b"Identity"));
        let mut graph = field_bytes(1, &node);
        graph.extend(field_bytes(2, b"identity"));
        graph.extend(field_bytes(11, &value_info("image", &[1, 2])));
        graph.extend(field_bytes(12, &value_info("score", &[1])));
        let mut model = vec![0x08, 0x09];
        model.extend(field_bytes(2, b"into-markdown-test"));
        model.extend(field_bytes(7, &graph));
        model.extend(field_bytes(8, &[0x10, 0x12]));
        Arc::from(model)
    }

    fn model(name: &str) -> ResolvedModel {
        let bytes = tiny_identity_model();
        ResolvedModel {
            identity: ModelIdentity {
                canonical_path: PathBuf::from(format!("/trusted/{name}.onnx")),
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                bytes: u64::try_from(bytes.len()).unwrap(),
                file_identity: format!("device:inode:{name}"),
            },
            contract: contract(),
            bytes,
            memory_reservation: None,
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
        fn estimate_bytes(
            &self,
            model: &ResolvedModel,
            _options: &SessionOptions,
        ) -> Result<u64, ConversionError> {
            Ok(model.contract.session_memory_bytes)
        }

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
                    ir_version: model.contract.ir_version,
                    opsets: model.contract.opsets.clone(),
                    inputs: model.contract.inputs.clone(),
                    overridable_inputs: model.contract.overridable_inputs.clone(),
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
            ir_version: expected.ir_version,
            opsets: BTreeMap::from([(String::new(), 19)]),
            inputs: expected.inputs.clone(),
            overridable_inputs: expected.overridable_inputs.clone(),
            outputs: expected.outputs.clone(),
        };
        assert!(
            format!("{}", validate_metadata(&expected, &actual).unwrap_err())
                .contains("opsetMismatch")
        );
        actual.opsets = expected.opsets.clone();
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
    fn tensor_count_name_rank_and_storage_budgets_are_bounded() {
        let mut too_many = Vec::new();
        for index in 0..=MAX_TENSORS {
            too_many.push(spec(&format!("tensor-{index}"), vec![Dimension::Exact(1)]));
        }
        assert!(
            format!("{}", validate_specs(&too_many).unwrap_err()).contains("invalidTensorContract")
        );
        assert!(
            validate_specs(&[spec(
                &"n".repeat(MAX_TENSOR_NAME_BYTES + 1),
                vec![Dimension::Exact(1)],
            )])
            .is_err()
        );
        assert!(
            validate_specs(&[spec("rank", vec![Dimension::Exact(1); MAX_TENSOR_RANK + 1],)])
                .is_err()
        );

        let contract = contract();
        let inputs = [Tensor { shape: vec![1, 2], values: vec![1.0, 2.0] }];
        let peak = run_memory_peak(&inputs, &contract).unwrap();
        let value_only = contract.run_memory_bytes + 2 * 4 + 4;
        assert!(peak > value_only, "shape and adapter slots must be charged");
        let invalid_tensor = Tensor { shape: vec![1; MAX_TENSOR_RANK + 1], values: vec![1.0] };
        assert!(validate_tensors(&contract.inputs, &[invalid_tensor], "input").is_err());
    }

    #[test]
    fn onnx_protobuf_ir_and_opsets_are_parsed_with_bounds() {
        let model = tiny_identity_model();
        let parsed = parse_model_proto(&model).unwrap();
        assert_eq!(parsed.ir_version, 9);
        assert_eq!(parsed.opsets, BTreeMap::from([(String::new(), 18)]));
        assert_eq!(parsed.inputs[0].name, "image");
        assert_eq!(parsed.outputs[0].name, "score");

        // Unsupported top-level fields and truncation are rejected before decode.
        let mut with_unknown = model.to_vec();
        with_unknown.extend([0xF8, 0x01, 0x01]);
        assert!(
            format!("{}", parse_model_proto(&with_unknown).unwrap_err())
                .contains("invalidOnnxProtobuf")
        );
        assert!(
            format!("{}", parse_model_proto(&model[..model.len() - 1]).unwrap_err())
                .contains("invalidOnnxProtobuf")
        );

        // Empty and ai.onnx are the same canonical domain and cannot coexist.
        let mut duplicate_default = model.to_vec();
        duplicate_default.extend(field_bytes(
            8,
            &[0x0a, 0x07, b'a', b'i', b'.', b'o', b'n', b'n', b'x', 0x10, 0x12],
        ));
        assert!(
            format!("{}", parse_model_proto(&duplicate_default).unwrap_err())
                .contains("duplicateOpsetDomain")
        );
    }

    #[test]
    fn graph_io_rejects_missing_duplicate_long_rank_and_unsupported_types() {
        let model = tiny_identity_model();
        let mut missing_graph = vec![0x08, 0x09];
        missing_graph.extend(field_bytes(8, &[0x10, 0x12]));
        assert!(parse_model_proto(&missing_graph).is_err());

        let mut graph = field_bytes(11, &value_info("image", &[1, 2]));
        graph.extend(field_bytes(11, &value_info("image", &[1, 2])));
        graph.extend(field_bytes(12, &value_info("score", &[1])));
        let mut duplicate = vec![0x08, 0x09];
        duplicate.extend(field_bytes(7, &graph));
        duplicate.extend(field_bytes(8, &[0x10, 0x12]));
        assert!(parse_model_proto(&duplicate).is_err());

        let mut graph = field_bytes(11, &value_info(&"n".repeat(257), &[1]));
        graph.extend(field_bytes(12, &value_info("score", &[1])));
        let mut long = vec![0x08, 0x09];
        long.extend(field_bytes(7, &graph));
        long.extend(field_bytes(8, &[0x10, 0x12]));
        assert!(parse_model_proto(&long).is_err());

        let mut graph = field_bytes(11, &symbolic_value_info("image", MAX_TENSOR_RANK + 1));
        graph.extend(field_bytes(12, &value_info("score", &[1])));
        let mut rank = vec![0x08, 0x09];
        rank.extend(field_bytes(7, &graph));
        rank.extend(field_bytes(8, &[0x10, 0x12]));
        assert!(parse_model_proto(&rank).is_err());

        let mut unknown = model.to_vec();
        unknown.extend([0xa8, 0x06, 0x01]);
        assert!(parse_model_proto(&unknown).is_err());
    }

    #[test]
    fn graph_initializer_inputs_follow_ir_overridable_semantics() {
        let mut initializer = vec![0x08, 0x01, 0x10, 0x01];
        initializer.extend(field_bytes(8, b"weight"));
        initializer.extend(field_bytes(9, &1_f32.to_le_bytes()));
        let mut graph = field_bytes(5, &initializer);
        graph.extend(field_bytes(11, &value_info("image", &[1])));
        graph.extend(field_bytes(11, &value_info("weight", &[1])));
        graph.extend(field_bytes(12, &value_info("score", &[1])));
        let mut model = vec![0x08, 0x09];
        model.extend(field_bytes(7, &graph));
        model.extend(field_bytes(8, &[0x10, 0x12]));
        let parsed = parse_model_proto(&model).unwrap();
        assert_eq!(
            parsed.inputs.iter().map(|spec| spec.name.as_str()).collect::<Vec<_>>(),
            ["image"]
        );
        assert_eq!(
            parsed.overridable_inputs.iter().map(|spec| spec.name.as_str()).collect::<Vec<_>>(),
            ["weight"]
        );

        let duplicate_initializer = {
            let mut graph = field_bytes(5, &initializer);
            graph.extend(field_bytes(5, &initializer));
            graph.extend(field_bytes(11, &value_info("image", &[1])));
            graph.extend(field_bytes(12, &value_info("score", &[1])));
            let mut model = vec![0x08, 0x09];
            model.extend(field_bytes(7, &graph));
            model.extend(field_bytes(8, &[0x10, 0x12]));
            model
        };
        assert!(parse_model_proto(&duplicate_initializer).is_err());
    }

    #[test]
    fn model_proto_mismatch_and_contract_digest_are_stable() {
        let original = model("proto");
        let mut wrong_ir = original.clone();
        wrong_ir.contract.ir_version += 1;
        assert!(
            format!("{}", validate_resolved_model(&wrong_ir).unwrap_err())
                .contains("modelIrVersionMismatch")
        );
        let mut wrong_opset = original.clone();
        wrong_opset.contract.opsets.insert(String::new(), 17);
        assert!(
            format!("{}", validate_resolved_model(&wrong_opset).unwrap_err())
                .contains("opsetMismatch")
        );

        let original_digest = contract_sha256(&original.contract).unwrap();
        let mut other_contract = original.contract.clone();
        other_contract.outputs[0].name = "other".into();
        assert_ne!(original_digest, contract_sha256(&other_contract).unwrap());
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
            fn estimate_bytes(
                &self,
                model: &ResolvedModel,
                _options: &SessionOptions,
            ) -> Result<u64, ConversionError> {
                Ok(model.contract.session_memory_bytes)
            }

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
            contract_sha256: contract_sha256(&resolved.contract).unwrap(),
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
        let cache = SessionCache::new(CacheLimits { max_sessions: 1, max_bytes: 1024 }).unwrap();
        let factory = FakeFactory::new(0, 8);
        let options = SessionOptions { max_session_bytes: 1024, ..SessionOptions::default() };
        for name in ["a", "b", "a"] {
            let resolved = model(name);
            let key = CacheKey {
                model: resolved.identity.clone(),
                contract_sha256: contract_sha256(&resolved.contract).unwrap(),
                options: options.clone(),
                runtime_version: runtime_authority().unwrap().0,
            };
            cache.get_or_create(&key, &resolved, &options, &factory, &context()).unwrap();
        }
        assert_eq!(factory.calls.load(Ordering::SeqCst), 3);
        let state = lock(&cache.state);
        assert_eq!(state.entries.len(), 1);
        let live = lock(&cache.live.state);
        assert_eq!(live.sessions, 1);
        assert_eq!(live.bytes, 1024);
    }

    #[test]
    fn session_load_reserves_the_full_authority_bound() {
        struct UnderestimatingFactory;
        impl SessionFactory for UnderestimatingFactory {
            fn estimate_bytes(
                &self,
                _model: &ResolvedModel,
                _options: &SessionOptions,
            ) -> Result<u64, ConversionError> {
                Ok(512)
            }
            fn create(
                &self,
                _model: &ResolvedModel,
                _options: &SessionOptions,
                _context: &ExecutionContext,
            ) -> Result<Arc<dyn SessionAdapter>, ConversionError> {
                panic!("underestimated session must not be created")
            }
        }
        let resolved = model("full-bound");
        let cache = SessionCache::new(CacheLimits { max_sessions: 1, max_bytes: 1024 }).unwrap();
        let options = SessionOptions { max_session_bytes: 1024, ..SessionOptions::default() };
        let key = CacheKey {
            model: resolved.identity.clone(),
            contract_sha256: contract_sha256(&resolved.contract).unwrap(),
            options: options.clone(),
            runtime_version: runtime_authority().unwrap().0,
        };
        let Err(error) =
            cache.get_or_create(&key, &resolved, &options, &UnderestimatingFactory, &context())
        else {
            panic!("an underestimated authority bound must fail closed");
        };
        assert_eq!(error.code().as_str(), "resourceLimit");
        assert_eq!(lock(&cache.live.state).sessions, 0);
    }

    #[test]
    fn evicted_session_remains_charged_until_last_arc_drops() {
        let cache = SessionCache::new(CacheLimits { max_sessions: 1, max_bytes: 1024 }).unwrap();
        let factory = FakeFactory::new(0, 8);
        let options = SessionOptions { max_session_bytes: 1024, ..SessionOptions::default() };
        let first_model = model("held-a");
        let first_key = CacheKey {
            model: first_model.identity.clone(),
            contract_sha256: contract_sha256(&first_model.contract).unwrap(),
            options: options.clone(),
            runtime_version: runtime_authority().unwrap().0,
        };
        let held =
            cache.get_or_create(&first_key, &first_model, &options, &factory, &context()).unwrap();
        let second_model = model("held-b");
        let second_key = CacheKey {
            model: second_model.identity.clone(),
            contract_sha256: contract_sha256(&second_model.contract).unwrap(),
            options: options.clone(),
            runtime_version: runtime_authority().unwrap().0,
        };
        let Err(blocked) =
            cache.get_or_create(&second_key, &second_model, &options, &factory, &context())
        else {
            panic!("a held live session must retain its budget");
        };
        assert_eq!(blocked.code().as_str(), "resourceLimit");
        assert_eq!(lock(&cache.live.state).sessions, 1);
        drop(held);
        cache.get_or_create(&second_key, &second_model, &options, &factory, &context()).unwrap();
        assert_eq!(lock(&cache.live.state).sessions, 1);
    }

    #[test]
    fn concurrent_different_key_loads_cannot_overcommit_live_budget() {
        struct BlockingFactory {
            gate: Arc<(Mutex<(bool, bool)>, Condvar)>,
        }
        impl SessionFactory for BlockingFactory {
            fn estimate_bytes(
                &self,
                model: &ResolvedModel,
                _options: &SessionOptions,
            ) -> Result<u64, ConversionError> {
                Ok(model.contract.session_memory_bytes)
            }
            fn create(
                &self,
                model: &ResolvedModel,
                _options: &SessionOptions,
                _context: &ExecutionContext,
            ) -> Result<Arc<dyn SessionAdapter>, ConversionError> {
                let (state, changed) = &*self.gate;
                let mut state = lock(state);
                state.0 = true;
                changed.notify_all();
                while !state.1 {
                    state = changed.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                Ok(Arc::new(FakeSession {
                    metadata: ModelMetadata {
                        ir_version: model.contract.ir_version,
                        opsets: model.contract.opsets.clone(),
                        inputs: model.contract.inputs.clone(),
                        overridable_inputs: model.contract.overridable_inputs.clone(),
                        outputs: model.contract.outputs.clone(),
                    },
                    bytes: 8,
                }))
            }
        }
        let cache =
            Arc::new(SessionCache::new(CacheLimits { max_sessions: 1, max_bytes: 1024 }).unwrap());
        let gate = Arc::new((Mutex::new((false, false)), Condvar::new()));
        let factory = Arc::new(BlockingFactory { gate: Arc::clone(&gate) });
        let options = SessionOptions { max_session_bytes: 1024, ..SessionOptions::default() };
        let first_model = model("loading-a");
        let first_key = CacheKey {
            model: first_model.identity.clone(),
            contract_sha256: contract_sha256(&first_model.contract).unwrap(),
            options: options.clone(),
            runtime_version: runtime_authority().unwrap().0,
        };
        let loader = {
            let cache = Arc::clone(&cache);
            let factory = Arc::clone(&factory);
            let options = options.clone();
            thread::spawn(move || {
                cache.get_or_create(
                    &first_key,
                    &first_model,
                    &options,
                    factory.as_ref(),
                    &context(),
                )
            })
        };
        let (state, changed) = &*gate;
        let mut state = lock(state);
        while !state.0 {
            state = changed.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        drop(state);

        let second_model = model("loading-b");
        let second_key = CacheKey {
            model: second_model.identity.clone(),
            contract_sha256: contract_sha256(&second_model.contract).unwrap(),
            options: options.clone(),
            runtime_version: runtime_authority().unwrap().0,
        };
        let Err(blocked) =
            cache.get_or_create(&second_key, &second_model, &options, factory.as_ref(), &context())
        else {
            panic!("a concurrent loading reservation must remain charged");
        };
        assert_eq!(blocked.code().as_str(), "resourceLimit");
        let mut state = lock(&gate.0);
        state.1 = true;
        gate.1.notify_all();
        drop(state);
        assert!(loader.join().unwrap().is_ok());
        assert_eq!(lock(&cache.live.state).sessions, 1);
    }

    #[test]
    fn live_budget_is_released_after_native_session_destructor() {
        struct DropCheckedSession {
            budget: Arc<LiveBudget>,
            saw_charge: Arc<std::sync::atomic::AtomicBool>,
        }
        impl SessionAdapter for DropCheckedSession {
            fn metadata(&self) -> Result<ModelMetadata, ConversionError> {
                unreachable!()
            }
            fn estimated_bytes(&self) -> u64 {
                8
            }
            fn run(
                &self,
                _inputs: &[Tensor],
                _context: &ExecutionContext,
            ) -> Result<Vec<Tensor>, ConversionError> {
                unreachable!()
            }
        }
        impl Drop for DropCheckedSession {
            fn drop(&mut self) {
                self.saw_charge.store(
                    lock(&self.budget.state).sessions == 1,
                    std::sync::atomic::Ordering::Release,
                );
            }
        }
        let budget = Arc::new(LiveBudget {
            state: Mutex::new(LiveBudgetState::default()),
            limits: CacheLimits { max_sessions: 1, max_bytes: 8 },
        });
        let lease = budget.try_reserve(8).unwrap();
        let saw_charge = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let native: Arc<dyn SessionAdapter> = Arc::new(DropCheckedSession {
            budget: Arc::clone(&budget),
            saw_charge: Arc::clone(&saw_charge),
        });
        let leased = lease.into_session(native);
        drop(leased);
        assert!(saw_charge.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(lock(&budget.state).sessions, 0);
        assert_eq!(lock(&budget.state).bytes, 0);
    }

    #[test]
    fn current_manifest_is_stably_unavailable() {
        let temporary = tempfile::tempdir().unwrap();
        let manager = Arc::new(
            super::super::ModelManager::embedded(temporary.path().to_path_buf(), None).unwrap(),
        );
        let error = ManifestModelResolver::new(manager)
            .resolve("pp-ocrv6-tiny-recognizer-onnx", &context())
            .unwrap_err();
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
