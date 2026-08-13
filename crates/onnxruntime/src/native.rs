//! Direct, bounded ONNX Runtime C API adapter used only inside the worker.

use super::{
    LoadError, audit_loaded_modules, authority, current_target, ensure_loader_environment_clean,
    load_verified_library, probe,
};
use into_markdown_core::Tensor;
use into_markdown_ocr::{
    Dimension, MAX_TENSOR_NAME_BYTES, MAX_TENSOR_RANK, MAX_TENSORS, ModelContract, ModelMetadata,
    SessionOptions, TensorElementType, TensorSpec,
};
use libloading::Library;
use ort::sys::{
    ExecutionMode, GraphOptimizationLevel, ONNXTensorElementDataType, ONNXType, OrtAllocator,
    OrtAllocatorType, OrtApi, OrtEnv, OrtLoggingLevel, OrtMemType, OrtMemoryInfo, OrtSession,
    OrtSessionOptions, OrtStatusPtr, OrtTensorTypeAndShapeInfo, OrtTypeInfo, OrtValue,
};
use std::ffi::CString;
use std::path::Path;
use std::ptr::{null, null_mut};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeError {
    Abi,
    Session,
    Metadata,
    Inference,
    Resource,
}

#[derive(Clone, Copy)]
enum IoKind {
    Input,
    Overridable,
    Output,
}

trait MetadataSource {
    fn count(&mut self, kind: IoKind) -> Result<usize, NativeError>;
    fn spec(&mut self, kind: IoKind, index: usize) -> Result<TensorSpec, NativeError>;
}

fn collect_metadata(
    source: &mut impl MetadataSource,
    kind: IoKind,
    allow_empty: bool,
    before_allocate: impl FnOnce(),
) -> Result<Vec<TensorSpec>, NativeError> {
    let count = source.count(kind)?;
    if count > MAX_TENSORS || (!allow_empty && count == 0) {
        return Err(NativeError::Metadata);
    }
    before_allocate();
    let mut specs = Vec::new();
    specs.try_reserve_exact(count).map_err(|_| NativeError::Resource)?;
    for index in 0..count {
        specs.push(source.spec(kind, index)?);
    }
    Ok(specs)
}

pub(crate) struct NativeSession {
    api: OrtApi,
    env: *mut OrtEnv,
    session: *mut OrtSession,
    memory_info: *mut OrtMemoryInfo,
    metadata: ModelMetadata,
    input_names: Vec<CString>,
    output_names: Vec<CString>,
    _library: Library,
}

impl NativeSession {
    pub(crate) fn new(
        runtime_path: &Path,
        expected_version: &str,
        expected_api: u32,
        model: &[u8],
        contract: &ModelContract,
        options: &SessionOptions,
    ) -> Result<Self, NativeError> {
        let target_name = current_target().ok_or(NativeError::Abi)?;
        let authority = authority().map_err(|_| NativeError::Abi)?;
        let target = authority.targets.get(target_name).ok_or(NativeError::Abi)?;
        ensure_loader_environment_clean().map_err(|_| NativeError::Abi)?;
        audit_loaded_modules(target, None).map_err(|_| NativeError::Abi)?;
        let library = load_verified_library(runtime_path).map_err(|_| NativeError::Abi)?;
        audit_loaded_modules(target, Some(runtime_path)).map_err(|_| NativeError::Abi)?;
        let (version, api) = probe(&library, expected_api).map_err(|_| NativeError::Abi)?;
        if version != expected_version {
            return Err(NativeError::Abi);
        }

        let mut env = null_mut();
        // SAFETY: every pointer passed here is either static NUL-terminated
        // storage or an out pointer to correctly aligned live storage.
        unsafe {
            status(
                &api,
                (api.CreateEnv)(
                    OrtLoggingLevel::ORT_LOGGING_LEVEL_WARNING,
                    c"into-markdown-worker".as_ptr(),
                    &raw mut env,
                ),
                NativeError::Session,
            )?;
        }
        if env.is_null() {
            return Err(NativeError::Session);
        }
        // SAFETY: `env` is live and owned by this constructor until transferred
        // to the returned session or released on an error path.
        if unsafe { status(&api, (api.DisableTelemetryEvents)(env), NativeError::Session) }.is_err()
        {
            // SAFETY: `env` is non-null and has not yet been released.
            unsafe { (api.ReleaseEnv)(env) };
            return Err(NativeError::Session);
        }

        let result = create_session(&api, env, model, contract, options);
        let (session, memory_info, metadata, input_names, output_names) = match result {
            Ok(value) => value,
            Err(error) => {
                // SAFETY: `env` remains live and owned on constructor failure.
                unsafe { (api.ReleaseEnv)(env) };
                return Err(error);
            }
        };
        Ok(Self {
            api,
            env,
            session,
            memory_info,
            metadata,
            input_names,
            output_names,
            _library: library,
        })
    }

    pub(crate) fn metadata(&self) -> &ModelMetadata {
        &self.metadata
    }

    pub(crate) fn run(&mut self, inputs: &mut [Tensor]) -> Result<Vec<Tensor>, NativeError> {
        if inputs.len() != self.metadata.inputs.len() || inputs.len() > MAX_TENSORS {
            return Err(NativeError::Inference);
        }
        let mut input_values = [null_mut::<OrtValue>(); MAX_TENSORS];
        let mut input_name_pointers = [null::<core::ffi::c_char>(); MAX_TENSORS];
        let mut shape = [0_i64; MAX_TENSOR_RANK];
        for (index, ((tensor, spec), name)) in
            inputs.iter_mut().zip(&self.metadata.inputs).zip(&self.input_names).enumerate()
        {
            let elements = validate_tensor_shape(&tensor.shape, spec)?;
            if elements != tensor.values.len() {
                release_values(&self.api, &mut input_values);
                return Err(NativeError::Inference);
            }
            for (target, value) in shape.iter_mut().zip(&tensor.shape) {
                *target = i64::try_from(*value).map_err(|_| NativeError::Inference)?;
            }
            // SAFETY: tensor backing, shape and memory info remain live through
            // `Run`; lengths are checked against the contract before this call.
            let result = unsafe {
                status(
                    &self.api,
                    (self.api.CreateTensorWithDataAsOrtValue)(
                        self.memory_info,
                        tensor.values.as_mut_ptr().cast(),
                        tensor
                            .values
                            .len()
                            .checked_mul(size_of::<f32>())
                            .ok_or(NativeError::Resource)?,
                        shape.as_ptr(),
                        tensor.shape.len(),
                        ONNXTensorElementDataType::ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT,
                        &raw mut input_values[index],
                    ),
                    NativeError::Inference,
                )
            };
            if let Err(error) = result {
                release_values(&self.api, &mut input_values);
                return Err(error);
            }
            input_name_pointers[index] = name.as_ptr();
        }

        let mut output_values = [null_mut::<OrtValue>(); MAX_TENSORS];
        let mut output_name_pointers = [null::<core::ffi::c_char>(); MAX_TENSORS];
        for (index, name) in self.output_names.iter().enumerate() {
            output_name_pointers[index] = name.as_ptr();
        }
        // SAFETY: all pointer arrays contain exactly the validated live entries
        // described by their lengths. Output slots are fixed-capacity nulls.
        let run = unsafe {
            status(
                &self.api,
                (self.api.Run)(
                    self.session,
                    null(),
                    input_name_pointers.as_ptr(),
                    input_values.as_ptr().cast(),
                    inputs.len(),
                    output_name_pointers.as_ptr(),
                    self.output_names.len(),
                    output_values.as_mut_ptr(),
                ),
                NativeError::Inference,
            )
        };
        release_values(&self.api, &mut input_values);
        if let Err(error) = run {
            // ORT may populate a prefix of the output array before returning a
            // failure. Release every non-null slot while still in the worker.
            release_values(&self.api, &mut output_values);
            return Err(error);
        }

        let mut outputs = Vec::new();
        outputs
            .try_reserve_exact(self.metadata.outputs.len())
            .map_err(|_| NativeError::Resource)?;
        for (value, expected) in output_values.iter_mut().zip(&self.metadata.outputs) {
            let output = copy_output(&self.api, *value, expected);
            if let Err(error) = output {
                release_values(&self.api, &mut output_values);
                return Err(error);
            }
            outputs.push(output?);
            // SAFETY: this exact output is released once after its checked copy.
            unsafe { (self.api.ReleaseValue)(*value) };
            *value = null_mut();
        }
        Ok(outputs)
    }
}

impl Drop for NativeSession {
    fn drop(&mut self) {
        // SAFETY: each pointer is either null or uniquely owned by this object;
        // the library field is deliberately retained until this Drop completes.
        unsafe {
            if !self.session.is_null() {
                (self.api.ReleaseSession)(self.session);
            }
            if !self.memory_info.is_null() {
                (self.api.ReleaseMemoryInfo)(self.memory_info);
            }
            if !self.env.is_null() {
                (self.api.ReleaseEnv)(self.env);
            }
        }
    }
}

type SessionParts =
    (*mut OrtSession, *mut OrtMemoryInfo, ModelMetadata, Vec<CString>, Vec<CString>);

fn create_session(
    api: &OrtApi,
    env: *mut OrtEnv,
    model: &[u8],
    contract: &ModelContract,
    options: &SessionOptions,
) -> Result<SessionParts, NativeError> {
    let mut session_options = null_mut::<OrtSessionOptions>();
    // SAFETY: out pointer is valid and status is always consumed.
    unsafe {
        status(api, (api.CreateSessionOptions)(&raw mut session_options), NativeError::Session)?;
    }
    if session_options.is_null() {
        return Err(NativeError::Session);
    }
    let configured = configure_options(api, session_options, options);
    if let Err(error) = configured {
        // SAFETY: options is uniquely owned and non-null.
        unsafe { (api.ReleaseSessionOptions)(session_options) };
        return Err(error);
    }
    let mut session = null_mut();
    // SAFETY: env/options/model bytes are all live through the call; output is
    // checked before use.
    let created = unsafe {
        status(
            api,
            (api.CreateSessionFromArray)(
                env,
                model.as_ptr().cast(),
                model.len(),
                session_options,
                &raw mut session,
            ),
            NativeError::Session,
        )
    };
    // SAFETY: CreateSessionFromArray does not retain the options object.
    unsafe { (api.ReleaseSessionOptions)(session_options) };
    created?;
    if session.is_null() {
        return Err(NativeError::Session);
    }

    let result = (|| {
        let mut source = NativeMetadataSource { api, session };
        let inputs = collect_metadata(&mut source, IoKind::Input, false, || {})?;
        let overridable_inputs = collect_metadata(&mut source, IoKind::Overridable, true, || {})?;
        let outputs = collect_metadata(&mut source, IoKind::Output, false, || {})?;
        let metadata = ModelMetadata {
            ir_version: contract.ir_version,
            opsets: contract.opsets.clone(),
            inputs,
            overridable_inputs,
            outputs,
        };
        let input_names = c_names(&metadata.inputs)?;
        let output_names = c_names(&metadata.outputs)?;
        let mut memory_info = null_mut();
        // SAFETY: out pointer is valid and status is consumed.
        unsafe {
            status(
                api,
                (api.CreateCpuMemoryInfo)(
                    OrtAllocatorType::OrtDeviceAllocator,
                    OrtMemType::OrtMemTypeDefault,
                    &raw mut memory_info,
                ),
                NativeError::Session,
            )?;
        }
        if memory_info.is_null() {
            return Err(NativeError::Session);
        }
        Ok((memory_info, metadata, input_names, output_names))
    })();
    match result {
        Ok((memory_info, metadata, input_names, output_names)) => {
            Ok((session, memory_info, metadata, input_names, output_names))
        }
        Err(error) => {
            // SAFETY: session remains uniquely owned on construction failure.
            unsafe { (api.ReleaseSession)(session) };
            Err(error)
        }
    }
}

fn configure_options(
    api: &OrtApi,
    options: *mut OrtSessionOptions,
    policy: &SessionOptions,
) -> Result<(), NativeError> {
    if policy.intra_op_threads == 0 || policy.inter_op_threads == 0 {
        return Err(NativeError::Session);
    }
    // SAFETY: all calls mutate the same uniquely owned live options object and
    // each returned status is consumed before the next call.
    unsafe {
        status(
            api,
            (api.SetSessionExecutionMode)(options, ExecutionMode::ORT_SEQUENTIAL),
            NativeError::Session,
        )?;
        status(api, (api.DisableMemPattern)(options), NativeError::Session)?;
        let arena = if policy.cpu_arena { api.EnableCpuMemArena } else { api.DisableCpuMemArena };
        status(api, arena(options), NativeError::Session)?;
        status(
            api,
            (api.SetSessionGraphOptimizationLevel)(
                options,
                GraphOptimizationLevel::ORT_ENABLE_BASIC,
            ),
            NativeError::Session,
        )?;
        status(
            api,
            (api.SetIntraOpNumThreads)(options, i32::from(policy.intra_op_threads)),
            NativeError::Session,
        )?;
        status(
            api,
            (api.SetInterOpNumThreads)(options, i32::from(policy.inter_op_threads)),
            NativeError::Session,
        )?;
    }
    Ok(())
}

fn c_names(specs: &[TensorSpec]) -> Result<Vec<CString>, NativeError> {
    let mut names = Vec::new();
    names.try_reserve_exact(specs.len()).map_err(|_| NativeError::Resource)?;
    for spec in specs {
        names.push(CString::new(spec.name.as_bytes()).map_err(|_| NativeError::Metadata)?);
    }
    Ok(names)
}

struct NativeMetadataSource<'a> {
    api: &'a OrtApi,
    session: *mut OrtSession,
}

impl MetadataSource for NativeMetadataSource<'_> {
    fn count(&mut self, kind: IoKind) -> Result<usize, NativeError> {
        let mut count = 0;
        let function = match kind {
            IoKind::Input => self.api.SessionGetInputCount,
            IoKind::Overridable => self.api.SessionGetOverridableInitializerCount,
            IoKind::Output => self.api.SessionGetOutputCount,
        };
        // SAFETY: session is live and `count` is a correctly aligned scalar out
        // pointer. No Rust allocation occurs before this bound is checked.
        unsafe {
            status(self.api, function(self.session, &raw mut count), NativeError::Metadata)?;
        }
        Ok(count)
    }

    fn spec(&mut self, kind: IoKind, index: usize) -> Result<TensorSpec, NativeError> {
        let name = read_name(self.api, self.session, kind, index)?;
        let dimensions = read_type(self.api, self.session, kind, index)?;
        Ok(TensorSpec { name, element_type: TensorElementType::Float32, dimensions })
    }
}

#[repr(C)]
struct FixedNameAllocator {
    allocator: OrtAllocator,
    buffer: [u8; MAX_TENSOR_NAME_BYTES + 1],
    requested: usize,
    allocated: bool,
    rejected: bool,
}

impl FixedNameAllocator {
    fn new() -> Self {
        Self {
            allocator: OrtAllocator {
                version: ort::sys::ORT_API_VERSION,
                Alloc: Some(fixed_alloc),
                Free: Some(fixed_free),
                Info: Some(fixed_info),
                Reserve: Some(fixed_reserve),
            },
            buffer: [0; MAX_TENSOR_NAME_BYTES + 1],
            requested: 0,
            allocated: false,
            rejected: false,
        }
    }

    fn finish(&self, pointer: *mut core::ffi::c_char) -> Result<String, NativeError> {
        if self.rejected
            || !self.allocated
            || pointer.cast::<u8>() != self.buffer.as_ptr().cast_mut()
            || self.requested == 0
            || self.requested > self.buffer.len()
        {
            return Err(NativeError::Metadata);
        }
        let bytes = &self.buffer[..self.requested];
        let end = bytes.iter().position(|byte| *byte == 0).ok_or(NativeError::Metadata)?;
        if end == 0 || end > MAX_TENSOR_NAME_BYTES {
            return Err(NativeError::Metadata);
        }
        std::str::from_utf8(&bytes[..end]).map(str::to_owned).map_err(|_| NativeError::Metadata)
    }
}

unsafe extern "system" fn fixed_alloc(
    allocator: *mut OrtAllocator,
    size: usize,
) -> *mut core::ffi::c_void {
    // SAFETY: the API receives the address of the first field of a live
    // `FixedNameAllocator`; repr(C) preserves that address and layout.
    let state = unsafe { &mut *allocator.cast::<FixedNameAllocator>() };
    if size == 0 || size > state.buffer.len() || state.allocated {
        state.rejected = true;
        return null_mut();
    }
    state.requested = size;
    state.allocated = true;
    state.buffer.fill(0);
    state.buffer.as_mut_ptr().cast()
}

unsafe extern "system" fn fixed_free(
    allocator: *mut OrtAllocator,
    pointer: *mut core::ffi::c_void,
) {
    // SAFETY: same allocator provenance invariant as `fixed_alloc`.
    let state = unsafe { &mut *allocator.cast::<FixedNameAllocator>() };
    if pointer != state.buffer.as_mut_ptr().cast() {
        state.rejected = true;
    }
}

unsafe extern "system" fn fixed_info(_allocator: *const OrtAllocator) -> *const OrtMemoryInfo {
    null()
}

unsafe extern "system" fn fixed_reserve(
    allocator: *const OrtAllocator,
    size: usize,
) -> *mut core::ffi::c_void {
    // SAFETY: ORT's reserve callback has the same exclusive allocator
    // semantics as Alloc despite the historical const-qualified signature.
    unsafe { fixed_alloc(allocator.cast_mut(), size) }
}

fn read_name(
    api: &OrtApi,
    session: *mut OrtSession,
    kind: IoKind,
    index: usize,
) -> Result<String, NativeError> {
    let mut allocator = FixedNameAllocator::new();
    let mut pointer = null_mut();
    let function = match kind {
        IoKind::Input => api.SessionGetInputName,
        IoKind::Overridable => api.SessionGetOverridableInitializerName,
        IoKind::Output => api.SessionGetOutputName,
    };
    // SAFETY: session/index were bounded by the preceding scalar count; the
    // allocator exposes only one fixed 257-byte buffer and rejects larger asks.
    unsafe {
        status(
            api,
            function(session, index, &raw mut allocator.allocator, &raw mut pointer),
            NativeError::Metadata,
        )?;
    }
    allocator.finish(pointer)
}

fn read_type(
    api: &OrtApi,
    session: *mut OrtSession,
    kind: IoKind,
    index: usize,
) -> Result<Vec<Dimension>, NativeError> {
    let mut info = null_mut::<OrtTypeInfo>();
    let function = match kind {
        IoKind::Input => api.SessionGetInputTypeInfo,
        IoKind::Overridable => api.SessionGetOverridableInitializerTypeInfo,
        IoKind::Output => api.SessionGetOutputTypeInfo,
    };
    // SAFETY: index was checked against the matching scalar count.
    unsafe {
        status(api, function(session, index, &raw mut info), NativeError::Metadata)?;
    }
    if info.is_null() {
        return Err(NativeError::Metadata);
    }
    let result = read_type_inner(api, info);
    // SAFETY: this non-null TypeInfo is owned by the caller and released once.
    unsafe { (api.ReleaseTypeInfo)(info) };
    result
}

fn read_type_inner(api: &OrtApi, info: *mut OrtTypeInfo) -> Result<Vec<Dimension>, NativeError> {
    let mut onnx_type = ONNXType::ONNX_TYPE_UNKNOWN;
    // SAFETY: info is live for the duration of this helper and scalar out
    // pointers are valid.
    unsafe {
        status(
            api,
            (api.GetOnnxTypeFromTypeInfo)(info, &raw mut onnx_type),
            NativeError::Metadata,
        )?;
    }
    if onnx_type != ONNXType::ONNX_TYPE_TENSOR {
        return Err(NativeError::Metadata);
    }
    let mut tensor_info = null::<OrtTensorTypeAndShapeInfo>();
    // SAFETY: tensor_info is borrowed from live TypeInfo and not released
    // independently.
    unsafe {
        status(
            api,
            (api.CastTypeInfoToTensorInfo)(info, &raw mut tensor_info),
            NativeError::Metadata,
        )?;
    }
    if tensor_info.is_null() {
        return Err(NativeError::Metadata);
    }
    let mut element_type = ONNXTensorElementDataType::ONNX_TENSOR_ELEMENT_DATA_TYPE_UNDEFINED;
    let mut rank = 0_usize;
    // SAFETY: borrowed tensor info is live and both outputs are fixed scalars.
    unsafe {
        status(
            api,
            (api.GetTensorElementType)(tensor_info, &raw mut element_type),
            NativeError::Metadata,
        )?;
        status(api, (api.GetDimensionsCount)(tensor_info, &raw mut rank), NativeError::Metadata)?;
    }
    if element_type != ONNXTensorElementDataType::ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT
        || rank == 0
        || rank > MAX_TENSOR_RANK
    {
        return Err(NativeError::Metadata);
    }
    let mut raw = [0_i64; MAX_TENSOR_RANK];
    // SAFETY: rank was checked against the fixed stack capacity.
    unsafe {
        status(
            api,
            (api.GetDimensions)(tensor_info, raw.as_mut_ptr(), rank),
            NativeError::Metadata,
        )?;
    }
    let mut dimensions = Vec::new();
    dimensions.try_reserve_exact(rank).map_err(|_| NativeError::Resource)?;
    for dimension in &raw[..rank] {
        dimensions.push(if *dimension == -1 {
            Dimension::Dynamic { min: 1, max: usize::MAX }
        } else {
            Dimension::Exact(
                usize::try_from(*dimension)
                    .ok()
                    .filter(|value| *value > 0)
                    .ok_or(NativeError::Metadata)?,
            )
        });
    }
    Ok(dimensions)
}

fn copy_output(
    api: &OrtApi,
    value: *mut OrtValue,
    expected: &TensorSpec,
) -> Result<Tensor, NativeError> {
    if value.is_null() {
        return Err(NativeError::Inference);
    }
    let mut info = null_mut::<OrtTensorTypeAndShapeInfo>();
    // SAFETY: output value is live and uniquely owned until this function
    // returns; the info output pointer is valid.
    unsafe {
        status(api, (api.GetTensorTypeAndShape)(value, &raw mut info), NativeError::Inference)?;
    }
    if info.is_null() {
        return Err(NativeError::Inference);
    }
    let result = copy_output_inner(api, value, info, expected);
    // SAFETY: shape info is uniquely owned and released exactly once.
    unsafe { (api.ReleaseTensorTypeAndShapeInfo)(info) };
    result
}

fn copy_output_inner(
    api: &OrtApi,
    value: *mut OrtValue,
    info: *mut OrtTensorTypeAndShapeInfo,
    expected: &TensorSpec,
) -> Result<Tensor, NativeError> {
    let mut element_type = ONNXTensorElementDataType::ONNX_TENSOR_ELEMENT_DATA_TYPE_UNDEFINED;
    let mut rank = 0_usize;
    // SAFETY: only fixed scalar outputs are written before validation.
    unsafe {
        status(
            api,
            (api.GetTensorElementType)(info, &raw mut element_type),
            NativeError::Inference,
        )?;
        status(api, (api.GetDimensionsCount)(info, &raw mut rank), NativeError::Inference)?;
    }
    if element_type != ONNXTensorElementDataType::ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT
        || rank == 0
        || rank > MAX_TENSOR_RANK
        || rank != expected.dimensions.len()
    {
        return Err(NativeError::Inference);
    }
    let mut raw_shape = [0_i64; MAX_TENSOR_RANK];
    // SAFETY: rank is bounded by the fixed stack buffer.
    unsafe {
        status(
            api,
            (api.GetDimensions)(info, raw_shape.as_mut_ptr(), rank),
            NativeError::Inference,
        )?;
    }
    let mut elements = 1_usize;
    for (actual, bound) in raw_shape[..rank].iter().zip(&expected.dimensions) {
        let actual = usize::try_from(*actual).map_err(|_| NativeError::Inference)?;
        let valid = match bound {
            Dimension::Exact(expected) => actual == *expected,
            Dimension::Dynamic { min, max } => actual >= *min && actual <= *max,
        };
        if !valid {
            return Err(NativeError::Resource);
        }
        elements = elements.checked_mul(actual).ok_or(NativeError::Resource)?;
    }
    elements.checked_mul(size_of::<f32>()).ok_or(NativeError::Resource)?;
    let mut data = null_mut::<core::ffi::c_void>();
    // SAFETY: the native shape has already been checked against the exact
    // contract maximum before obtaining or forming a Rust slice.
    unsafe {
        status(api, (api.GetTensorMutableData)(value, &raw mut data), NativeError::Inference)?;
    }
    if data.is_null() {
        return Err(NativeError::Inference);
    }
    let mut shape = Vec::new();
    shape.try_reserve_exact(rank).map_err(|_| NativeError::Resource)?;
    for value in &raw_shape[..rank] {
        shape.push(usize::try_from(*value).map_err(|_| NativeError::Inference)?);
    }
    let mut values = Vec::new();
    values.try_reserve_exact(elements).map_err(|_| NativeError::Resource)?;
    // SAFETY: ORT reports a CPU float tensor whose checked shape contains
    // exactly `elements`; the owning OrtValue remains live through the copy.
    let native = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), elements) };
    values.extend_from_slice(native);
    Ok(Tensor { shape, values })
}

fn validate_tensor_shape(shape: &[usize], spec: &TensorSpec) -> Result<usize, NativeError> {
    if shape.len() != spec.dimensions.len() || shape.is_empty() || shape.len() > MAX_TENSOR_RANK {
        return Err(NativeError::Inference);
    }
    shape.iter().zip(&spec.dimensions).try_fold(1_usize, |elements, (actual, expected)| {
        let valid = match expected {
            Dimension::Exact(value) => actual == value,
            Dimension::Dynamic { min, max } => actual >= min && actual <= max,
        };
        if !valid {
            return Err(NativeError::Inference);
        }
        elements.checked_mul(*actual).ok_or(NativeError::Resource)
    })
}

unsafe fn status(
    api: &OrtApi,
    status: OrtStatusPtr,
    error: NativeError,
) -> Result<(), NativeError> {
    if status.0.is_null() {
        return Ok(());
    }
    let classified = if error == NativeError::Inference {
        // SAFETY: the message belongs to this live status and is inspected with
        // a fixed stack bound before the status is released.
        let message = unsafe { (api.GetErrorMessage)(status.0) };
        if unsafe { bounded_allocation_message(message) } { NativeError::Resource } else { error }
    } else {
        error
    };
    // SAFETY: every non-null status returned by this exact API table is owned
    // by the caller and consumed exactly once here.
    unsafe { (api.ReleaseStatus)(status.0) };
    Err(classified)
}

unsafe fn bounded_allocation_message(pointer: *const core::ffi::c_char) -> bool {
    if pointer.is_null() {
        return false;
    }
    let mut bytes = [0_u8; 512];
    let mut length = 0;
    while length < bytes.len() {
        // SAFETY: ORT documents the status message as NUL-terminated. The scan
        // is fixed at 512 bytes and never forms a slice beyond bytes read.
        let byte = unsafe { pointer.add(length).read() };
        if byte == 0 {
            break;
        }
        bytes[length] = u8::try_from(byte).unwrap_or_default().to_ascii_lowercase();
        length += 1;
    }
    if length == bytes.len() {
        return false;
    }
    let message = &bytes[..length];
    [b"alloc".as_slice(), b"memory".as_slice(), b"bad_alloc".as_slice()]
        .iter()
        .any(|needle| message.windows(needle.len()).any(|window| window == *needle))
}

fn release_values(api: &OrtApi, values: &mut [*mut OrtValue]) {
    for value in values {
        if !value.is_null() {
            // SAFETY: each non-null slot is uniquely owned and reset after its
            // one matching release.
            unsafe { (api.ReleaseValue)(*value) };
            *value = null_mut();
        }
    }
}

impl From<LoadError> for NativeError {
    fn from(_value: LoadError) -> Self {
        Self::Abi
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeMetadata {
        count: usize,
        spec_calls: AtomicUsize,
    }

    impl MetadataSource for FakeMetadata {
        fn count(&mut self, _kind: IoKind) -> Result<usize, NativeError> {
            Ok(self.count)
        }

        fn spec(&mut self, _kind: IoKind, _index: usize) -> Result<TensorSpec, NativeError> {
            self.spec_calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("oversized count must fail before item allocation")
        }
    }

    #[test]
    fn huge_native_count_fails_before_vec_or_item_allocation() {
        let mut fake = FakeMetadata { count: usize::MAX, spec_calls: AtomicUsize::new(0) };
        let allocations = AtomicUsize::new(0);
        assert!(
            collect_metadata(&mut fake, IoKind::Input, false, || {
                allocations.fetch_add(1, Ordering::SeqCst);
            })
            .is_err()
        );
        assert_eq!(allocations.load(Ordering::SeqCst), 0);
        assert_eq!(fake.spec_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn bounded_name_allocator_rejects_large_and_unterminated_strings() {
        let mut allocator = FixedNameAllocator::new();
        // SAFETY: test passes the exact embedded allocator address.
        let rejected =
            unsafe { fixed_alloc(&raw mut allocator.allocator, MAX_TENSOR_NAME_BYTES + 2) };
        assert!(rejected.is_null());
        assert!(allocator.finish(null_mut()).is_err());

        let mut allocator = FixedNameAllocator::new();
        // SAFETY: same exact embedded allocator address.
        let pointer = unsafe { fixed_alloc(&raw mut allocator.allocator, 4) };
        assert!(!pointer.is_null());
        allocator.buffer[..4].copy_from_slice(b"name");
        assert!(allocator.finish(pointer.cast()).is_err());
    }

    #[test]
    fn allocation_status_messages_are_classified_without_unbounded_c_strings() {
        // SAFETY: both test buffers are live for their fixed bounded scans.
        assert!(unsafe { bounded_allocation_message(c"Failed to allocate memory".as_ptr()) });
        let unterminated = [core::ffi::c_char::try_from(b'a').unwrap(); 512];
        // SAFETY: the scanner reads exactly the complete fixed test buffer.
        assert!(!unsafe { bounded_allocation_message(unterminated.as_ptr()) });
    }
}
